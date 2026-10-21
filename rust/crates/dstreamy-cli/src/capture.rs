use std::sync::{Arc, Mutex, PoisonError};

use dstreamy_core::{Config, ConfigError, ConfigPaths};
use dstreamy_media::{
    CaptureSource, CaptureTarget, MediaError, TEST_PATTERN_TARGET_ID, TargetKind, TestPatternSource,
};
use serde::Serialize;

use crate::CliError;

/// The id of the one Linux target; the portal dialog picks the window or monitor.
pub const PORTAL_TARGET_ID: &str = "portal";

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
            kind: kind_label(t.kind),
            title: t.title,
            app: t.app,
            width: t.width,
            height: t.height,
        }
    }
}

pub const fn kind_label(kind: TargetKind) -> &'static str {
    match kind {
        TargetKind::Display => "display",
        TargetKind::Window => "window",
    }
}

/// The portal restore token a source holds after its latest successful start; `None` before one.
#[derive(Debug, Clone, Default)]
pub struct PortalToken(Arc<Mutex<Option<Option<String>>>>);

impl PortalToken {
    pub fn get(&self) -> Option<Option<String>> {
        self.0
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .clone()
    }

    #[cfg_attr(not(target_os = "linux"), allow(dead_code))]
    fn set(&self, token: Option<String>) {
        *self.0.lock().unwrap_or_else(PoisonError::into_inner) = Some(token);
    }
}

pub struct Picked {
    pub source: Box<dyn CaptureSource>,
    pub portal_token: PortalToken,
}

#[cfg(target_os = "macos")]
fn platform_source(
    _restore_token: Option<String>,
    _token: &PortalToken,
) -> Option<Box<dyn CaptureSource>> {
    Some(Box::new(dstreamy_capture_macos::MacCaptureSource::new()))
}

#[cfg(target_os = "linux")]
fn platform_source(
    restore_token: Option<String>,
    token: &PortalToken,
) -> Option<Box<dyn CaptureSource>> {
    let inner = dstreamy_capture_linux::LinuxCaptureSource::new().with_restore_token(restore_token);
    Some(Box::new(linux::TokenReporting {
        inner,
        token: token.clone(),
    }))
}

#[cfg(not(any(target_os = "macos", target_os = "linux")))]
fn platform_source(
    _restore_token: Option<String>,
    _token: &PortalToken,
) -> Option<Box<dyn CaptureSource>> {
    None
}

#[cfg(target_os = "linux")]
mod linux {
    use async_trait::async_trait;
    use dstreamy_capture_linux::LinuxCaptureSource;
    use dstreamy_media::{CaptureConfig, CaptureSource, CaptureTarget, FrameReceiver, MediaError};

    use super::PortalToken;

    pub struct TokenReporting {
        pub inner: LinuxCaptureSource,
        pub token: PortalToken,
    }

    #[async_trait]
    impl CaptureSource for TokenReporting {
        async fn list_targets(&self) -> Result<Vec<CaptureTarget>, MediaError> {
            self.inner.list_targets().await
        }

        async fn start(&mut self, config: CaptureConfig) -> Result<FrameReceiver, MediaError> {
            let started = self.inner.start(config).await;
            if started.is_ok() {
                self.token
                    .set(self.inner.restore_token().map(str::to_owned));
            }
            started
        }

        async fn stop(&mut self) -> Result<(), MediaError> {
            self.inner.stop().await
        }

        fn request_keyframe(&self) {
            self.inner.request_keyframe();
        }
    }
}

/// A fresh source for `target`; a Linux portal source starts from `restore_token`.
pub fn source_for(target: &str, restore_token: Option<String>) -> Result<Picked, CliError> {
    let portal_token = PortalToken::default();
    let source: Box<dyn CaptureSource> = if target == TEST_PATTERN_TARGET_ID {
        Box::new(TestPatternSource::new())
    } else {
        platform_source(restore_token, &portal_token).ok_or(CliError::NoPlatformCapture)?
    };
    Ok(Picked {
        source,
        portal_token,
    })
}

