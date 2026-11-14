use std::sync::{Arc, Mutex};

use dstreamy_core::{Config, ConfigError, ConfigPaths};
use dstreamy_media::{
    AudioSource, AudioSourceKind, CaptureSource, CaptureTarget, MediaError, TEST_PATTERN_TARGET_ID,
    TargetKind, TestPatternSource,
};
use serde::Serialize;

use crate::CliError;

#[derive(Debug, Serialize)]
pub struct AudioSourceRow {
    pub id: String,
    pub kind: &'static str,
    pub name: String,
}

impl From<AudioSource> for AudioSourceRow {
    fn from(s: AudioSource) -> Self {
        Self {
            id: s.id,
            kind: match s.kind {
                AudioSourceKind::Output => "output",
                AudioSourceKind::Input => "input",
            },
            name: s.name,
        }
    }
}

/// The audio devices a stream may capture; empty where the platform picks its own.
pub async fn audio_sources() -> Result<Vec<AudioSource>, CliError> {
    match platform_source(None, &PortalToken::default()) {
        Some(source) => Ok(source.list_audio_sources().await?),
        None => Ok(Vec::new()),
    }
}

/// Errors unless `id` names a listed audio source; a warning when the devices cannot be listed.
pub fn check_audio_source(
    id: &str,
    listed: Result<Vec<AudioSource>, MediaError>,
) -> Result<Option<String>, CliError> {
    let unknown = || Err(CliError::UnknownAudioSource(id.to_owned()));
    if id.trim().is_empty() {
        return unknown();
    }
    let why = match listed {
        Ok(sources) if sources.iter().any(|s| s.id == id) => return Ok(None),
        Ok(sources) if !sources.is_empty() => return unknown(),
        Ok(_) => "no audio devices were listed".to_owned(),
        Err(e) => format!("listing the audio devices failed: {e}"),
    };
    Ok(Some(format!(
        "cannot confirm that audio source {id:?} exists ({why}); capturing it anyway, \
         and the stream fails rather than capture another device if it does not"
    )))
}

/// [`check_audio_source`] against what `source` lists; the test pattern captures no audio device.
pub async fn verify_audio_source(
    target: &str,
    id: &str,
    source: &dyn CaptureSource,
) -> Result<Option<String>, CliError> {
    if !id.trim().is_empty() && target == TEST_PATTERN_TARGET_ID {
        return Ok(None);
    }
    check_audio_source(id, source.list_audio_sources().await)
}

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

/// The portal restore token a source holds after its latest start or stop; `None` before a start.
#[derive(Debug, Clone, Default)]
pub struct PortalToken(Arc<Mutex<Option<Option<String>>>>);

impl PortalToken {
    pub fn get(&self) -> Option<Option<String>> {
        dstreamy_core::lock(&self.0).clone()
    }

    #[cfg(any(target_os = "linux", test))]
    fn set(&self, token: Option<String>) {
        *dstreamy_core::lock(&self.0) = Some(token);
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
    use dstreamy_media::{
        AudioActivity, AudioSource, CaptureConfig, CaptureSource, CaptureTarget, FrameReceiver,
        MediaError,
    };

    use super::PortalToken;

    /// A source whose portal restore token can change on start and stop.
    pub trait RestoreTokenSource: CaptureSource {
        fn restore_token(&self) -> Option<&str>;
    }

    impl RestoreTokenSource for LinuxCaptureSource {
        fn restore_token(&self) -> Option<&str> {
            LinuxCaptureSource::restore_token(self)
        }
    }

    pub struct TokenReporting<S = LinuxCaptureSource> {
        pub inner: S,
        pub token: PortalToken,
    }

    impl<S: RestoreTokenSource> TokenReporting<S> {
        fn report(&self) {
            self.token
                .set(self.inner.restore_token().map(str::to_owned));
        }
    }

    #[async_trait]
    impl<S: RestoreTokenSource> CaptureSource for TokenReporting<S> {
        async fn list_targets(&self) -> Result<Vec<CaptureTarget>, MediaError> {
            self.inner.list_targets().await
        }

        async fn list_audio_sources(&self) -> Result<Vec<AudioSource>, MediaError> {
            self.inner.list_audio_sources().await
        }

        fn audio_activity(&self) -> Option<AudioActivity> {
            self.inner.audio_activity()
        }

        fn capture_drops(&self) -> dstreamy_media::CaptureDrops {
            self.inner.capture_drops()
        }

        /// Reports the token even when the start fails: the portal spent the old one on the way.
        async fn start(&mut self, config: CaptureConfig) -> Result<FrameReceiver, MediaError> {
            let started = self.inner.start(config).await;
            self.report();
            started
        }

        async fn stop(&mut self) -> Result<(), MediaError> {
            let stopped = self.inner.stop().await;
            self.report();
            stopped
        }

        fn request_keyframe(&self) {
            self.inner.request_keyframe();
        }
    }
}

/// A test target whose source reports a fresh portal token and then fails to start.
#[cfg(test)]
pub const FAILING_PORTAL_TARGET: &str = "test-failing-portal";
#[cfg(test)]
pub const FRESH_PORTAL_TOKEN: &str = "fresh-grant";

#[cfg(test)]
struct FailingPortal(PortalToken);

#[cfg(test)]
#[async_trait::async_trait]
impl CaptureSource for FailingPortal {
    async fn list_targets(&self) -> Result<Vec<CaptureTarget>, MediaError> {
        Ok(Vec::new())
    }

