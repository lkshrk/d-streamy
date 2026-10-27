//! The module config: the dock's settings and the session ownership journal, kept between OBS runs.

use std::ffi::{CStr, CString};
use std::io;
use std::path::{Path, PathBuf};

use dstreamy_obs_producer::control::{Ownership, StartCertainty};
use dstreamy_obs_producer::dock::DockSettings;
use dstreamy_obs_sys as sys;
use serde_json::Value;

pub const VERSION: i64 = 1;

#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct ConfigSnapshot {
    pub settings: DockSettings,
    pub ownership_certainty: StartCertainty,
}

/// `config.json` in the module's OBS config folder, and the `config.json.bak` that `obs_data_save_json_safe` keeps.
pub struct ConfigStore {
    path: Result<PathBuf, String>,
    /// Set once a load finds the latest record unusable; nothing is saved for the rest of the run.
    recovery: Option<String>,
    loaded: bool,
}

enum Version {
    Absent,
    Valid(ConfigSnapshot),
    Corrupt(String),
    Future(i64),
}

impl ConfigStore {
    pub fn for_module(module: *mut sys::obs_module_t) -> Self {
        Self::with_path(module_path(module))
    }

    #[cfg(test)]
    pub fn at(path: PathBuf) -> Self {
        Self::with_path(Ok(path))
    }

    fn with_path(path: Result<PathBuf, String>) -> Self {
        Self {
            path: path.and_then(|p| create_parent(&p).map(|()| p)),
            recovery: None,
            loaded: false,
        }
    }

    /// The reason saving stays blocked, if a load left one.
    pub fn recovery(&self) -> Option<&str> {
        self.recovery.as_deref()
    }

    pub fn load(&mut self) -> Result<ConfigSnapshot, String> {
        self.loaded = true;
        let path = self
            .path
            .clone()
            .inspect_err(|e| self.recovery = Some(e.clone()))?;
        let backup = backup_path(&path);
        let (primary, older) = (inspect(&path), inspect(&backup));
        let shown = |name: &Path| {
            name.file_name()
                .map_or_else(String::new, |n| n.to_string_lossy().into_owned())
        };
        let (result, recovery) = match (primary, older) {
            (Version::Future(v), _) => {
                let e = format!(
                    "{} was written by a newer D-Streamy (config version {v}); it is left untouched and nothing is saved",
                    path.display()
                );
                (Err(e.clone()), Some(e))
            }
            (Version::Valid(s), Version::Future(v)) => (
                Ok(s),
                Some(format!(
                    "{} is from a newer D-Streamy (config version {v}); nothing is saved so it is not replaced",
                    backup.display()
                )),
            ),
            (Version::Valid(s), _) => (Ok(s), None),
            (Version::Absent, Version::Absent) => (Ok(ConfigSnapshot::default()), None),
            (latest, Version::Valid(s)) => {
                let why = match latest {
                    Version::Corrupt(e) => e,
                    _ => format!("{} is missing", shown(&path)),
                };
                (
                    Ok(s),
                    Some(format!(
                        "{why}; showing the settings from {}, which may be older, and saving nothing until {} is repaired",
                        shown(&backup),
                        path.display()
                    )),
                )
            }
            (latest, older) => {
                let describe = |v: Version, p: &Path| match v {
                    Version::Corrupt(e) => e,
                    Version::Future(n) => format!(
                        "{} is from a newer D-Streamy (config version {n})",
                        shown(p)
                    ),
                    Version::Absent => format!("{} is missing", shown(p)),
                    Version::Valid(_) => unreachable!("handled above"),
                };
                let e = format!(
                    "{}; {}; nothing is saved until {} is repaired",
                    describe(latest, &path),
                    describe(older, &backup),
                    path.display()
                );
                (Err(e.clone()), Some(e))
            }
        };
        self.recovery = recovery;
        result
    }

