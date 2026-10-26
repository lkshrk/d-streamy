//! A systemd user unit for `dstreamy serve`.

use std::ffi::OsString;
use std::path::{Path, PathBuf};

use super::{Backend, QUERY_TIMEOUT, ServiceError, Step, Supervision, checked_output};

pub const UNIT_NAME: &str = "dstreamy.service";

pub struct Systemd {
    systemctl: PathBuf,
    exe: PathBuf,
    unit_path: PathBuf,
}

impl Systemd {
    pub fn new(systemctl: PathBuf, exe: &Path) -> Result<Self, ServiceError> {
        Ok(Self {
            systemctl,
            exe: exe.to_owned(),
            unit_path: user_unit_dir()?.join(UNIT_NAME),
        })
    }

    fn wants_link(&self) -> PathBuf {
        self.unit_path
            .with_file_name("default.target.wants")
            .join(UNIT_NAME)
    }

    /// The main process runs an outdated definition or a binary that was replaced.
    fn stale(&self, show: &str) -> bool {
        if property(show, "NeedDaemonReload") == Some("yes") {
            return true;
        }
        let pid = property(show, "MainPID").unwrap_or("0");
        let exe = std::fs::read_link(format!("/proc/{pid}/exe"));
        if pid != "0" && exe.is_ok_and(|exe| exe.to_string_lossy().ends_with(" (deleted)")) {
            return true;
        }
        let started = property(show, "ExecMainStartTimestamp")
            .and_then(|t| t.strip_prefix('@'))
            .and_then(|t| t.parse::<u64>().ok());
        let written = std::fs::metadata(&self.unit_path)
            .and_then(|m| m.modified())
            .ok()
            .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
            .map(|d| d.as_secs());
        matches!((started, written), (Some(started), Some(written)) if started < written)
    }

    fn systemctl(&self, args: &[&str]) -> Vec<OsString> {
        let mut argv = vec![self.systemctl.clone().into_os_string(), "--user".into()];
        argv.extend(args.iter().map(OsString::from));
        argv
    }

    fn read(&self, args: &[&str]) -> Result<(String, String), ServiceError> {
        let argv = self.systemctl(args);
        let output = checked_output(&argv, Some(QUERY_TIMEOUT))?;
        Ok((
            super::shell_words(&argv),
            String::from_utf8_lossy(&output.stdout).into_owned(),
        ))
    }
}

impl Backend for Systemd {
    fn name(&self) -> &str {
        UNIT_NAME
    }

    fn definition_path(&self) -> &Path {
        &self.unit_path
    }

    fn definition(&self) -> Result<String, ServiceError> {
        unit(&self.exe)
    }

    fn install(&self, definition: &str, write: bool) -> Vec<Step> {
        if !write {
            return vec![Step::Run(self.systemctl(&["enable", "--now", UNIT_NAME]))];
        }
        vec![
            Step::Write {
                path: self.unit_path.clone(),
                contents: definition.to_owned(),
            },
            Step::Run(self.systemctl(&["daemon-reload"])),
            Step::Run(self.systemctl(&["try-restart", UNIT_NAME])),
            Step::Run(self.systemctl(&["enable", "--now", UNIT_NAME])),
        ]
    }

    fn uninstall(&self, definition_present: bool, known: bool) -> Vec<Step> {
        if definition_present {
            return vec![
                Step::Run(self.systemctl(&["disable", "--now", UNIT_NAME])),
                Step::Remove(self.unit_path.clone()),
                Step::Run(self.systemctl(&["daemon-reload"])),
            ];
        }
        // `disable` fails without the unit file, before its `--now` stop.
        let mut steps = Vec::new();
        if known {
            steps.push(Step::Run(self.systemctl(&["stop", UNIT_NAME])));
            steps.push(Step::Run(self.systemctl(&["reset-failed", UNIT_NAME])));
        }
        steps.push(Step::Remove(self.wants_link()));
        steps.push(Step::Run(self.systemctl(&["daemon-reload"])));
        steps
    }

    fn enablement_left(&self) -> bool {
        std::fs::symlink_metadata(self.wants_link()).is_ok()
    }

    fn query(&self) -> Result<Supervision, ServiceError> {
        let (_, show) = self.read(&[
            "show",
            "--timestamp=unix",
            "--property=LoadState,ActiveState,SubState,UnitFileState,MainPID,NeedDaemonReload,ExecMainStartTimestamp",
            UNIT_NAME,
        ])?;
        let mut state = supervision(&show);
        state.stale = state.running && self.stale(&show);
        Ok(state)
    }

    fn check_location(&self) -> Result<(), ServiceError> {
        let (command, show) = self.read(&["show", "--property=UnitPath"])?;
        let search_path = property(&show, "UnitPath").ok_or(ServiceError::Unexpected {
            command,
            what: "UnitPath".to_owned(),
        })?;
        let dir = self.unit_path.parent().unwrap_or(Path::new("/"));
        if searched(search_path, dir) {
            Ok(())
        } else {
            Err(ServiceError::NotSearched {
                dir: dir.to_owned(),
                search_path: search_path.to_owned(),
            })
        }
    }

