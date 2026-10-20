use serde::Deserialize;
use serde_json::Value;

const GUILD_VOICE: u8 = 2;
const GUILD_STAGE_VOICE: u8 = 13;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Guild {
    pub id: String,
    pub name: String,
    /// `false` while Discord reports the guild as unavailable (outage, or not yet sent after READY).
    pub available: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum VoiceChannelKind {
    Voice,
    Stage,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct VoiceChannel {
    pub id: String,
    pub guild_id: String,
    pub name: String,
    pub position: i64,
    pub parent_id: Option<String>,
    pub kind: VoiceChannelKind,
}

#[derive(Deserialize)]
struct RawGuild {
    id: String,
    #[serde(default)]
    name: Option<String>,
    #[serde(default)]
    unavailable: Option<bool>,
    #[serde(default)]
    channels: Option<Vec<RawChannel>>,
    #[serde(default)]
    properties: Option<RawGuildProperties>,
}

#[derive(Deserialize)]
struct RawGuildProperties {
    #[serde(default)]
    name: Option<String>,
}

#[derive(Deserialize)]
struct RawChannel {
    id: String,
    #[serde(rename = "type")]
    kind: u8,
    #[serde(default)]
    guild_id: Option<String>,
    #[serde(default)]
    name: Option<String>,
    #[serde(default)]
    position: Option<i64>,
    #[serde(default)]
    parent_id: Option<String>,
}

#[derive(Deserialize)]
struct RawReady {
    #[serde(default)]
    guilds: Vec<RawGuild>,
}

struct Entry {
    guild: Guild,
    channels: Vec<VoiceChannel>,
}

/// Guilds in the order Discord sent them, each with its voice and stage channels.
#[derive(Default)]
pub(crate) struct Cache {
    entries: Vec<Entry>,
}

impl Cache {
    pub(crate) fn guilds(&self) -> Vec<Guild> {
        self.entries.iter().map(|e| e.guild.clone()).collect()
    }

    pub(crate) fn voice_channels(&self, guild_id: &str) -> Vec<VoiceChannel> {
        let mut channels = self
            .entry(guild_id)
            .map(|e| e.channels.clone())
            .unwrap_or_default();
        channels
            .sort_by(|a, b| (a.position, a.id.len(), &a.id).cmp(&(b.position, b.id.len(), &b.id)));
        channels
    }

    /// Applies a dispatch; returns whether the cache changed.
    pub(crate) fn apply(&mut self, name: &str, data: &Value) -> bool {
        match name {
            "READY" => {
                self.entries.clear();
                if let Ok(ready) = RawReady::deserialize(data) {
                    for raw in ready.guilds {
                        self.upsert(raw);
                    }
                }
                true
            }
            "GUILD_CREATE" | "GUILD_UPDATE" => match RawGuild::deserialize(data) {
                Ok(raw) => {
                    self.upsert(raw);
                    true
                }
                Err(_) => false,
            },
            "GUILD_DELETE" => match RawGuild::deserialize(data) {
                Ok(raw) if raw.unavailable == Some(true) => {
                    if let Some(e) = self.entry_mut(&raw.id) {
                        e.guild.available = false;
                    }
                    true
                }
                Ok(raw) => {
                    self.entries.retain(|e| e.guild.id != raw.id);
                    true
                }
                Err(_) => false,
            },
            "CHANNEL_CREATE" | "CHANNEL_UPDATE" | "CHANNEL_DELETE" => {
                let Ok(raw) = RawChannel::deserialize(data) else {
                    return false;
                };
                let Some(guild_id) = raw.guild_id.clone() else {
                    return false;
                };
                let Some(entry) = self.entry_mut(&guild_id) else {
                    return false;
                };
                let before = entry.channels.len();
                entry.channels.retain(|c| c.id != raw.id);
                let removed = entry.channels.len() != before;
                if name == "CHANNEL_DELETE" {
                    return removed;
                }
                match voice_channel(raw, &guild_id) {
                    Some(c) => {
                        entry.channels.push(c);
                        true
                    }
                    None => removed,
                }
            }
            _ => false,
        }
    }

    fn upsert(&mut self, raw: RawGuild) {
        let name = raw
            .name
            .or_else(|| raw.properties.and_then(|p| p.name))
            .or_else(|| self.entry(&raw.id).map(|e| e.guild.name.clone()))
            .unwrap_or_default();
        let available = raw.unavailable != Some(true);
        let channels = raw.channels.map(|list| {
            list.into_iter()
                .filter_map(|c| voice_channel(c, &raw.id))
                .collect::<Vec<_>>()
        });
        let guild = Guild {
            id: raw.id,
            name,
            available,
        };
        match self.entry_mut(&guild.id) {
            Some(e) => {
                e.guild = guild;
                if let Some(channels) = channels {
                    e.channels = channels;
                }
            }
            None => self.entries.push(Entry {
                guild,
                channels: channels.unwrap_or_default(),
            }),
        }
    }

    fn entry(&self, id: &str) -> Option<&Entry> {
        self.entries.iter().find(|e| e.guild.id == id)
    }

    fn entry_mut(&mut self, id: &str) -> Option<&mut Entry> {
        self.entries.iter_mut().find(|e| e.guild.id == id)
    }
}

fn voice_channel(raw: RawChannel, guild_id: &str) -> Option<VoiceChannel> {
    let kind = match raw.kind {
        GUILD_VOICE => VoiceChannelKind::Voice,
        GUILD_STAGE_VOICE => VoiceChannelKind::Stage,
        _ => return None,
    };
    Some(VoiceChannel {
        id: raw.id,
        guild_id: guild_id.to_owned(),
        name: raw.name.unwrap_or_default(),
        position: raw.position.unwrap_or_default(),
        parent_id: raw.parent_id,
        kind,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn ready() -> Value {
        json!({
            "guilds": [
                {"id": "1", "name": "One", "channels": [
                    {"id": "11", "type": 2, "name": "B", "position": 2},
                    {"id": "12", "type": 0, "name": "text", "position": 0},
                    {"id": "13", "type": 2, "name": "A", "position": 1, "parent_id": "10"},
                ]},
                {"id": "2", "unavailable": true},
            ]
        })
    }

    #[test]
    fn ready_keeps_voice_channels_sorted_by_position() {
        let mut cache = Cache::default();
        assert!(cache.apply("READY", &ready()));
        let names: Vec<_> = cache
            .voice_channels("1")
            .into_iter()
            .map(|c| c.name)
            .collect();
        assert_eq!(names, ["A", "B"]);
        assert!(!cache.guilds()[1].available);
    }

    #[test]
    fn channel_update_to_text_removes_it() {
        let mut cache = Cache::default();
        cache.apply("READY", &ready());
        assert!(cache.apply(
            "CHANNEL_UPDATE",
            &json!({"id": "11", "type": 0, "guild_id": "1", "name": "B"})
        ));
        assert_eq!(cache.voice_channels("1").len(), 1);
    }

    #[test]
    fn guild_update_without_channels_keeps_them() {
        let mut cache = Cache::default();
        cache.apply("READY", &ready());
        cache.apply("GUILD_UPDATE", &json!({"id": "1", "name": "Renamed"}));
        assert_eq!(cache.guilds()[0].name, "Renamed");
        assert_eq!(cache.voice_channels("1").len(), 2);
    }

    #[test]
    fn guild_delete_unavailable_keeps_guild() {
        let mut cache = Cache::default();
        cache.apply("READY", &ready());
        cache.apply("GUILD_DELETE", &json!({"id": "1", "unavailable": true}));
        assert!(!cache.guilds()[0].available);
        cache.apply("GUILD_DELETE", &json!({"id": "1"}));
        assert_eq!(cache.guilds().len(), 1);
    }
}
