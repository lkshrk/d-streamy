use std::fs;
use std::io::{self, Read, Write};
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::accounts::{Accounts, ResolvedToken};
use crate::crop::CropRect;
use crate::stream_config::{InvalidStreamConfig, StreamConfig};
use crate::target::CaptureTarget;

/// Environment variable whose token wins over every saved account (ADR 0005).
pub const TOKEN_ENV: &str = "DSTREAMY_TOKEN";

/// Why the config directory, `config.toml`, a token or an account could not be used.
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
    #[error(
        "invalid account name {0:?}: use up to 32 lower-case letters, digits, _, . or -, starting with a letter, digit or _"
    )]
    InvalidAccountName(String),
    #[error("unknown account {name:?}; {}", available_accounts(available))]
    UnknownAccount {
        name: String,
        available: Vec<String>,
    },
    #[error("account {0:?} has no saved token: run `dstreamy login --account {0}`")]
    AccountWithoutToken(String),
    #[error(
        "an account can't be chosen while {TOKEN_ENV} is set, because that token wins: unset it to use the saved accounts"
    )]
    AccountWithEnvToken,
    #[error("moving the token into an account: {0}")]
    Migration(String),
    #[error("another dstreamy is changing the saved accounts ({}); try again", .0.display())]
    StoreBusy(PathBuf),
}

fn available_accounts(names: &[String]) -> String {
    if names.is_empty() {
        "no account is saved: run `dstreamy login`".to_owned()
    } else {
        format!(
            "saved accounts: {} (see `dstreamy account list`)",
            names.join(", ")
        )
    }
}

pub(crate) fn io_err(path: &Path) -> impl FnOnce(io::Error) -> ConfigError + '_ {
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
    /// The audio device to capture; unset captures what plays on the default output.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub audio_source: Option<String>,
    #[serde(skip_serializing_if = "ServeConfig::is_default")]
    pub serve: ServeConfig,
}

/// `[serve]`: how `dstreamy serve` treats an external producer (ADR 0004).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct ServeConfig {
    pub grace_secs: u64,
    pub idle: IdleSetting,
    pub stall_ms: u64,
}

impl ServeConfig {
    fn is_default(&self) -> bool {
        *self == Self::default()
    }
}

impl Default for ServeConfig {
    fn default() -> Self {
        Self {
            grace_secs: 30,
            idle: IdleSetting::Documented,
            stall_ms: 2000,
        }
    }
}

/// `[serve] idle`: what Discord gets after a producer detached (ADR 0004); maps onto the media crate's `IdleMode`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum IdleSetting {
    #[default]
    Documented,
    None,
    Continuous,
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
        config.validate().map_err(|source| ConfigError::Invalid {
            path: path.to_owned(),
            source,
        })?;
        Ok(config)
    }

    /// Written owner-only because a portal restore token may be stored in it.
    pub fn save(&self, path: &Path) -> Result<(), ConfigError> {
        self.validate().map_err(|source| ConfigError::Invalid {
            path: path.to_owned(),
            source,
        })?;
        let text = toml::to_string_pretty(self)?;
        write_private(path, text.as_bytes())
    }

    pub fn validate(&self) -> Result<(), InvalidStreamConfig> {
        self.stream.validate()?;
        if let Some(crop) = self.crop {
            crop.validate()?;
        }
        Ok(())
    }
}

/// Locations of the config file and the saved accounts.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ConfigPaths {
    pub dir: PathBuf,
    pub config_file: PathBuf,
    /// The single token of builds before saved accounts; moved into an account on first use.
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
            dir: dir.to_owned(),
            config_file: dir.join("config.toml"),
            token_file: dir.join("token"),
        }
    }

    pub fn accounts(&self) -> Accounts {
        Accounts::new(&self.dir)
    }

    /// The token and its account: `DSTREAMY_TOKEN`, else `account`, else the default account; `None` if none is saved.
    pub fn resolve_token(
        &self,
        account: Option<&str>,
    ) -> Result<Option<ResolvedToken>, ConfigError> {
        self.accounts()
            .resolve(account, std::env::var(TOKEN_ENV).ok().as_deref())
    }

    /// Saves the token into the default account, or into a new account named `default` when none is saved.
    pub fn save_token(&self, token: &str) -> Result<(), ConfigError> {
        let accounts = self.accounts();
        let name = accounts
            .default_name()?
            .unwrap_or_else(|| crate::accounts::FALLBACK_NAME.to_owned());
        accounts.save(&name, token, None)
    }
}

