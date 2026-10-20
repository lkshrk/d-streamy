//! Linux capture backend: ScreenCast portal, PipeWire and GStreamer encoding.
#![cfg(target_os = "linux")]

mod encoder;
mod pipeline;
mod portal;
mod source;

pub use encoder::{EncoderSettings, EncoderSpec, encoder_candidates};
pub use portal::PortalSources;
pub use source::{LinuxCaptureSource, PORTAL_TARGET_ID};
