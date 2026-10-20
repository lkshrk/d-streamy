use std::fs;
use std::io::{self, Read, Write};
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::crop::CropRect;
use crate::stream_config::{InvalidStreamConfig, StreamConfig};
use crate::target::CaptureTarget;

pub const TOKEN_ENV: &str = "DSTREAMY_TOKEN";

#[derive(Debug, thiserror::Error)]
pub enum ConfigError {
    #[error("no home directory to place the config in")]
    NoHomeDir,
    #[error("{}: {source}", path.display())]
    Io { path: PathBuf, source: io::Error },
    #[error("{}: {source}", path.display())]
    Parse {
        path: PathBuf,
        source: toml::de::Error,
    },
    #[error("serializing config: {0}")]
    Serialize(#[from] toml::ser::Error),
    #[error("{}: {source}", path.display())]
    Invalid {
        path: PathBuf,
        source: InvalidStreamConfig,
    },
    #[error(
        "token file {} is accessible by other users (mode {mode:o}); run chmod 600 on it",
        path.display()
    )]
    InsecureTokenFile { path: PathBuf, mode: u32 },
}

fn io_err(path: &Path) -> impl FnOnce(io::Error) -> ConfigError + '_ {
    move |source| ConfigError::Io {
        path: path.to_owned(),
        source,
    }
}

/// Persistent settings, stored as TOML.
#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Config {
    pub stream: StreamConfig,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub target: Option<CaptureTarget>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub crop: Option<CropRect>,
}

impl Config {
    /// A missing file yields the defaults.
    pub fn load(path: &Path) -> Result<Self, ConfigError> {
        let text = match fs::read_to_string(path) {
            Ok(text) => text,
            Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(Self::default()),
            Err(e) => return Err(io_err(path)(e)),
        };
        let config: Self = toml::from_str(&text).map_err(|source| ConfigError::Parse {
            path: path.to_owned(),
            source,
        })?;
        config
            .stream
            .validate()
            .map_err(|source| ConfigError::Invalid {
                path: path.to_owned(),
                source,
            })?;
        Ok(config)
    }

    /// Written owner-only because a portal restore token may be stored in it.
    pub fn save(&self, path: &Path) -> Result<(), ConfigError> {
        self.stream
            .validate()
            .map_err(|source| ConfigError::Invalid {
                path: path.to_owned(),
                source,
            })?;
        let text = toml::to_string_pretty(self)?;
        write_private(path, text.as_bytes())
    }
}

/// Locations of the config and token files.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ConfigPaths {
    pub config_file: PathBuf,
    pub token_file: PathBuf,
}

impl ConfigPaths {
    pub fn discover() -> Result<Self, ConfigError> {
        directories::ProjectDirs::from("me", "harke", "dstreamy")
            .map(|dirs| Self::in_dir(dirs.config_dir()))
            .ok_or(ConfigError::NoHomeDir)
    }

    pub fn in_dir(dir: &Path) -> Self {
        Self {
            config_file: dir.join("config.toml"),
            token_file: dir.join("token"),
        }
    }

    /// The token from `DSTREAMY_TOKEN`, else from the token file; `None` if neither is set.
    pub fn load_token(&self) -> Result<Option<String>, ConfigError> {
        resolve_token(std::env::var(TOKEN_ENV).ok().as_deref(), &self.token_file)
    }

    /// Writes the token file readable by the owner only.
    pub fn save_token(&self, token: &str) -> Result<(), ConfigError> {
        write_private(&self.token_file, format!("{}\n", token.trim()).as_bytes())
    }
}

fn resolve_token(env: Option<&str>, token_file: &Path) -> Result<Option<String>, ConfigError> {
    if let Some(token) = env.map(str::trim).filter(|t| !t.is_empty()) {
        return Ok(Some(token.to_owned()));
    }
    let mut file = match fs::File::open(token_file) {
        Ok(file) => file,
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(e) => return Err(io_err(token_file)(e)),
    };
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mode = file
            .metadata()
            .map_err(io_err(token_file))?
            .permissions()
            .mode()
            & 0o777;
        if mode & 0o077 != 0 {
            return Err(ConfigError::InsecureTokenFile {
                path: token_file.to_owned(),
                mode,
            });
        }
    }
    let mut text = String::new();
    file.read_to_string(&mut text).map_err(io_err(token_file))?;
    let token = text.trim();
    Ok((!token.is_empty()).then(|| token.to_owned()))
}

/// Atomically replaces `path` with an owner-only (0600 on unix) file.
fn write_private(path: &Path, contents: &[u8]) -> Result<(), ConfigError> {
    create_parent(path)?;
    let mut tmp_name = path.file_name().unwrap_or_default().to_owned();
    tmp_name.push(".tmp");
    let tmp = path.with_file_name(tmp_name);
    let mut options = fs::OpenOptions::new();
    options.write(true).create(true).truncate(true);
    #[cfg(unix)]
    std::os::unix::fs::OpenOptionsExt::mode(&mut options, 0o600);
    let mut file = options.open(&tmp).map_err(io_err(&tmp))?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        // mode() only applies on creation, so tighten a leftover temp file too.
        file.set_permissions(fs::Permissions::from_mode(0o600))
            .map_err(io_err(&tmp))?;
    }
    file.write_all(contents)
        .and_then(|()| file.sync_all())
        .map_err(io_err(&tmp))?;
    fs::rename(&tmp, path).map_err(io_err(path))
}