/// A token file's trimmed contents; `None` when it is missing or blank, an error when others may read it.
pub(crate) fn read_token_file(token_file: &Path) -> Result<Option<String>, ConfigError> {
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
pub(crate) fn write_private(path: &Path, contents: &[u8]) -> Result<(), ConfigError> {
    create_parent(path)?;
    let dir = match path.parent() {
        Some(dir) if !dir.as_os_str().is_empty() => dir,
        _ => Path::new("."),
    };
    let mut tmp = tempfile::NamedTempFile::new_in(dir).map_err(io_err(dir))?;
    tmp.write_all(contents)
        .and_then(|()| tmp.as_file().sync_all())
        .map_err(io_err(path))?;
    tmp.persist(path).map_err(|e| io_err(path)(e.error))?;
    #[cfg(unix)]
    fs::File::open(dir)
        .and_then(|d| d.sync_all())
        .map_err(io_err(dir))?;
    Ok(())
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
                require_e2ee: true,
            },
            target: Some(CaptureTarget::Portal {
                restore_token: Some("tok-123".into()),
            }),
            crop: Some(CropRect::new(10, 20, 800, 600)),
            audio_source: Some("pipeweaver_stream".into()),
            serve: ServeConfig {
                grace_secs: 10,
                idle: IdleSetting::None,
                stall_ms: 1500,
            },
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
        assert_eq!(config.serve, ServeConfig::default());
    }

    #[test]
    fn serve_table_parses_and_rejects_unknown_values() {
        let (_dir, paths) = paths();
        create_parent(&paths.config_file).unwrap();
        fs::write(
            &paths.config_file,
            "[serve]\ngrace_secs = 5\nidle = \"continuous\"\nstall_ms = 750\n",
        )
        .unwrap();
        let serve = Config::load(&paths.config_file).unwrap().serve;
        assert_eq!(
            serve,
            ServeConfig {
                grace_secs: 5,
                idle: IdleSetting::Continuous,
                stall_ms: 750,
            }
        );
        for text in [
            "[serve]\nidle = \"sometimes\"\n",
            "[serve]\nstall_ms = -1\n",
            "[serve]\nbogus = 1\n",
        ] {
            fs::write(&paths.config_file, text).unwrap();
            assert!(Config::load(&paths.config_file).is_err(), "{text}");
        }
    }

    #[test]
    fn invalid_config_is_rejected() {
        let (_dir, paths) = paths();
        create_parent(&paths.config_file).unwrap();
        for text in [
            "[stream]\nbitrate_kbps = 50000\n",
            "[stream]\nfps = 24\n",
            "[stream]\nbogus = 1\n",
            "[crop]\nx = 0\ny = 0\nwidth = 0\nheight = 10\n",
            "[crop]\nx = 4294967295\ny = 0\nwidth = 2\nheight = 10\n",
        ] {
            fs::write(&paths.config_file, text).unwrap();
            assert!(Config::load(&paths.config_file).is_err(), "{text}");
        }
    }

    #[test]
    fn save_token_fills_the_default_account() {
        let (_dir, paths) = paths();
        paths.save_token("first").unwrap();
        let accounts = paths.accounts();
        assert_eq!(accounts.names().unwrap(), ["default"]);
        accounts.save("other", "second", None).unwrap();
        accounts.set_default("other").unwrap();
        paths.save_token("third").unwrap();
        assert_eq!(
            accounts.load_token("other").unwrap().as_deref(),
            Some("third")
        );
        assert_eq!(
            accounts.load_token("default").unwrap().as_deref(),
            Some("first")
        );
        assert!(!paths.token_file.exists());
    }

    #[test]
    fn no_token_file_is_none() {
        let (_dir, paths) = paths();
        assert_eq!(read_token_file(&paths.token_file).unwrap(), None);
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
        let config = Config {
            crop: Some(CropRect::new(0, 0, 10, 0)),
            ..Config::default()
        };
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

        write_private(&paths.token_file, b"secret\n").unwrap();

        let mode = fs::metadata(&paths.token_file)
            .unwrap()
            .permissions()
            .mode()
            & 0o777;
        assert_eq!(mode, 0o600);
        assert_eq!(
            read_token_file(&paths.token_file).unwrap(),
            Some("secret".into())
        );
    }

    #[cfg(unix)]
    #[test]
    fn token_file_readable_by_others_is_rejected() {
        use std::os::unix::fs::PermissionsExt;
        let (_dir, paths) = paths();
        write_private(&paths.token_file, b"secret").unwrap();
        for mode in [0o644, 0o640, 0o604] {
            fs::set_permissions(&paths.token_file, fs::Permissions::from_mode(mode)).unwrap();
            match read_token_file(&paths.token_file) {
                Err(ConfigError::InsecureTokenFile { mode: m, .. }) => assert_eq!(m, mode),
                other => panic!("mode {mode:o}: {other:?}"),
            }
        }
        assert!(paths.accounts().resolve(None, Some("env")).is_ok());
    }
}
