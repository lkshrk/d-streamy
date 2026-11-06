use serde::Deserialize;
use serde_json::Value;

use crate::gateway::Dispatch;
use crate::transport::parse_stream_key;

#[derive(Deserialize)]
struct StreamPayload {
    stream_key: String,
    #[serde(default)]
    viewer_ids: Option<Vec<Value>>,
}

/// Who watches our Go Live, from STREAM_CREATE, STREAM_UPDATE and STREAM_DELETE.
#[derive(Debug, Default)]
pub(crate) struct Viewers {
    count: Option<u32>,
}

impl Viewers {
    /// Viewers other than the streaming account; `None` until Discord reports them.
    pub(crate) fn count(&self) -> Option<u32> {
        self.count
    }

    pub(crate) fn reset(&mut self) {
        self.count = None;
    }

    pub(crate) fn apply(
        &mut self,
        dispatch: &Dispatch,
        guild_id: Option<&str>,
        channel_id: &str,
        user_id: &str,
    ) {
        let name = dispatch.name.as_str();
        if !matches!(name, "STREAM_CREATE" | "STREAM_UPDATE" | "STREAM_DELETE") {
            return;
        }
        let Ok(payload) = StreamPayload::deserialize(&*dispatch.data) else {
            return;
        };
        let ours = parse_stream_key(&payload.stream_key).is_ok_and(|k| {
            k.guild_id.as_deref() == guild_id && k.channel_id == channel_id && k.user_id == user_id
        });
        if !ours {
            return;
        }
        let count = payload.viewer_ids.map(|ids| {
            let others = ids.iter().filter(|id| !is_user(id, user_id)).count();
            u32::try_from(others).unwrap_or(u32::MAX)
        });
        match name {
            // A new stream starts with no viewers, which says nothing yet.
            "STREAM_CREATE" => self.count = count.filter(|&n| n > 0),
            "STREAM_UPDATE" => self.count = count.or(self.count),
            _ => self.count = None,
        }
    }
}

fn is_user(id: &Value, user_id: &str) -> bool {
    match id {
        Value::String(s) => s == user_id,
        Value::Number(n) => n.to_string() == user_id,
        _ => false,
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use serde_json::json;

    use super::*;

    const GUILD: &str = "1";
    const CHANNEL: &str = "2";
    const ME: &str = "3";
    const KEY: &str = "guild:1:2:3";

    fn dispatch(name: &str, data: Value) -> Dispatch {
        Dispatch {
            name: name.to_owned(),
            seq: None,
            data: Arc::new(data),
        }
    }

    fn apply(v: &mut Viewers, name: &str, data: Value) -> Option<u32> {
        v.apply(&dispatch(name, data), Some(GUILD), CHANNEL, ME);
        v.count()
    }

    #[test]
    fn unknown_until_discord_reports_viewers() {
        let mut v = Viewers::default();
        assert_eq!(v.count(), None);
        let created =
            json!({"stream_key": KEY, "rtc_server_id": "9", "viewer_ids": [], "paused": false});
        assert_eq!(apply(&mut v, "STREAM_CREATE", created), None);
        let no_ids = json!({"stream_key": KEY, "paused": true});
        assert_eq!(apply(&mut v, "STREAM_UPDATE", no_ids), None);
    }

    #[test]
    fn updates_count_viewers_as_they_join_and_leave() {
        let mut v = Viewers::default();
        let update = |ids: Value| json!({"stream_key": KEY, "viewer_ids": ids, "paused": false});
        assert_eq!(
            apply(&mut v, "STREAM_UPDATE", update(json!(["10", "11"]))),
            Some(2)
        );
        assert_eq!(
            apply(&mut v, "STREAM_UPDATE", update(json!(["11"]))),
            Some(1)
        );
        assert_eq!(apply(&mut v, "STREAM_UPDATE", update(json!([]))), Some(0));
        let paused = json!({"stream_key": KEY, "paused": true});
        assert_eq!(apply(&mut v, "STREAM_UPDATE", paused), Some(0));
    }

    #[test]
    fn our_own_id_is_not_a_viewer() {
        let mut v = Viewers::default();
        let ids = json!({"stream_key": KEY, "viewer_ids": [ME, "10", 3]});
        assert_eq!(apply(&mut v, "STREAM_UPDATE", ids), Some(1));
        let only_me = json!({"stream_key": KEY, "viewer_ids": [ME]});
        assert_eq!(apply(&mut v, "STREAM_UPDATE", only_me), Some(0));
    }

    #[test]
    fn other_streams_are_ignored() {
        let mut v = Viewers::default();
        let ours = json!({"stream_key": KEY, "viewer_ids": ["10"]});
        assert_eq!(apply(&mut v, "STREAM_UPDATE", ours), Some(1));
        for key in ["guild:1:2:4", "guild:1:5:3", "call:2:3", "nonsense"] {
            let other = json!({"stream_key": key, "viewer_ids": ["10", "11", "12"]});
            assert_eq!(
                apply(&mut v, "STREAM_UPDATE", other.clone()),
                Some(1),
                "{key}"
            );
            assert_eq!(apply(&mut v, "STREAM_DELETE", other), Some(1), "{key}");
        }
        assert_eq!(
            apply(&mut v, "VOICE_STATE_UPDATE", json!({"stream_key": KEY})),
            Some(1)
        );
        assert_eq!(
            apply(&mut v, "STREAM_UPDATE", json!({"viewer_ids": []})),
            Some(1)
        );
    }

    #[test]
    fn a_call_stream_matches_without_a_guild() {
        let mut v = Viewers::default();
        let call = dispatch(
            "STREAM_UPDATE",
            json!({"stream_key": "call:2:3", "viewer_ids": ["10"]}),
        );
        v.apply(&call, None, CHANNEL, ME);
        assert_eq!(v.count(), Some(1));
    }

    #[test]
    fn a_new_or_ended_stream_forgets_the_count() {
        let mut v = Viewers::default();
        let ids = json!({"stream_key": KEY, "viewer_ids": ["10", "11"]});
        assert_eq!(apply(&mut v, "STREAM_UPDATE", ids), Some(2));
        let created = json!({"stream_key": KEY, "viewer_ids": []});
        assert_eq!(apply(&mut v, "STREAM_CREATE", created), None);
        let created_watched = json!({"stream_key": KEY, "viewer_ids": ["10"]});
        assert_eq!(apply(&mut v, "STREAM_CREATE", created_watched), Some(1));
        let deleted = json!({"stream_key": KEY, "unavailable": false});
        assert_eq!(apply(&mut v, "STREAM_DELETE", deleted), None);
        let ids = json!({"stream_key": KEY, "viewer_ids": ["10"]});
        apply(&mut v, "STREAM_UPDATE", ids);
        v.reset();
        assert_eq!(v.count(), None);
    }
}
