use dstreamy_media::{
    CaptureSource, CaptureTarget, MediaError, TEST_PATTERN_TARGET_ID, TargetKind, TestPatternSource,
};
use serde::Serialize;

use crate::CliError;

#[derive(Debug, Serialize)]
pub struct TargetRow {
    pub id: String,
    pub kind: &'static str,
    pub title: String,
    pub app: Option<String>,
    pub width: u32,
    pub height: u32,
}

impl From<CaptureTarget> for TargetRow {
    fn from(t: CaptureTarget) -> Self {
        Self {
            id: t.id,
            kind: match t.kind {
                TargetKind::Display => "display",
                TargetKind::Window => "window",
            },
            title: t.title,
            app: t.app,
            width: t.width,
            height: t.height,
        }
    }
}

#[cfg(target_os = "macos")]
fn platform_source() -> Option<Box<dyn CaptureSource>> {
    Some(Box::new(dstreamy_capture_macos::MacCaptureSource::new()))
}

#[cfg(target_os = "linux")]
fn platform_source() -> Option<Box<dyn CaptureSource>> {
    Some(Box::new(dstreamy_capture_linux::LinuxCaptureSource::new()))
}

#[cfg(not(any(target_os = "macos", target_os = "linux")))]
fn platform_source() -> Option<Box<dyn CaptureSource>> {
    None
}

pub fn source_for(target: &str) -> Result<Box<dyn CaptureSource>, CliError> {
    if target == TEST_PATTERN_TARGET_ID {
        return Ok(Box::new(TestPatternSource::new()));
    }
    platform_source().ok_or(CliError::NoPlatformCapture)
}

/// Platform targets (an error if listing them failed) followed by the test pattern.
pub async fn targets(include_platform: bool) -> (Vec<CaptureTarget>, Option<MediaError>) {
    let mut targets = Vec::new();
    let mut failure = None;
    if include_platform && let Some(source) = platform_source() {
        match source.list_targets().await {
            Ok(list) => targets.extend(list),
            Err(e) => failure = Some(e),
        }
    }
    if let Ok(list) = TestPatternSource::new().list_targets().await {
        targets.extend(list);
    }
    (targets, failure)
}
