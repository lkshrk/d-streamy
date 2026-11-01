//! What the settings dialog does with the module config: when to read it, when to write it, and what it shows.

use std::fs::{self, File};
use std::io::{self, Write};
use std::path::Path;
use std::time::{SystemTime, UNIX_EPOCH};

use serde_json::Value;

use crate::dock::{DockModel, DockSettings, track_name};

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ConfigError {
    /// OBS gave the module no config folder.
    NoFolder,
    /// The file exists but is not a settings object; it is never overwritten.
    Corrupt { path: String, detail: String },
    /// The file exists but reading it failed; it is never overwritten either.
    Unreadable { path: String, detail: String },
    /// A write failed; it is retried on the next open.
    Failed(String),
}

impl ConfigError {
    pub fn message(&self, track: u8) -> String {
        let track = track_name(track);
        match self {
            Self::NoFolder => format!(
                "OBS gave D-Streamy no settings folder, so {track} is used until OBS quits but is not saved"
            ),
            Self::Corrupt { path, detail } => format!(
                "Couldn't read D-Streamy's settings from {path}: not valid JSON ({detail}). \
                 Fix or delete the file; until then {track} is used until OBS quits but is not saved"
            ),
            Self::Unreadable { path, detail } => format!(
                "Couldn't read D-Streamy's settings from {path}: {detail}. \
                 Fix or delete the file; until then {track} is used until OBS quits but is not saved"
            ),
            Self::Failed(reason) => format!("{reason}; {track} is used until OBS quits"),
        }
    }
}

/// A config file's raw contents, and where it was read.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Saved {
    pub path: String,
    pub bytes: Vec<u8>,
}

/// A file's bytes as settings, or `Corrupt`.
pub fn decode(Saved { path, bytes }: Saved) -> Result<DockSettings, ConfigError> {
    String::from_utf8(bytes)
        .map_err(|e| format!("not UTF-8: {e}"))
        .and_then(|text| parse(&text))
        .map_err(|detail| ConfigError::Corrupt { path, detail })
}

/// The settings JSON as saved, or why it can't be used.
pub fn parse(text: &str) -> Result<DockSettings, String> {
    match serde_json::from_str::<Value>(text) {
        Ok(Value::Object(_)) => Ok(DockSettings::from_json(text)),
        Ok(_) => Err("not an object".into()),
        Err(e) => Err(e.to_string()),
    }
}

/// The saved bytes at `path`, or `None` only when no file exists yet.
pub fn load_file(path: &Path) -> Result<Option<Saved>, ConfigError> {
    match fs::read(path) {
        Ok(bytes) => Ok(Some(Saved {
            path: path.display().to_string(),
            bytes,
        })),
        Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(ConfigError::Unreadable {
            path: path.display().to_string(),
            detail: e.to_string(),
        }),
    }
}

/// Writes `json` to `path` through a synced temp file and a rename, then syncs the folder.
pub fn save_file(path: &Path, json: &str) -> Result<(), ConfigError> {
    save_file_checked(path, json, || {})
}

fn save_file_checked(
    path: &Path,
    json: &str,
    before_rename: impl FnOnce(),
) -> Result<(), ConfigError> {
    let failed = |e: io::Error| {
        ConfigError::Failed(format!(
            "Couldn't save D-Streamy's settings to {}: {e}",
            path.display()
        ))
    };
    let dir = path.parent().unwrap_or(Path::new("."));
    fs::create_dir_all(dir).map_err(failed)?;
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.subsec_nanos());
    // Unique per process and call, so two OBS instances never share a temp file.
    let tmp = path.with_extension(format!("json.{}.{nanos}.tmp", std::process::id()));
    let result = write_synced(&tmp, json).map_err(failed).and_then(|()| {
        before_rename();
        // No lock covers files edited outside OBS, so a file broken since the last read is refused here instead.
        if let Some(saved) = load_file(path)? {
            decode(saved)?;
        }
        fs::rename(&tmp, path).map_err(failed)
    });
    if result.is_err() {
        let _ = fs::remove_file(&tmp);
    }
    result?;
    sync_dir(dir).map_err(failed)
}