    pub fn check_writable(&self) -> Result<(), String> {
        if !self.loaded {
            return Err("the D-Streamy config has not been read yet".into());
        }
        if let Some(e) = &self.recovery {
            return Err(e.clone());
        }
        let path = self.path.as_ref().map_err(Clone::clone)?;
        let backup = backup_path(path);
        match inspect(path) {
            Version::Valid(_) => {}
            Version::Absent if matches!(inspect(&backup), Version::Absent) => return Ok(()),
            Version::Absent => {
                return Err(format!(
                    "{} disappeared while {} exists; nothing is saved",
                    path.display(),
                    backup.display()
                ));
            }
            Version::Corrupt(e) => return Err(format!("{e}; nothing is saved")),
            Version::Future(v) => {
                return Err(format!(
                    "{} is now from a newer D-Streamy (config version {v}); nothing is saved",
                    path.display()
                ));
            }
        }
        match inspect(&backup) {
            Version::Future(v) => Err(format!(
                "{} is from a newer D-Streamy (config version {v}); nothing is saved so it is not replaced",
                backup.display()
            )),
            _ => Ok(()),
        }
    }

    pub fn save(&self, snapshot: &ConfigSnapshot) -> Result<(), String> {
        self.check_writable()?;
        let path = self.path.as_ref().map_err(Clone::clone)?;
        let json = CString::new(encode(snapshot)).map_err(|e| e.to_string())?;
        let file = CString::new(path.as_os_str().as_encoded_bytes())
            .map_err(|_| format!("{} is not a usable path", path.display()))?;
        // SAFETY: both strings are NUL-terminated and outlive the calls; the data is released before returning.
        let saved = unsafe {
            let data = sys::obs_data_create_from_json(json.as_ptr());
            if data.is_null() {
                return Err("libobs could not read the config it was given".into());
            }
            let saved =
                sys::obs_data_save_json_safe(data, file.as_ptr(), c"tmp".as_ptr(), c"bak".as_ptr());
            sys::obs_data_release(data);
            saved
        };
        if saved {
            Ok(())
        } else {
            Err(format!("cannot write {}", path.display()))
        }
    }
}

fn module_path(module: *mut sys::obs_module_t) -> Result<PathBuf, String> {
    // SAFETY: libobs returns a bmalloc'd copy or NULL; it is copied and freed here.
    unsafe {
        let raw = sys::obs_module_get_config_path(module, c"config.json".as_ptr());
        if raw.is_null() {
            return Err("OBS has no config folder for D-Streamy".into());
        }
        let path = CStr::from_ptr(raw)
            .to_str()
            .map(PathBuf::from)
            .map_err(|_| "the D-Streamy config folder is not valid UTF-8".to_owned());
        sys::bfree(raw.cast());
        path
    }
}

fn create_parent(path: &Path) -> Result<(), String> {
    let parent = path
        .parent()
        .filter(|p| !p.as_os_str().is_empty())
        .ok_or_else(|| format!("{} has no folder", path.display()))?;
    let c = CString::new(parent.as_os_str().as_encoded_bytes())
        .map_err(|_| format!("{} is not a usable path", parent.display()))?;
    // SAFETY: NUL-terminated path that outlives the call.
    let r = unsafe { sys::os_mkdirs(c.as_ptr()) };
    if r == sys::MKDIR_SUCCESS as i32 || r == sys::MKDIR_EXISTS as i32 {
        Ok(())
    } else {
        Err(format!(
            "cannot create the D-Streamy config folder {}",
            parent.display()
        ))
    }
}

fn backup_path(path: &Path) -> PathBuf {
    let mut name = path.as_os_str().to_owned();
    name.push(".bak");
    PathBuf::from(name)
}

