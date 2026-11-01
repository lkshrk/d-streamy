//! What the settings dialog does with the module config: when to read it, when to write it, and what it shows.

use serde_json::Value;

use crate::dock::{DockModel, DockSettings, track_name};

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ConfigError {
    /// OBS gave the module no config folder.
    NoFolder,
    /// The file exists but is not a settings object; it is never overwritten.
    Corrupt {
        path: String,
        detail: String,
    },
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
            Self::Failed(reason) => format!("{reason}; {track} is used until OBS quits"),
        }
    }
}

/// A config file's contents, and where it was read.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Saved {
    pub path: String,
    pub text: String,
}

/// The settings JSON as saved, or why it can't be used.
pub fn parse(text: &str) -> Result<DockSettings, String> {
    match serde_json::from_str::<Value>(text) {
        Ok(Value::Object(_)) => Ok(DockSettings::from_json(text)),
        Ok(_) => Err("not an object".into()),
        Err(e) => Err(e.to_string()),
    }
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

    fn corrupt(&self) -> bool {
        matches!(self.error, Some(ConfigError::Corrupt { .. }))
    }

    /// A dialog opens: unsaved settings are retried rather than replaced; otherwise the file is read.
    pub fn open(
        &mut self,
        read: impl FnOnce() -> Result<Option<Saved>, ConfigError>,
        write: impl FnOnce(&str) -> Result<(), ConfigError>,
    ) {
        if self.unsaved && !self.corrupt() {
            self.save(write);
            return;
        }
        match read() {
            Ok(Some(Saved { path, text })) => match parse(&text) {
                Ok(settings) => {
                    self.model.reload(settings);
                    self.error = None;
                    self.unsaved = false;
                }
                Err(detail) => self.error = Some(ConfigError::Corrupt { path, detail }),
            },
            Ok(None) if self.unsaved => self.save(write),
            Ok(None) => self.error = None,
            Err(e) => self.error = Some(e),
        }
    }

    /// The dialog picked a track. False when out of range; a corrupt file is left alone.
    pub fn select_audio_track(
        &mut self,
        track: u8,
        write: impl FnOnce(&str) -> Result<(), ConfigError>,
    ) -> bool {
        if !self.model.select_audio_track(track) {
            return false;
        }
        if self.corrupt() {
            self.unsaved = true;
        } else {
            self.save(write);
        }
        true
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
                    text,
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
        s.open(|| Err(failed()), |_| unreachable!("nothing to save"));
        assert_eq!(s.model.settings().audio_track, 3);
        assert_eq!(s.error(), Some(&failed()));
    }
}
