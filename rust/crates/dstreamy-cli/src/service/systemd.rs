//! A systemd user unit for `dstreamy serve`.

use std::ffi::OsString;
use std::path::{Path, PathBuf};

use super::{Backend, ServiceError, Step, Supervision, output, shell_words};

pub const UNIT_NAME: &str = "dstreamy.service";

pub struct Systemd {
    systemctl: PathBuf,
    unit_path: PathBuf,
    unit: String,
}

impl Systemd {
    /// Places the unit where the user manager looks first for user configuration.
    pub fn new(systemctl: PathBuf, exe: &Path) -> Result<Self, ServiceError> {
        Self::with_unit_dir(systemctl, exe, &user_unit_dir()?)
    }

    pub fn with_unit_dir(systemctl: PathBuf, exe: &Path, dir: &Path) -> Result<Self, ServiceError> {
        Ok(Self {
            systemctl,
            unit_path: dir.join(UNIT_NAME),
            unit: unit(exe)?,
        })
    }

    fn systemctl(&self, args: &[&str]) -> Vec<OsString> {
        let mut argv = vec![self.systemctl.clone().into_os_string(), "--user".into()];
        argv.extend(args.iter().map(OsString::from));
        argv
    }
}

impl Backend for Systemd {
    fn name(&self) -> &str {
        UNIT_NAME
    }

    fn definition_path(&self) -> &Path {
        &self.unit_path
    }

    fn definition(&self) -> &str {
        &self.unit
    }

    fn install(&self) -> Vec<Step> {
        vec![
            Step::Write {
                path: self.unit_path.clone(),
                contents: self.unit.clone(),
            },
            Step::Run(self.systemctl(&["daemon-reload"])),
            Step::Run(self.systemctl(&["enable", "--now", UNIT_NAME])),
        ]
    }

    fn uninstall(&self) -> Vec<Step> {
        vec![
            Step::Run(self.systemctl(&["disable", "--now", UNIT_NAME])),
            Step::Remove(self.unit_path.clone()),
            Step::Run(self.systemctl(&["daemon-reload"])),
        ]
    }

    fn query(&self) -> Result<Supervision, ServiceError> {
        let argv = self.systemctl(&[
            "show",
            "--property=LoadState,ActiveState,SubState,UnitFileState,MainPID",
            UNIT_NAME,
        ]);
        let output = output(&argv)?;
        if !output.status.success() {
            return Err(ServiceError::Failed {
                command: shell_words(&argv),
                status: output.status,
            });
        }
        Ok(supervision(&String::from_utf8_lossy(&output.stdout)))
    }
}

fn supervision(show: &str) -> Supervision {
    let property = |name: &str| {
        show.lines()
            .find_map(|line| line.strip_prefix(name)?.strip_prefix('='))
            .unwrap_or("")
    };
    let (load, active, sub, file, pid) = (
        property("LoadState"),
        property("ActiveState"),
        property("SubState"),
        property("UnitFileState"),
        property("MainPID"),
    );
    let running = active == "active" && sub == "running";
    let mut detail = format!("{load}, {active} ({sub})");
    if !file.is_empty() {
        detail.push_str(&format!(", {file}"));
    }
    if running && pid != "0" && !pid.is_empty() {
        detail.push_str(&format!(", pid {pid}"));
    }
    Supervision {
        enabled: file == "enabled",
        running,
        detail,
    }
}

/// `$XDG_CONFIG_HOME/systemd/user`, else `$HOME/.config/systemd/user`, as the user manager resolves it.
fn user_unit_dir() -> Result<PathBuf, ServiceError> {
    let config = match std::env::var_os("XDG_CONFIG_HOME") {
        Some(dir) => PathBuf::from(dir),
        None => std::env::home_dir()
            .ok_or(ServiceError::NoHome)?
            .join(".config"),
    };
    if !config.is_absolute() {
        return Err(ServiceError::RelativeConfigDir(config));
    }
    Ok(config.join("systemd/user"))
}