fn inspect(path: &Path) -> Version {
    let name = path.display();
    let bytes = match std::fs::read(path) {
        Ok(b) => b,
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Version::Absent,
        Err(e) => return Version::Corrupt(format!("cannot read {name}: {e}")),
    };
    let Ok(text) = std::str::from_utf8(&bytes) else {
        return Version::Corrupt(format!("{name} is not UTF-8"));
    };
    let Ok(Value::Object(map)) = serde_json::from_str::<Value>(text) else {
        return Version::Corrupt(format!("{name} is not a JSON object"));
    };
    match map.get("version").and_then(Value::as_i64) {
        Some(VERSION) => {}
        Some(v) if v > VERSION => return Version::Future(v),
        _ => return Version::Corrupt(format!("{name} has no supported version")),
    }
    let owned = match map.get("owned") {
        None | Some(Value::Null) => None,
        Some(Value::String(text)) => match serde_json::from_str::<Ownership>(text) {
            Ok(o) if !o.connections.is_empty() => Some(o),
            _ => return Version::Corrupt(format!("{name} has an unreadable session record")),
        },
        Some(_) => return Version::Corrupt(format!("{name} has an unreadable session record")),
    };
    let ownership_certainty = match map.get("ownership_certainty") {
        _ if owned.as_ref().is_some_and(|o| o.instance.is_none()) => StartCertainty::Pending,
        None => StartCertainty::Pending,
        Some(v) => match serde_json::from_value(v.clone()) {
            Ok(c) => c,
            Err(_) => {
                return Version::Corrupt(format!("{name} has an unknown ownership_certainty"));
            }
        },
    };
    Version::Valid(ConfigSnapshot {
        settings: DockSettings {
            owned,
            ..DockSettings::from_json(text)
        },
        ownership_certainty,
    })
}

fn encode(snapshot: &ConfigSnapshot) -> String {
    let mut v = serde_json::to_value(&snapshot.settings).expect("settings serialize");
    v["version"] = VERSION.into();
    v["owned"] = match &snapshot.settings.owned {
        Some(o) => serde_json::to_string(o)
            .expect("ownership serializes")
            .into(),
        None => Value::Null,
    };
    v["ownership_certainty"] =
        serde_json::to_value(snapshot.ownership_certainty).expect("certainty serializes");
    v.to_string()
}

#[cfg(test)]
mod tests {
    use std::fs;
    use std::os::unix::fs::PermissionsExt;
    use std::path::Path;

    use dstreamy_obs_producer::control::{Candidate, Instance, Ownership};
    use dstreamy_obs_producer::dock::VideoMode;
    use serde_json::{Value, json};

    use super::*;
    use crate::testdir;

    const INSTANCE: Instance = Instance {
        dev: u64::MAX,
        ino: 9_007_199_254_740_993,
        mtime: -5,
        mtime_nsec: 999_999_999,
    };

    fn owner(ids: &[u64], instance: Option<Instance>) -> Ownership {
        Ownership {
            connections: ids
                .iter()
                .map(|&id| Candidate {
                    id,
                    fps: (id % 2 == 0).then_some(30 + (id % 7) as u32),
                })
                .collect(),
            guild_id: "g".into(),
            channel_id: "c".into(),
            instance,
        }
    }

    fn snapshot(owned: Option<Ownership>, certainty: StartCertainty) -> ConfigSnapshot {
        ConfigSnapshot {
            settings: DockSettings {
                guild_id: Some("g".into()),
                channel_id: Some("c".into()),
                grace_secs: Some(90),
                mode: VideoMode::Share,
                video_encoder: "obs_nvenc".into(),
                bitrate_kbps: 12_000,
                share_fallback: false,
                start_with_stream: true,
                owned,
            },
            ownership_certainty: certainty,
        }
    }

    fn file(dir: &Path, json: &Value) -> PathBuf {
        let path = dir.join("config.json");
        fs::write(&path, json.to_string()).unwrap();
        path
    }

    fn v1(owned: Option<&Ownership>, certainty: Option<&str>) -> Value {
        let mut v = json!({"version": 1, "guild_id": "g", "channel_id": "c", "bitrate_kbps": 8000});
        if let Some(o) = owned {
            v["owned"] = serde_json::to_string(o).unwrap().into();
        }
        if let Some(c) = certainty {
            v["ownership_certainty"] = c.into();
        }
        v
    }

