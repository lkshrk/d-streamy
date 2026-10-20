//! ScreenCaptureKit capture and VideoToolbox encoding for macOS; empty on other platforms.
#![cfg(target_os = "macos")]

mod audio;
mod dimensions;
mod source;
mod targets;
mod vt;

pub use audio::{AudioPacketizer, SourceBuffer, stereo_i16_from_f32};
pub use dimensions::CaptureDimensions;
pub use source::MacCaptureSource;
pub use targets::{TargetId, is_user_window};
pub use vt::{VtEncoder, VtEncoderConfig};