fn write_synced(tmp: &Path, json: &str) -> io::Result<()> {
    let mut file = File::create(tmp)?;
    file.write_all(json.as_bytes())?;
    file.sync_all()
}

#[cfg(unix)]
fn sync_dir(dir: &Path) -> io::Result<()> {
    File::open(dir)?.sync_all()
}

/// Windows can't open a folder as a file; its rename is durable once MoveFileEx returns.
#[cfg(not(unix))]
fn sync_dir(_dir: &Path) -> io::Result<()> {
    Ok(())
}

/// The producer model with its module-config state.
#[derive(Debug, Clone)]
pub struct ConfigState {
    pub model: DockModel,
    error: Option<ConfigError>,
    /// The model holds settings the file doesn't; the file must not be read over them.
    unsaved: bool,
}

impl Default for ConfigState {
    fn default() -> Self {
        Self {
            model: DockModel::new(DockSettings::default()),
            error: None,
            unsaved: false,
        }
    }
}

impl ConfigState {
    pub fn error(&self) -> Option<&ConfigError> {
        self.error.as_ref()
    }

    /// An existing file can't be used, so nothing may replace it until it reads cleanly.
    fn blocked(&self) -> bool {
        matches!(
            self.error,
            Some(ConfigError::Corrupt { .. } | ConfigError::Unreadable { .. })
        )
    }

    /// A dialog opens and the file is read first. A failed write is retried only over a file that still reads
    /// cleanly; a file repaired after being unusable is loaded instead.
    pub fn open(
        &mut self,
        read: impl FnOnce() -> Result<Option<Saved>, ConfigError>,
        write: impl FnOnce(&str) -> Result<(), ConfigError>,
    ) {
        let was_blocked = self.blocked();
        match read().and_then(|saved| saved.map(decode).transpose()) {
            Ok(_) if self.unsaved && !was_blocked => self.save(write),
            Ok(Some(settings)) => {
                let file_owned = settings.owned.clone();
                self.model.reload(settings);
                self.error = None;
                self.unsaved = false;
                // A running session keeps its own ownership over the file's, and the file must say so.
                if self.model.persisted() != file_owned.as_ref() {
                    self.save(write);
                }
            }
            Ok(None) if self.unsaved => self.save(write),
            Ok(None) => self.error = None,
            Err(e) => self.error = Some(e),
        }
    }

    /// Runs a session step; an ownership it changes is saved at once, so a later open reads back the same one.
    pub fn session<R>(
        &mut self,
        write: impl FnOnce(&str) -> Result<(), ConfigError>,
        step: impl FnOnce(&mut DockModel) -> R,
    ) -> R {
        let before = self.model.persisted().cloned();
        let result = step(&mut self.model);
        if self.model.persisted() != before.as_ref() {
            if self.blocked() {
                self.unsaved = true;
            } else {
                self.save(write);
            }
        }
        result
    }

    /// The dialog picked a track. False when out of range; a corrupt or unreadable file is left alone.
    pub fn select_audio_track(
        &mut self,
        track: u8,
        write: impl FnOnce(&str) -> Result<(), ConfigError>,
    ) -> bool {
        if !self.model.select_audio_track(track) {
            return false;
        }
        if self.blocked() {
            self.unsaved = true;
        } else {
            self.save(write);
        }
        true
    }

    /// The dialog changed another setting; saved like a track.
    pub fn edit<R>(
        &mut self,
        write: impl FnOnce(&str) -> Result<(), ConfigError>,
        step: impl FnOnce(&mut DockModel) -> R,
    ) -> R {
        let before = self.model.settings().clone();
        let result = step(&mut self.model);
        if self.model.settings() != &before {
            if self.blocked() {
                self.unsaved = true;
            } else {
                self.save(write);
            }
        }
        result
    }