    fn bytes(path: &Path) -> Vec<u8> {
        fs::read(path).unwrap()
    }

    fn backup(primary: &Path) -> PathBuf {
        primary.with_extension("json.bak")
    }

    fn loaded(path: &Path) -> (ConfigStore, Result<ConfigSnapshot, String>) {
        let mut store = ConfigStore::at(path.to_owned());
        let result = store.load();
        (store, result)
    }

    #[test]
    fn a_fresh_directory_round_trips_every_ownership_field() {
        let dir = testdir::fresh("config-roundtrip");
        let path = dir.join("nested").join("deeper").join("config.json");
        let (store, first) = loaded(&path);
        assert_eq!(first, Ok(ConfigSnapshot::default()));
        assert!(path.parent().unwrap().is_dir(), "parent created");
        store.check_writable().unwrap();

        for certainty in [StartCertainty::Pending, StartCertainty::Confirmed] {
            let saved = snapshot(
                Some(owner(&[u64::MAX, 2, 3, 4, 5, 6, 7, 8], Some(INSTANCE))),
                certainty,
            );
            store.save(&saved).unwrap();
            assert_eq!(loaded(&path).1, Ok(saved));
        }
        let unknown = snapshot(Some(owner(&[1], None)), StartCertainty::Pending);
        store.save(&unknown).unwrap();
        assert_eq!(loaded(&path).1, Ok(unknown));
        let empty = snapshot(None, StartCertainty::Confirmed);
        store.save(&empty).unwrap();
        assert_eq!(loaded(&path).1, Ok(empty));
    }

    #[test]
    fn the_file_is_version_one_with_the_ownership_as_json_text() {
        let dir = testdir::fresh("config-format");
        let path = dir.join("config.json");
        let (store, _) = loaded(&path);
        let o = owner(&[7], Some(INSTANCE));
        store
            .save(&snapshot(Some(o.clone()), StartCertainty::Pending))
            .unwrap();
        let v: Value = serde_json::from_slice(&bytes(&path)).unwrap();
        assert_eq!(v["version"], 1);
        assert_eq!(v["ownership_certainty"], "pending");
        assert_eq!(v["mode"], "share");
        assert_eq!(v["bitrate_kbps"], 12_000);
        let text = v["owned"].as_str().expect("owned is JSON text");
        assert_eq!(serde_json::from_str::<Ownership>(text).unwrap(), o);
    }

    #[test]
    fn saving_again_keeps_the_previous_file_as_the_backup() {
        let dir = testdir::fresh("config-backup");
        let path = dir.join("config.json");
        let (store, _) = loaded(&path);
        store
            .save(&snapshot(None, StartCertainty::Confirmed))
            .unwrap();
        let first = bytes(&path);
        store
            .save(&snapshot(
                Some(owner(&[1], Some(INSTANCE))),
                StartCertainty::Pending,
            ))
            .unwrap();
        assert_eq!(bytes(&backup(&path)), first);
        assert!(!path.with_extension("json.tmp").exists());
    }

    #[test]
    fn a_missing_certainty_with_an_owner_reads_as_pending() {
        let dir = testdir::fresh("config-missing-certainty");
        let o = owner(&[4], Some(INSTANCE));
        let path = file(&dir, &v1(Some(&o), None));
        let (store, result) = loaded(&path);
        let snap = result.unwrap();
        assert_eq!(snap.settings.owned, Some(o));
        assert_eq!(snap.ownership_certainty, StartCertainty::Pending);
        store.check_writable().unwrap();
    }

    #[test]
    fn an_owner_of_an_unknown_service_run_always_reads_as_pending() {
        let dir = testdir::fresh("config-unknown-instance");
        let o = owner(&[4], None);
        let path = file(&dir, &v1(Some(&o), Some("confirmed")));
        let snap = loaded(&path).1.unwrap();
        assert_eq!(snap.settings.owned, Some(o));
        assert_eq!(snap.ownership_certainty, StartCertainty::Pending);
    }