fn create_parent(path: &Path) -> Result<(), ConfigError> {
    match path.parent() {
        Some(dir) if !dir.as_os_str().is_empty() => fs::create_dir_all(dir).map_err(io_err(dir)),
        _ => Ok(()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::codec::Codec;
    use crate::stream_config::{Fps, Resolution};

    fn paths() -> (tempfile::TempDir, ConfigPaths) {
        let dir = tempfile::tempdir().unwrap();
        let paths = ConfigPaths::in_dir(&dir.path().join("dstreamy"));
        (dir, paths)
    }

    #[test]
    fn config_round_trip() {
        let (_dir, paths) = paths();
        let config = Config {
            stream: StreamConfig {
                resolution: Resolution::P900,
                fps: Fps::Fps60,
                bitrate_kbps: 8_500,
                codec: Codec::H265,
                audio_gain: 1.5,
            },
            target: Some(CaptureTarget::Portal {
                restore_token: Some("tok-123".into()),
            }),
            crop: Some(CropRect::new(10, 20, 800, 600)),
        };
        config.save(&paths.config_file).unwrap();
        assert_eq!(Config::load(&paths.config_file).unwrap(), config);
    }

    #[test]
    fn missing_config_file_yields_defaults() {
        let (_dir, paths) = paths();
        assert_eq!(Config::load(&paths.config_file).unwrap(), Config::default());
    }

    #[test]
    fn partial_config_fills_defaults() {
        let (_dir, paths) = paths();
        create_parent(&paths.config_file).unwrap();
        fs::write(&paths.config_file, "[stream]\nfps = 15\n").unwrap();
        let config = Config::load(&paths.config_file).unwrap();
        assert_eq!(config.stream.fps, Fps::Fps15);
        assert_eq!(config.stream.bitrate_kbps, 6_000);
        assert_eq!(config.target, None);
    }

    #[test]
    fn invalid_config_is_rejected() {
        let (_dir, paths) = paths();
        create_parent(&paths.config_file).unwrap();
        for text in [
            "[stream]\nbitrate_kbps = 50000\n",
            "[stream]\nfps = 24\n",
            "[stream]\nbogus = 1\n",
        ] {
            fs::write(&paths.config_file, text).unwrap();
            assert!(Config::load(&paths.config_file).is_err(), "{text}");
        }
    }

    #[test]
    fn env_token_takes_precedence_over_file() {
        let (_dir, paths) = paths();
        paths.save_token("from-file").unwrap();
        assert_eq!(
            resolve_token(Some(" from-env \n"), &paths.token_file).unwrap(),
            Some("from-env".into())
        );
        assert_eq!(
            resolve_token(Some(""), &paths.token_file).unwrap(),
            Some("from-file".into())
        );
        assert_eq!(
            resolve_token(None, &paths.token_file).unwrap(),
            Some("from-file".into())
        );
    }

    #[test]
    fn no_token_anywhere_is_none() {
        let (_dir, paths) = paths();
        assert_eq!(resolve_token(None, &paths.token_file).unwrap(), None);
    }

    #[test]
    fn save_rejects_invalid_stream_config() {
        let (_dir, paths) = paths();
        let mut config = Config::default();
        config.stream.audio_gain = f32::NAN;
        assert!(matches!(
            config.save(&paths.config_file),
            Err(ConfigError::Invalid { .. })
        ));
        assert!(!paths.config_file.exists());
    }

    #[cfg(unix)]
    #[test]
    fn config_file_is_written_owner_only_without_leftovers() {
        use std::os::unix::fs::PermissionsExt;
        let (dir, paths) = paths();
        Config::default().save(&paths.config_file).unwrap();
        let mode = fs::metadata(&paths.config_file)
            .unwrap()
            .permissions()
            .mode()
            & 0o777;
        assert_eq!(mode, 0o600);
        let entries = fs::read_dir(dir.path().join("dstreamy")).unwrap().count();
        assert_eq!(entries, 1);
    }

    #[cfg(unix)]
    #[test]
    fn token_file_is_written_owner_only() {
        use std::os::unix::fs::PermissionsExt;
        let (_dir, paths) = paths();
        create_parent(&paths.token_file).unwrap();
        fs::write(&paths.token_file, "old").unwrap();
        fs::set_permissions(&paths.token_file, fs::Permissions::from_mode(0o644)).unwrap();

        paths.save_token("secret").unwrap();

        let mode = fs::metadata(&paths.token_file)
            .unwrap()
            .permissions()
            .mode()
            & 0o777;
        assert_eq!(mode, 0o600);
        assert_eq!(
            resolve_token(None, &paths.token_file).unwrap(),
            Some("secret".into())
        );
    }

    #[cfg(unix)]
    #[test]
    fn token_file_readable_by_others_is_rejected() {
        use std::os::unix::fs::PermissionsExt;
        let (_dir, paths) = paths();
        paths.save_token("secret").unwrap();
        for mode in [0o644, 0o640, 0o604] {
            fs::set_permissions(&paths.token_file, fs::Permissions::from_mode(mode)).unwrap();
            match resolve_token(None, &paths.token_file) {
                Err(ConfigError::InsecureTokenFile { mode: m, .. }) => assert_eq!(m, mode),
                other => panic!("mode {mode:o}: {other:?}"),
            }
        }
        assert!(resolve_token(Some("env"), &paths.token_file).is_ok());
    }
}