    fn save(&mut self, write: impl FnOnce(&str) -> Result<(), ConfigError>) {
        match write(&self.model.settings().to_json()) {
            Ok(()) => {
                self.error = None;
                self.unsaved = false;
            }
            Err(e) => {
                self.error = Some(e);
                self.unsaved = true;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::control::{Candidate, Ownership, Started};
    use crate::dock::Canvas;
    use std::cell::RefCell;

    fn failed() -> ConfigError {
        ConfigError::Failed("Couldn't save D-Streamy's settings to /c: denied".into())
    }

    /// A config file and whether writing it works.
    struct File {
        text: RefCell<Option<String>>,
        writable: bool,
    }

    impl File {
        fn new(text: Option<&str>) -> Self {
            Self {
                text: RefCell::new(text.map(str::to_owned)),
                writable: true,
            }
        }
        fn read(&self) -> impl FnOnce() -> Result<Option<Saved>, ConfigError> + '_ {
            || {
                Ok(self.text.borrow().clone().map(|text| Saved {
                    path: "/c".into(),
                    bytes: text.into_bytes(),
                }))
            }
        }
        fn write(&self) -> impl FnOnce(&str) -> Result<(), ConfigError> + '_ {
            |json| {
                if !self.writable {
                    return Err(failed());
                }
                *self.text.borrow_mut() = Some(json.to_owned());
                Ok(())
            }
        }
        fn track(&self) -> Option<u64> {
            let text = self.text.borrow().clone()?;
            serde_json::from_str::<Value>(&text).ok()?["audio_track"].as_u64()
        }
    }

    fn open(s: &mut ConfigState, f: &File) {
        s.open(f.read(), f.write());
    }

    #[test]
    fn only_a_json_object_parses() {
        assert_eq!(parse(r#"{"audio_track":4}"#).unwrap().audio_track, 4);
        assert_eq!(parse("{}").unwrap(), DockSettings::default());
        for bad in ["", "garbage", "[1]", "3", "{\"audio_track\":"] {
            assert!(parse(bad).is_err(), "{bad}");
        }
    }

    #[test]
    fn a_saved_track_reopens_and_writes_keep_the_other_settings() {
        let f = File::new(Some(
            r#"{"audio_track":3,"guild_id":"g","bitrate_kbps":2500}"#,
        ));
        let mut s = ConfigState::default();
        open(&mut s, &f);
        assert_eq!(s.model.settings().audio_track, 3);
        assert!(s.select_audio_track(5, f.write()));
        assert_eq!(f.track(), Some(5));
        let saved = DockSettings::from_json(f.text.borrow().as_deref().unwrap());
        assert_eq!(saved.guild_id.as_deref(), Some("g"));
        assert_eq!(saved.bitrate_kbps, 2500);
        assert!(!s.select_audio_track(9, f.write()));
        assert_eq!(f.track(), Some(5));
        assert_eq!(s.error(), None);
    }

    #[test]
    fn a_corrupt_file_is_reported_and_never_overwritten() {
        let f = File::new(Some("not json"));
        let mut s = ConfigState::default();
        open(&mut s, &f);
        assert!(matches!(s.error(), Some(ConfigError::Corrupt { .. })));
        assert!(s.select_audio_track(4, f.write()));
        assert_eq!(f.text.borrow().as_deref(), Some("not json"));
        open(&mut s, &f);
        assert_eq!(f.text.borrow().as_deref(), Some("not json"));
        assert_eq!(s.model.settings().audio_track, 4, "kept across reopen");
        assert!(matches!(s.error(), Some(ConfigError::Corrupt { .. })));
        let msg = s.error().unwrap().message(4);
        assert!(
            msg.starts_with("Couldn't read D-Streamy's settings from /c: not valid JSON ("),
            "{msg}"
        );
        assert!(
            msg.contains("Track 4 is used until OBS quits but is not saved"),
            "{msg}"
        );
    }

    #[test]
    fn a_repaired_file_is_read_again() {
        let f = File::new(Some("[]"));
        let mut s = ConfigState::default();
        open(&mut s, &f);
        s.select_audio_track(4, f.write());
        *f.text.borrow_mut() = Some(r#"{"audio_track":2}"#.into());
        open(&mut s, &f);
        assert_eq!(s.error(), None);
        assert_eq!(s.model.settings().audio_track, 2);
        assert!(s.select_audio_track(6, f.write()));
        assert_eq!(f.track(), Some(6));
    }

    #[test]
    fn an_unsaved_track_survives_reopening_until_a_save_works() {
        for initial in [Some(r#"{"audio_track":1}"#), None] {
            let mut f = File::new(initial);
            let mut s = ConfigState::default();
            open(&mut s, &f);
            f.writable = false;
            assert!(s.select_audio_track(4, f.write()));
            assert_eq!(s.error(), Some(&failed()));
            open(&mut s, &f);
            assert_eq!(s.model.settings().audio_track, 4, "{initial:?}");
            assert_eq!(s.error(), Some(&failed()));
            assert_eq!(f.track(), initial.map(|_| 1));
            f.writable = true;
            open(&mut s, &f);
            assert_eq!(s.error(), None);
            assert_eq!(f.track(), Some(4), "the retry saves it");
        }
    }

    #[test]
    fn a_read_error_keeps_the_model_and_is_shown() {
        let mut s = ConfigState::default();
        s.model.select_audio_track(3);
        s.open(|| Err(unreadable()), |_| unreachable!("nothing to save"));
        assert_eq!(s.model.settings().audio_track, 3);
        assert_eq!(s.error(), Some(&unreadable()));
    }

    fn unreadable() -> ConfigError {
        ConfigError::Unreadable {
            path: "/c".into(),
            detail: "Permission denied (os error 13)".into(),
        }
    }

    #[test]
    fn an_existing_file_that_cant_be_read_is_never_written() {
        let mut s = ConfigState::default();
        s.model.select_audio_track(2);
        s.open(|| Err(unreadable()), |_| panic!("no write on open"));
        assert_eq!(s.error(), Some(&unreadable()));
        assert!(s.select_audio_track(4, |_| panic!("no write while unreadable")));
        s.open(
            || Err(unreadable()),
            |_| panic!("no retry while unreadable"),
        );
        assert_eq!(s.model.settings().audio_track, 4);
        assert_eq!(s.error(), Some(&unreadable()));
        let msg = s.error().unwrap().message(4);
        assert_eq!(
            msg,
            "Couldn't read D-Streamy's settings from /c: Permission denied (os error 13). \
             Fix or delete the file; until then Track 4 is used until OBS quits but is not saved"
        );
        let f = File::new(Some(r#"{"audio_track":3}"#));
        open(&mut s, &f);
        assert_eq!(s.error(), None, "readable again");
        assert_eq!(s.model.settings().audio_track, 3);
    }

    #[test]
    fn a_file_that_is_not_utf8_is_corrupt_and_never_written() {
        let mut s = ConfigState::default();
        let bytes = vec![b'{', 0xff, 0xfe, b'}'];
        let read = || {
            Ok(Some(Saved {
                path: "/c".into(),
                bytes: bytes.clone(),
            }))
        };
        s.open(read, |_| panic!("no write on open"));
        assert!(
            matches!(s.error(), Some(ConfigError::Corrupt { detail, .. }) if detail.starts_with("not UTF-8")),
            "{:?}",
            s.error()
        );
        assert!(s.select_audio_track(5, |_| panic!("no write while corrupt")));
        s.open(read, |_| panic!("no retry while corrupt"));
        assert_eq!(s.model.settings().audio_track, 5);
    }

    #[test]
    fn a_failed_write_is_retried_over_a_file_that_still_reads_cleanly() {
        let f = File::new(Some(r#"{"audio_track":2}"#));
        let mut s = ConfigState::default();
        s.select_audio_track(4, |_| Err(failed()));
        open(&mut s, &f);
        assert_eq!(f.track(), Some(4), "unsaved settings win over the file");
        assert_eq!(s.error(), None);
    }

    #[test]
    fn a_failed_write_is_not_retried_over_a_file_that_became_unusable() {
        for (bytes, unusable) in [
            (Some(b"garbage".to_vec()), "corrupt"),
            (Some(vec![0xff, 0xfe]), "not UTF-8"),
            (None, "unreadable"),
        ] {
            let mut s = ConfigState::default();
            s.select_audio_track(4, |_| Err(failed()));
            let read = || match &bytes {
                Some(bytes) => Ok(Some(Saved {
                    path: "/c".into(),
                    bytes: bytes.clone(),
                })),
                None => Err(unreadable()),
            };
            s.open(read, |_| panic!("{unusable}: never written"));
            assert!(
                matches!(
                    s.error(),
                    Some(ConfigError::Corrupt { .. } | ConfigError::Unreadable { .. })
                ),
                "{unusable}: {:?}",
                s.error()
            );
            assert!(s.select_audio_track(5, |_| panic!("{unusable}: blocked")));
            s.open(read, |_| panic!("{unusable}: still blocked"));
            assert_eq!(s.model.settings().audio_track, 5, "{unusable}");
            let f = File::new(Some(r#"{"audio_track":1}"#));
            open(&mut s, &f);
            assert_eq!(
                s.model.settings().audio_track,
                1,
                "{unusable}: the repaired file is loaded"
            );
            assert_eq!(s.error(), None);
            assert!(s.select_audio_track(3, f.write()));
            assert_eq!(f.track(), Some(3), "{unusable}: saved again once usable");
        }
    }

    fn scratch() -> tempfile::TempDir {
        let base = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../target/test-tmp");
        fs::create_dir_all(&base).unwrap();
        tempfile::tempdir_in(base).unwrap()
    }

    fn leftovers(dir: &Path) -> Vec<String> {
        fs::read_dir(dir)
            .unwrap()
            .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
            .filter(|n| n.ends_with(".tmp"))
            .collect()
    }

    #[test]
    fn a_save_replaces_the_file_and_leaves_no_temp_file() {
        let dir = scratch();
        let path = dir.path().join("obs-dstreamy").join("config.json");
        assert_eq!(load_file(&path), Ok(None));
        save_file(&path, r#"{"audio_track":3}"#).unwrap();
        save_file(&path, r#"{"audio_track":4}"#).unwrap();
        let saved = load_file(&path).unwrap().unwrap();
        assert_eq!(decode(saved).unwrap().audio_track, 4);
        assert!(leftovers(path.parent().unwrap()).is_empty());
    }

    #[test]
    fn a_file_broken_between_the_read_and_the_rename_is_kept() {
        let dir = scratch();
        let path = dir.path().join("config.json");
        fs::write(&path, r#"{"audio_track":2}"#).unwrap();
        for broken in [b"garbage".as_slice(), &[0xff, 0xfe]] {
            let e = save_file_checked(&path, r#"{"audio_track":5}"#, || {
                fs::write(&path, broken).unwrap()
            })
            .unwrap_err();
            assert!(matches!(e, ConfigError::Corrupt { .. }), "{e:?}");
            assert_eq!(fs::read(&path).unwrap(), broken);
            assert!(leftovers(dir.path()).is_empty());
        }
    }

    #[test]
    fn a_folder_in_place_of_the_file_is_unreadable_and_kept() {
        let dir = scratch();
        let path = dir.path().join("config.json");
        let e = save_file_checked(&path, "{}", || fs::create_dir(&path).unwrap()).unwrap_err();
        assert!(matches!(e, ConfigError::Unreadable { .. }), "{e:?}");
        assert!(path.is_dir());
        let e = load_file(&path).unwrap_err();
        assert!(matches!(e, ConfigError::Unreadable { .. }), "{e:?}");
        assert!(
            e.message(1).ends_with("Fix or delete the file; until then Track 1 is used until OBS quits but is not saved"),
            "{}",
            e.message(1)
        );
        assert!(leftovers(dir.path()).is_empty());
    }

    fn picked(file: &str) -> (File, ConfigState) {
        let f = File::new(Some(file));
        let mut s = ConfigState::default();
        open(&mut s, &f);
        (f, s)
    }

    fn owned(file: &File) -> Option<u64> {
        let text = file.text.borrow().clone()?;
        let settings = parse(&text).unwrap();
        settings.owned.map(|o| o.connections[0].id)
    }

    fn ownership(id: u64) -> Ownership {
        Ownership {
            connections: vec![Candidate { id, fps: Some(30) }],
            guild_id: "g".into(),
            channel_id: "c".into(),
            instance: None,
        }
    }

    const CANVAS: Canvas = Canvas {
        fps_num: 30,
        fps_den: 1,
        output_width: 1280,
        output_height: 720,
    };

    fn started(id: u64) -> Started {
        Started {
            ownership: Some(ownership(id)),
            channel: "c".into(),
        }
    }

    const OWNED_3: &str = r#"{"guild_id":"g","channel_id":"c","audio_track":2,
        "owned":{"connections":[{"id":3,"fps":30}],"guild_id":"g","channel_id":"c"}}"#;

    #[test]
    fn every_ownership_change_of_a_session_is_saved_at_once() {
        let (f, mut s) = picked(OWNED_3);
        assert_eq!(s.model.persisted(), Some(&ownership(3)));
        s.session(f.write(), |m| m.start(CANVAS)).unwrap();
        assert_eq!(owned(&f), Some(3), "nothing changed yet");
        s.session(f.write(), |m| m.claim(started(7).ownership));
        assert_eq!(owned(&f), Some(7), "saved before the output starts");
        s.session(f.write(), |m| m.started(Ok(started(7))));
        assert_eq!(owned(&f), Some(7));
        s.session(f.write(), |m| m.stop()).unwrap();
        s.session(f.write(), |m| m.stopped(Ok(())));
        assert_eq!(owned(&f), None, "a stopped session is no longer ours");
        assert_eq!(f.track(), Some(2), "other settings kept");

        open(&mut s, &f);
        assert_eq!(
            s.model.persisted(),
            None,
            "reopening reads back what was saved"
        );
        assert_eq!(s.error(), None);
    }

    #[test]
    fn an_ownership_change_that_could_not_be_saved_wins_over_a_stale_file_on_reopen() {
        let (mut f, mut s) = picked(OWNED_3);
        s.session(f.write(), |m| m.start(CANVAS)).unwrap();
        s.session(f.write(), |m| m.claim(started(7).ownership));
        s.session(f.write(), |m| m.started(Ok(started(7))));
        f.writable = false;
        s.session(f.write(), |m| m.stop()).unwrap();
        s.session(f.write(), |m| m.stopped(Ok(())));
        assert_eq!(s.error(), Some(&failed()));
        *f.text.borrow_mut() = Some(OWNED_3.into());

        open(&mut s, &f);
        assert_eq!(
            s.model.persisted(),
            None,
            "unsaved, so the stale 3 is not read back"
        );
        assert_eq!(s.error(), Some(&failed()));
        f.writable = true;
        open(&mut s, &f);
        assert_eq!(owned(&f), None, "the retry saves it");
        assert_eq!(s.model.persisted(), None);
    }

    #[test]
    fn an_ended_session_keeps_its_ownership_saved_and_a_blocked_file_keeps_it_unsaved() {
        let (f, mut s) = picked(OWNED_3);
        s.session(f.write(), |m| m.start(CANVAS)).unwrap();
        s.session(f.write(), |m| m.claim(started(7).ownership));
        s.session(f.write(), |m| m.started(Ok(started(7))));
        s.session(f.write(), |m| m.ended(Some("lost".into())));
        assert_eq!(owned(&f), Some(7), "serve may still run it");

        *f.text.borrow_mut() = Some("garbage".into());
        open(&mut s, &f);
        s.session(|_| panic!("blocked"), |m| m.start(CANVAS))
            .unwrap();
        s.session(|_| panic!("blocked"), |m| m.claim(None));
        assert_eq!(f.text.borrow().as_deref(), Some("garbage"));
        *f.text.borrow_mut() = Some(OWNED_3.into());
        open(&mut s, &f);
        assert_eq!(s.model.persisted(), None, "the starting session's own");
        assert_eq!(owned(&f), None, "and written once the file is usable");
    }
}