    #[test]
    fn ordinary_fields_keep_their_tolerant_defaults() {
        let dir = testdir::fresh("config-tolerant");
        let path = file(
            &dir,
            &json!({"version": 1, "bitrate_kbps": "fast", "mode": 3, "guild_id": "g"}),
        );
        let (store, result) = loaded(&path);
        let snap = result.unwrap();
        assert_eq!(
            snap.settings.bitrate_kbps,
            DockSettings::default().bitrate_kbps
        );
        assert_eq!(snap.settings.mode, VideoMode::Own);
        assert_eq!(snap.settings.guild_id.as_deref(), Some("g"));
        store.check_writable().unwrap();
    }

    #[test]
    fn unreadable_ownership_is_a_recovery_error_not_none() {
        for owned in [
            json!("not json"),
            json!({"connections": []}),
            json!(5),
            json!("{}"),
        ] {
            let dir = testdir::fresh("config-bad-owned");
            let mut v = v1(None, Some("confirmed"));
            v["owned"] = owned.clone();
            let path = file(&dir, &v);
            let before = bytes(&path);
            let (store, result) = loaded(&path);
            assert!(result.is_err(), "{owned}: {result:?}");
            assert!(store.check_writable().is_err(), "{owned}");
            assert!(store.save(&ConfigSnapshot::default()).is_err(), "{owned}");
            assert_eq!(bytes(&path), before);
        }
    }

    #[test]
    fn an_unknown_certainty_is_a_recovery_error() {
        let dir = testdir::fresh("config-bad-certainty");
        let path = file(&dir, &v1(Some(&owner(&[1], Some(INSTANCE))), Some("maybe")));
        let (store, result) = loaded(&path);
        assert!(result.is_err());
        assert!(store.check_writable().is_err());
    }

    #[test]
    fn a_corrupt_primary_shows_the_backup_but_saves_nothing() {
        let dir = testdir::fresh("config-corrupt-primary");
        let path = dir.join("config.json");
        let a = owner(&[3], Some(INSTANCE));
        fs::write(&path, b"{\"version\": 1, \"owned\": \"{\\\"conn").unwrap();
        fs::write(backup(&path), v1(Some(&a), Some("pending")).to_string()).unwrap();
        let (primary, bak) = (bytes(&path), bytes(&backup(&path)));

        let (store, result) = loaded(&path);
        let snap = result.expect("backup settings are shown");
        assert_eq!(snap.settings.owned, Some(a));
        assert_eq!(snap.settings.guild_id.as_deref(), Some("g"));
        let error = store.check_writable().unwrap_err();
        assert!(error.contains("config.json"), "{error}");
        assert_eq!(store.save(&ConfigSnapshot::default()), Err(error.clone()));
        assert_eq!(store.check_writable(), Err(error));
        assert_eq!((bytes(&path), bytes(&backup(&path))), (primary, bak));
    }

    #[test]
    fn a_missing_primary_with_a_backup_is_also_display_only() {
        let dir = testdir::fresh("config-missing-primary");
        let path = dir.join("config.json");
        fs::write(backup(&path), v1(None, None).to_string()).unwrap();
        let (store, result) = loaded(&path);
        assert!(result.is_ok());
        assert!(store.check_writable().is_err());
        assert!(store.save(&ConfigSnapshot::default()).is_err());
        assert!(!path.exists());
    }

    #[test]
    fn neither_file_valid_blocks_everything() {
        let dir = testdir::fresh("config-both-corrupt");
        let path = dir.join("config.json");
        fs::write(&path, b"[]").unwrap();
        fs::write(backup(&path), b"{").unwrap();
        let (store, result) = loaded(&path);
        assert!(result.is_err());
        assert!(store.check_writable().is_err());
        assert!(store.save(&ConfigSnapshot::default()).is_err());
        assert_eq!(bytes(&path), b"[]");
        assert_eq!(bytes(&backup(&path)), b"{");

        let dir = testdir::fresh("config-corrupt-no-backup");
        let path = dir.join("config.json");
        fs::write(&path, b"nope").unwrap();
        let (store, result) = loaded(&path);
        assert!(result.is_err());
        assert!(store.check_writable().is_err());
        assert!(!backup(&path).exists());
    }