pub fn unit(exe: &Path) -> Result<String, ServiceError> {
    Ok(format!(
        "[Unit]\n\
         Description=D-Streamy service\n\
         \n\
         [Service]\n\
         ExecStart={} serve\n\
         Restart=on-failure\n\
         RestartSec=2\n\
         \n\
         [Install]\n\
         WantedBy=default.target\n",
        exec_path(exe)?
    ))
}

/// `exe` as the first word of `ExecStart=`, which systemd unquotes, then resolves `%` specifiers in.
fn exec_path(exe: &Path) -> Result<String, ServiceError> {
    let text = exe
        .to_str()
        .ok_or_else(|| ServiceError::NotUtf8(exe.to_owned()))?;
    if let Some(found) = text
        .chars()
        .find(|&c| c.is_control() || matches!(c, '"' | '\'' | '\\'))
    {
        return Err(ServiceError::UnsupportedPath {
            path: exe.to_owned(),
            found,
        });
    }
    // The executable is never variable-expanded, but argv[0] would be.
    let prefix = if text.contains('$') { ":" } else { "" };
    let word = format!("{prefix}{}", text.replace('%', "%%"));
    Ok(if text.contains(' ') {
        format!("\"{word}\"")
    } else {
        word
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    const GOLDEN: &str = include_str!("../../tests/fixtures/service/dstreamy.service");

    #[test]
    fn the_unit_matches_the_golden_file() {
        assert_eq!(unit(Path::new("/usr/local/bin/dstreamy")).unwrap(), GOLDEN);
    }

    #[test]
    fn exec_start_quotes_spaces_and_doubles_percent_signs() {
        let cases = [
            ("/opt/dstreamy/dstreamy", "/opt/dstreamy/dstreamy"),
            ("/opt/my apps/dstreamy", "\"/opt/my apps/dstreamy\""),
            ("/opt/100%/dstreamy", "/opt/100%%/dstreamy"),
            ("/opt/a b%n/dstreamy", "\"/opt/a b%%n/dstreamy\""),
            ("/opt/$HOME/dstreamy", ":/opt/$HOME/dstreamy"),
            ("/opt/a $b/dstreamy", "\":/opt/a $b/dstreamy\""),
            ("/opt/ünï/dstreamy", "/opt/ünï/dstreamy"),
        ];
        for (path, word) in cases {
            assert_eq!(exec_path(Path::new(path)).unwrap(), word, "{path}");
        }
    }

    #[test]
    fn exec_start_refuses_what_systemd_rejects_in_an_executable_name() {
        for (path, found) in [
            ("/opt/a\"b/dstreamy", '"'),
            ("/opt/it's/dstreamy", '\''),
            ("/opt/a\\b/dstreamy", '\\'),
            ("/opt/a\tb/dstreamy", '\t'),
            ("/opt/a\nb/dstreamy", '\n'),
            ("/opt/a\u{7f}b/dstreamy", '\u{7f}'),
        ] {
            match exec_path(Path::new(path)) {
                Err(ServiceError::UnsupportedPath { found: f, .. }) => {
                    assert_eq!(f, found, "{path}")
                }
                other => panic!("{path}: {other:?}"),
            }
        }
    }

    #[test]
    fn show_output_maps_to_running_and_enabled() {
        let up = supervision(
            "MainPID=4242\nLoadState=loaded\nActiveState=active\nSubState=running\nUnitFileState=enabled\n",
        );
        assert!(up.running && up.enabled);
        assert_eq!(up.detail, "loaded, active (running), enabled, pid 4242");

        let restarting = supervision(
            "MainPID=0\nLoadState=loaded\nActiveState=activating\nSubState=auto-restart\nUnitFileState=enabled\n",
        );
        assert!(!restarting.running && restarting.enabled);
        assert_eq!(
            restarting.detail,
            "loaded, activating (auto-restart), enabled"
        );

        let missing = supervision(
            "MainPID=0\nLoadState=not-found\nActiveState=inactive\nSubState=dead\nUnitFileState=\n",
        );
        assert!(!missing.running && !missing.enabled);
        assert_eq!(missing.detail, "not-found, inactive (dead)");
    }
}
