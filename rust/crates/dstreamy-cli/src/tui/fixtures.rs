use dstreamy_discord::{Guild, VoiceChannel, VoiceChannelKind};
use dstreamy_media::{CaptureTarget, TargetKind};

pub fn guild(id: &str, name: &str, available: bool) -> Guild {
    Guild {
        id: id.into(),
        name: name.into(),
        available,
    }
}

pub fn channel(id: &str, name: &str) -> VoiceChannel {
    VoiceChannel {
        id: id.into(),
        guild_id: "g1".into(),
        name: name.into(),
        position: 0,
        parent_id: None,
        kind: VoiceChannelKind::Voice,
    }
}

pub fn target(id: &str, title: &str) -> CaptureTarget {
    CaptureTarget {
        id: id.into(),
        kind: TargetKind::Display,
        title: title.into(),
        app: None,
        width: 1920,
        height: 1080,
    }
}
