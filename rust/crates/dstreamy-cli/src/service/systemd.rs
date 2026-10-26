//! A systemd user unit for `dstreamy serve`.

use std::ffi::OsString;
use std::path::{Path, PathBuf};

use super::{Backend, InstallPlan, QUERY_TIMEOUT, ServiceError, Step, Supervision, checked_output};

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

    /// The main process is not `<exe> serve` from the unit: another binary, a replaced one or other arguments.
    fn runs_something_else(&self, pid: &str) -> bool {
        if pid.is_empty() || pid == "0" {
            return false;
        }
        let Ok(exe) = std::fs::read_link(format!("/proc/{pid}/exe")) else {
            return false;
        };
        // A replaced binary reads as "<path> (deleted)", so it never equals the unit's path.
        if exe != self.exe {
            return true;
        }
        let Ok(cmdline) = std::fs::read(format!("/proc/{pid}/cmdline")) else {
            return false;
        };
        let mut expected = self.exe.as_os_str().as_encoded_bytes().to_vec();
        expected.extend_from_slice(b"\0serve\0");
        cmdline != expected
    }

    fn owns_wants_link(&self) -> bool {
        std::fs::read_link(self.wants_link()).is_ok_and(|target| target == self.unit_path)
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

    fn install(&self, definition: &str, plan: InstallPlan) -> Vec<Step> {
        if !plan.write {
            let mut steps = Vec::new();
            if plan.reload {
                steps.push(Step::Run(self.systemctl(&["daemon-reload"])));
            }
            if plan.restart {
                steps.push(Step::Run(self.systemctl(&["try-restart", UNIT_NAME])));
            }
            steps.push(Step::Run(self.systemctl(&["enable", "--now", UNIT_NAME])));
            return steps;
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

    fn uninstall(&self, definition_present: bool, state: Option<&Supervision>) -> Vec<Step> {
        if definition_present {
            return vec![
                Step::Run(self.systemctl(&["disable", "--now", UNIT_NAME])),
                Step::Remove(self.unit_path.clone()),
                Step::Run(self.systemctl(&["daemon-reload"])),
            ];
        }
        // `disable` fails without the unit file, before its `--now` stop.
        let mut steps = Vec::new();
        if state.is_none_or(|s| s.failed) {
            steps.push(Step::TryRun(self.systemctl(&["reset-failed", UNIT_NAME])));
        }
        if state.is_none_or(|s| s.active) {
            steps.push(Step::TryRun(self.systemctl(&["stop", UNIT_NAME])));
        }
        if self.owns_wants_link() {
            steps.push(Step::Remove(self.wants_link()));
        }
        steps.push(Step::Run(self.systemctl(&["daemon-reload"])));
        steps
    }

    fn enablement_left(&self) -> bool {
        self.owns_wants_link()
    }

    fn query(&self) -> Result<Supervision, ServiceError> {
        let (_, show) = self.read(&[
            "show",
            "--property=LoadState,ActiveState,SubState,UnitFileState,MainPID,NeedDaemonReload,FragmentPath",
            UNIT_NAME,
        ])?;
        let mut state = supervision(&show);
        state.runs_something_else =
            state.running && self.runs_something_else(property(&show, "MainPID").unwrap_or(""));
        state.foreign = property(&show, "FragmentPath")
            .filter(|fragment| !fragment.is_empty())
            .map(PathBuf::from)
            .filter(|fragment| fragment.components().ne(self.unit_path.components()));
        Ok(state)
    }

    fn check_location(&self) -> Result<(), ServiceError> {
        let (command, show) = self.read(&["show", "--property=UnitPath"])?;
        let search_path = property(&show, "UnitPath").ok_or(ServiceError::Unexpected {
            command,
            what: "UnitPath".to_owned(),
        })?;
        let dir = self.unit_path.parent().unwrap_or(Path::new("/"));
        if searched(&shell_words_unquoted(search_path), dir) {
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

/// Splits a `show` list value; elements with whitespace or shell metacharacters arrive double-quoted with `\` escapes.
fn shell_words_unquoted(value: &str) -> Vec<String> {
    let mut words = Vec::new();
    let mut word = String::new();
    let mut in_word = false;
    let mut quoted = false;
    let mut chars = value.chars();
    while let Some(c) = chars.next() {
        match c {
            '"' => {
                quoted = !quoted;
                in_word = true;
            }
            '\\' => {
                word.extend(chars.next());
                in_word = true;
            }
            c if c.is_whitespace() && !quoted => {
                if in_word {
                    words.push(std::mem::take(&mut word));
                    in_word = false;
                }
            }
            c => {
                word.push(c);
                in_word = true;
            }
        }
    }
    if in_word {
        words.push(word);
    }
    words
}

/// Whether `dir` is one of `entries`, compared by path components, then canonicalised.
fn searched(entries: &[String], dir: &Path) -> bool {
    let canonical_dir = std::fs::canonicalize(dir).ok();
    entries.iter().map(Path::new).any(|entry| {
        entry.components().eq(dir.components())
            || canonical_dir
                .as_ref()
                .is_some_and(|dir| std::fs::canonicalize(entry).ok().as_ref() == Some(dir))
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
        failed: active == "failed",
        running,
        need_reload: get("NeedDaemonReload") == "yes",
        runs_something_else: false,
        foreign: None,
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

    fn on_path(search: &str, dir: &Path) -> bool {
        searched(&shell_words_unquoted(search), dir)
    }

    #[test]
    fn the_unit_directory_must_be_on_the_managers_search_path() {
        let search = "/home/u/.config/systemd/user.control /run/user/1/systemd/transient /home/u/.config/systemd/user /etc/systemd/user";
        assert!(on_path(search, Path::new("/home/u/.config/systemd/user")));
        assert!(!on_path(search, Path::new("/home/u/.config/systemd")));
        assert!(!on_path(search, Path::new("/elsewhere/systemd/user")));
    }

    #[test]
    fn quoted_search_path_entries_with_spaces_and_dollars_match() {
        let search = r#"/a "/home/my user/.config/systemd/user" "/srv/\$x \"q\"/systemd/user" /b"#;
        assert_eq!(
            shell_words_unquoted(search),
            [
                "/a",
                "/home/my user/.config/systemd/user",
                "/srv/$x \"q\"/systemd/user",
                "/b"
            ]
        );
        assert!(on_path(
            search,
            Path::new("/home/my user/.config/systemd/user")
        ));
        assert!(on_path(search, Path::new("/srv/$x \"q\"/systemd/user")));
        assert!(!on_path(search, Path::new("/home/my")));
    }

    #[test]
    fn doubled_and_trailing_slashes_do_not_matter_even_before_the_directory_exists() {
        let dir = Path::new("/nonexistent/home/u/.config/systemd/user");
        assert!(on_path(
            "/x /nonexistent/home//u/.config/systemd/user /y",
            dir
        ));
        assert!(on_path(
            "/x /nonexistent/home/u/.config/systemd/user/ /y",
            dir
        ));
        assert!(on_path(
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
        assert!(on_path(&format!("/x {}", real.display()), &via_link));
    }
}
