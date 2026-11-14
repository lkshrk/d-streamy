//! Linux capture backend: ScreenCast portal, PipeWire and GStreamer encoding.
#![cfg(target_os = "linux")]

mod audio;
mod encoder;
mod pipeline;
mod portal;
mod source;

pub use source::{LinuxCaptureSource, PORTAL_TARGET_ID};