/// Platform targets (an error if listing them failed) followed by the test pattern.
pub async fn targets(include_platform: bool) -> (Vec<CaptureTarget>, Option<MediaError>) {
    let mut targets = Vec::new();
    let mut failure = None;
    if include_platform && let Some(source) = platform_source(None, &PortalToken::default()) {
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

pub fn target_id(target: &dstreamy_core::CaptureTarget) -> String {
    match target {
        dstreamy_core::CaptureTarget::Display { id } => format!("display:{id}"),
        dstreamy_core::CaptureTarget::Window { id } => format!("window:{id}"),
        dstreamy_core::CaptureTarget::Portal { .. } => PORTAL_TARGET_ID.to_owned(),
    }
}

/// The config form of a target id; `None` for ids that are not saved, like the test pattern.
pub fn config_target(
    id: &str,
    restore_token: Option<String>,
) -> Option<dstreamy_core::CaptureTarget> {
    if id == PORTAL_TARGET_ID {
        return Some(dstreamy_core::CaptureTarget::Portal { restore_token });
    }
    let (kind, n) = id.split_once(':')?;
    let n = n.parse().ok()?;
    match kind {
        "display" => Some(dstreamy_core::CaptureTarget::Display { id: n }),
        "window" => Some(dstreamy_core::CaptureTarget::Window { id: n }),
        _ => None,
    }
}

pub fn restore_token(config: &Config) -> Option<String> {
    match &config.target {
        Some(dstreamy_core::CaptureTarget::Portal { restore_token }) => restore_token.clone(),
        _ => None,
    }
}

/// Saves the token the portal reported if it differs from `previous`, clearing a spent one; true when it saved.
pub fn remember_portal_token(
    paths: &ConfigPaths,
    previous: Option<&str>,
    token: &PortalToken,
) -> Result<bool, ConfigError> {
    let Some(token) = token.get() else {
        return Ok(false);
    };
    if previous == token.as_deref() {
        return Ok(false);
    }
    let mut config = Config::load(&paths.config_file)?;
    config.target = Some(dstreamy_core::CaptureTarget::Portal {
        restore_token: token,
    });
    config.save(&paths.config_file)?;
    Ok(true)
}

/// Saves each new token a source reports, once.
pub struct PortalBook {
    previous: Option<String>,
    token: PortalToken,
}

impl PortalBook {
    pub fn new(previous: Option<String>, token: PortalToken) -> Self {
        Self { previous, token }
    }

    pub fn save(&mut self, paths: &ConfigPaths) -> Result<bool, ConfigError> {
        let saved = remember_portal_token(paths, self.previous.as_deref(), &self.token)?;
        if saved {
            self.previous = self.token.get().flatten();
        }
        Ok(saved)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn config_targets_round_trip_through_ids() {
        for id in ["display:1", "window:42", "portal"] {
            let target = config_target(id, None).unwrap();
            assert_eq!(target_id(&target), id);
        }
        for id in [TEST_PATTERN_TARGET_ID, "display:x", "screen:1", ""] {
            assert!(config_target(id, None).is_none(), "{id}");
        }
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn portal_id_matches_the_linux_backend() {
        assert_eq!(PORTAL_TARGET_ID, dstreamy_capture_linux::PORTAL_TARGET_ID);
    }

    #[test]
    fn a_new_portal_token_is_saved_once() {
        let dir = tempfile::tempdir().unwrap();
        let paths = ConfigPaths::in_dir(dir.path());
        let token = PortalToken::default();
        assert!(!remember_portal_token(&paths, None, &token).unwrap());
        assert!(!paths.config_file.exists());

        token.set(Some("abc".into()));
        assert!(!remember_portal_token(&paths, Some("abc"), &token).unwrap());
        assert!(remember_portal_token(&paths, Some("old"), &token).unwrap());
        let config = Config::load(&paths.config_file).unwrap();
        assert_eq!(restore_token(&config).as_deref(), Some("abc"));
        assert_eq!(config.stream, Default::default());
    }

    #[test]
    fn a_spent_portal_token_is_cleared() {
        let dir = tempfile::tempdir().unwrap();
        let paths = ConfigPaths::in_dir(dir.path());
        let token = PortalToken::default();
        token.set(Some("old".into()));
        assert!(remember_portal_token(&paths, None, &token).unwrap());

        token.set(None);
        assert!(!remember_portal_token(&paths, None, &token).unwrap());
        assert!(remember_portal_token(&paths, Some("old"), &token).unwrap());
        let config = Config::load(&paths.config_file).unwrap();
        assert_eq!(
            config.target,
            Some(dstreamy_core::CaptureTarget::Portal {
                restore_token: None
            })
        );
    }

    #[test]
    fn the_book_saves_each_new_token_once() {
        let dir = tempfile::tempdir().unwrap();
        let paths = ConfigPaths::in_dir(dir.path());
        let token = PortalToken::default();
        let mut book = PortalBook::new(Some("old".into()), token.clone());
        assert!(!book.save(&paths).unwrap());
        token.set(Some("new".into()));
        assert!(book.save(&paths).unwrap());
        assert!(!book.save(&paths).unwrap());
        token.set(Some("newer".into()));
        assert!(book.save(&paths).unwrap());
        let config = Config::load(&paths.config_file).unwrap();
        assert_eq!(restore_token(&config).as_deref(), Some("newer"));
    }
}