    fn service_runtime_dir(&self) -> Result<Option<OsString>, ServiceError> {
        let (command, env) = self.read(&["show-environment"])?;
        match property(&env, "XDG_RUNTIME_DIR") {
            None => Ok(None),
            Some(value) if value.starts_with("$'") => Err(ServiceError::Unexpected {
                command,
                what: "XDG_RUNTIME_DIR it can decode".to_owned(),
            }),
            Some(value) => Ok(Some(value.into())),
        }
    }
}

fn property<'a>(show: &'a str, name: &str) -> Option<&'a str> {
    show.lines()
        .find_map(|line| line.strip_prefix(name)?.strip_prefix('='))
}

/// Whether `dir` is an entry of the manager's space-separated `UnitPath`, compared by path components.
fn searched(search_path: &str, dir: &Path) -> bool {
    let words: Vec<&str> = search_path.split(' ').collect();
    let entries = (0..words.len())
        .flat_map(|start| (start + 1..=words.len()).map(move |end| (start, end)))
        .map(|(start, end)| PathBuf::from(words[start..end].join(" ")));
    let canonical_dir = std::fs::canonicalize(dir).ok();
    entries.into_iter().any(|entry| {
        entry.components().eq(dir.components())
            || canonical_dir
                .as_ref()
                .is_some_and(|dir| std::fs::canonicalize(&entry).ok().as_ref() == Some(dir))
    })
}

fn supervision(show: &str) -> Supervision {
    let get = |name| property(show, name).unwrap_or("");
    let (load, active, sub, file, pid) = (
        get("LoadState"),
        get("ActiveState"),
        get("SubState"),
        get("UnitFileState"),
        get("MainPID"),
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
        loaded: !load.is_empty() && load != "not-found",
        enabled: file == "enabled",
        active: !active.is_empty() && active != "inactive" && active != "failed",
        running,
        stale: false,
        detail,
    }
}

/// `$XDG_CONFIG_HOME/systemd/user`, else `$HOME/.config/systemd/user`, as the user manager resolves it.
pub fn user_unit_dir() -> Result<PathBuf, ServiceError> {
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
pub fn exec_path(exe: &Path) -> Result<String, ServiceError> {
    let text = exe
        .to_str()
        .ok_or_else(|| ServiceError::NotUtf8(exe.to_owned()))?;
    if let Some(found) = text
        .chars()
        .find(|&c| c < ' ' || matches!(c, '\u{7f}' | '"' | '\'' | '\\'))
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
            ("/opt/a\u{85}b/dstreamy", "/opt/a\u{85}b/dstreamy"),
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
            ("/opt/a\u{1f}b/dstreamy", '\u{1f}'),
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
        assert!(up.loaded && up.active && up.running && up.enabled);
        assert_eq!(up.detail, "loaded, active (running), enabled, pid 4242");

        let restarting = supervision(
            "MainPID=0\nLoadState=loaded\nActiveState=activating\nSubState=auto-restart\nUnitFileState=enabled\n",
        );
        assert!(restarting.active && !restarting.running && restarting.enabled);
        assert_eq!(
            restarting.detail,
            "loaded, activating (auto-restart), enabled"
        );

        let missing = supervision(
            "MainPID=0\nLoadState=not-found\nActiveState=inactive\nSubState=dead\nUnitFileState=\n",
        );
        assert!(!missing.loaded && !missing.active && !missing.running && !missing.enabled);
        assert_eq!(missing.detail, "not-found, inactive (dead)");
    }

    #[test]
    fn the_unit_directory_must_be_on_the_managers_search_path() {
        let search = "/home/u/.config/systemd/user.control /run/user/1/systemd/transient /home/u/.config/systemd/user /etc/systemd/user";
        assert!(searched(search, Path::new("/home/u/.config/systemd/user")));
        assert!(!searched(search, Path::new("/home/u/.config/systemd")));
        assert!(!searched(search, Path::new("/elsewhere/systemd/user")));
        assert!(searched(
            "/a /home/my user/.config/systemd/user /b",
            Path::new("/home/my user/.config/systemd/user")
        ));
    }

    #[test]
    fn doubled_and_trailing_slashes_do_not_matter_even_before_the_directory_exists() {
        let dir = Path::new("/nonexistent/home/u/.config/systemd/user");
        assert!(searched(
            "/x /nonexistent/home//u/.config/systemd/user /y",
            dir
        ));
        assert!(searched(
            "/x /nonexistent/home/u/.config/systemd/user/ /y",
            dir
        ));
        assert!(searched(
            "/x /nonexistent/home/u/.config/systemd/user",
            Path::new("/nonexistent/home//u/.config/systemd/user/")
        ));
    }

    #[test]
    fn a_search_path_entry_reached_through_a_symlink_matches() {
        let dir = tempfile::tempdir().unwrap();
        let real = dir.path().join("real/systemd/user");
        std::fs::create_dir_all(&real).unwrap();
        std::os::unix::fs::symlink(dir.path().join("real"), dir.path().join("link")).unwrap();
        let via_link = dir.path().join("link/systemd/user");
        assert!(searched(&format!("/x {}", real.display()), &via_link));
    }
}
