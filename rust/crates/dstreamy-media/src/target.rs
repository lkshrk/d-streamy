/// Whether a capture target is a display or a window.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum TargetKind {
    Display,
    Window,
}

/// A window or display a backend can capture; `id` is opaque and backend-specific.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CaptureTarget {
    pub id: String,
    pub kind: TargetKind,
    pub title: String,
    pub app: Option<String>,
    pub width: u32,
    pub height: u32,
}

/// Whether an audio device plays sound or records it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum AudioSourceKind {
    /// What plays on an output device.
    Output,
    /// A microphone, line-in or virtual source.
    Input,
}

/// An audio device a backend can capture; `id` is what [`CaptureConfig::audio_source`] takes.
///
/// [`CaptureConfig::audio_source`]: crate::CaptureConfig::audio_source
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AudioSource {
    pub id: String,
    pub kind: AudioSourceKind,
    pub name: String,
}
