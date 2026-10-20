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