    #[test]
    fn a_future_primary_blocks_without_falling_back() {
        let dir = testdir::fresh("config-future-primary");
        let path = dir.join("config.json");
        let future = json!({"version": 2, "guild_id": "new", "something": [1, 2]}).to_string();
        fs::write(&path, &future).unwrap();
        fs::write(backup(&path), v1(None, None).to_string()).unwrap();
        let bak = bytes(&backup(&path));
        let (store, result) = loaded(&path);
        let error = result.unwrap_err();
        assert!(error.contains("newer"), "{error}");
        assert!(store.check_writable().is_err());
        assert!(store.save(&ConfigSnapshot::default()).is_err());
        assert_eq!(bytes(&path), future.as_bytes());
        assert_eq!(bytes(&backup(&path)), bak);
    }

    #[test]
    fn a_future_backup_is_never_used_or_replaced() {
        let dir = testdir::fresh("config-future-backup");
        let path = file(&dir, &v1(None, None));
        let future = json!({"version": 7}).to_string();
        fs::write(backup(&path), &future).unwrap();
        let primary = bytes(&path);
        let (store, result) = loaded(&path);
        assert_eq!(result.unwrap().settings.guild_id.as_deref(), Some("g"));
        assert!(store.check_writable().is_err());
        assert!(store.save(&ConfigSnapshot::default()).is_err());
        assert_eq!(bytes(&path), primary);
        assert_eq!(bytes(&backup(&path)), future.as_bytes());

        let dir = testdir::fresh("config-future-backup-only");
        let path = dir.join("config.json");
        fs::write(backup(&path), &future).unwrap();
        let (store, result) = loaded(&path);
        assert!(result.is_err());
        assert!(store.check_writable().is_err());
    }

    #[test]
    fn saving_inspects_both_files_again() {
        let dir = testdir::fresh("config-reinspect");
        let path = dir.join("config.json");
        let (store, _) = loaded(&path);
        store
            .save(&snapshot(None, StartCertainty::Confirmed))
            .unwrap();
        let future = json!({"version": 2}).to_string();
        fs::write(&path, &future).unwrap();
        assert!(store.check_writable().is_err());
        assert!(store.save(&ConfigSnapshot::default()).is_err());
        assert_eq!(bytes(&path), future.as_bytes());

        fs::write(&path, v1(None, None).to_string()).unwrap();
        fs::write(backup(&path), &future).unwrap();
        assert!(store.save(&ConfigSnapshot::default()).is_err());
        assert_eq!(bytes(&backup(&path)), future.as_bytes());
    }

    #[test]
    fn a_folder_that_cannot_be_created_blocks_everything() {
        let dir = testdir::fresh("config-mkdir");
        fs::write(dir.join("blocker"), b"file").unwrap();
        let path = dir.join("blocker").join("sub").join("config.json");
        let (store, result) = loaded(&path);
        assert!(result.is_err());
        assert!(store.check_writable().is_err());
        assert!(store.save(&ConfigSnapshot::default()).is_err());
    }

    #[test]
    fn a_failed_save_is_reported() {
        let dir = testdir::fresh("config-readonly");
        let sub = dir.join("ro");
        let path = sub.join("config.json");
        let (store, _) = loaded(&path);
        fs::set_permissions(&sub, fs::Permissions::from_mode(0o555)).unwrap();
        let result = store.save(&ConfigSnapshot::default());
        fs::set_permissions(&sub, fs::Permissions::from_mode(0o755)).unwrap();
        assert!(result.is_err());
        assert!(!path.exists());
    }

    #[test]
    fn checking_before_loading_refuses() {
        let dir = testdir::fresh("config-unloaded");
        let store = ConfigStore::at(dir.join("config.json"));
        assert!(store.check_writable().is_err());
    }
}
