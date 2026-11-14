//! ScreenCaptureKit capture and VideoToolbox encoding for macOS; empty on other platforms.
#![cfg(target_os = "macos")]

mod audio;
mod dimensions;
mod liveness;
mod sink;
mod source;
mod targets;
mod video;
mod vt;

pub use source::MacCaptureSource;
pub use vt::{VtEncoder, VtEncoderConfig};