    async fn start(
        &mut self,
        _config: dstreamy_media::CaptureConfig,
    ) -> Result<dstreamy_media::FrameReceiver, MediaError> {
        self.0.set(Some(FRESH_PORTAL_TOKEN.into()));
        Err(MediaError::Backend("no encoder works".into()))
    }

    async fn stop(&mut self) -> Result<(), MediaError> {
        Ok(())
    }

    fn request_keyframe(&self) {}
}

/// A fresh source for `target`; a Linux portal source starts from `restore_token`.
pub fn source_for(target: &str, restore_token: Option<String>) -> Result<Picked, CliError> {
    let portal_token = PortalToken::default();
    #[cfg(test)]
    if target == FAILING_PORTAL_TARGET {
        return Ok(Picked {
            source: Box::new(FailingPortal(portal_token.clone())),
            portal_token,
        });
    }
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
    Config::update(&paths.config_file, |config| {
        config.target = Some(dstreamy_core::CaptureTarget::Portal {
            restore_token: token,
        });
    })?;
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

    #[cfg(target_os = "linux")]
    struct SpendingSource {
        token: Option<String>,
        on_start: Option<String>,
        on_stop: Option<String>,
    }

    #[cfg(target_os = "linux")]
    #[async_trait::async_trait]
    impl CaptureSource for SpendingSource {
        async fn list_targets(&self) -> Result<Vec<CaptureTarget>, MediaError> {
            Ok(Vec::new())
        }

        async fn start(
            &mut self,
            _config: dstreamy_media::CaptureConfig,
        ) -> Result<dstreamy_media::FrameReceiver, MediaError> {
            self.token = self.on_start.clone();
            Err(MediaError::Backend("no encoder works".into()))
        }

        async fn stop(&mut self) -> Result<(), MediaError> {
            self.token = self.on_stop.clone();
            Err(MediaError::Backend("the share went away".into()))
        }

        fn request_keyframe(&self) {}
    }

    #[cfg(target_os = "linux")]
    impl linux::RestoreTokenSource for SpendingSource {
        fn restore_token(&self) -> Option<&str> {
            self.token.as_deref()
        }
    }

    #[cfg(target_os = "linux")]
    #[tokio::test]
    async fn a_token_from_a_failed_start_or_a_stop_is_reported() {
        let token = PortalToken::default();
        let mut source = linux::TokenReporting {
            inner: SpendingSource {
                token: Some("spent".into()),
                on_start: Some("fresh".into()),
                on_stop: None,
            },
            token: token.clone(),
        };
        assert!(source.start(Default::default()).await.is_err());
        assert_eq!(token.get(), Some(Some("fresh".into())));
        assert!(source.stop().await.is_err());
        assert_eq!(token.get(), Some(None));
    }

    #[test]
    fn a_token_replaces_a_stale_non_portal_target() {
        let dir = tempfile::tempdir().unwrap();
        let paths = ConfigPaths::in_dir(dir.path());
        Config {
            target: Some(dstreamy_core::CaptureTarget::Display { id: 2 }),
            ..Config::default()
        }
        .save(&paths.config_file)
        .unwrap();
        let token = PortalToken::default();
        token.set(Some("new".into()));
        assert!(remember_portal_token(&paths, None, &token).unwrap());
        let config = Config::load(&paths.config_file).unwrap();
        assert_eq!(restore_token(&config).as_deref(), Some("new"));
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

    #[test]
    fn audio_sources_are_checked_against_the_list_when_there_is_one() {
        use dstreamy_media::{AudioSource, AudioSourceKind};
        let listed = [AudioSource {
            id: "pipeweaver_stream".into(),
            kind: AudioSourceKind::Input,
            name: "PipeWeaver Stream".into(),
        }];
        let check = |id: &str, listed: &[AudioSource]| check_audio_source(id, Ok(listed.to_vec()));
        assert!(matches!(check("pipeweaver_stream", &listed), Ok(None)));
        let err = check("nope", &listed).unwrap_err();
        assert!(err.to_string().contains("dstreamy audio-sources"), "{err}");
        let warning = check("anything", &[]).unwrap().unwrap();
        assert!(warning.contains("\"anything\""), "{warning}");
        assert!(
            warning.contains("fails rather than capture another device"),
            "{warning}"
        );
        let failed = check_audio_source(
            "pipeweaver_stream",
            Err(dstreamy_media::MediaError::Backend("no pulse".into())),
        )
        .unwrap()
        .unwrap();
        assert!(
            failed.contains("listing the audio devices failed: "),
            "{failed}"
        );
        assert!(check("", &[]).is_err());
        assert!(
            check_audio_source(" ", Err(dstreamy_media::MediaError::Backend("x".into()))).is_err()
        );
    }
}
