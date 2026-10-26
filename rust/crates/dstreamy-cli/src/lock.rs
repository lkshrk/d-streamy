//! One live session per config directory, held with an OS file lock that dies with the process.

use std::fs::{self, File, OpenOptions, TryLockError};
use std::io;
use std::path::{Path, PathBuf};

use dstreamy_core::ConfigPaths;

#[derive(Debug, thiserror::Error)]
pub enum LockError {
    #[error("another dstreamy session is live{}", pid.map(|p| format!(" (pid {p})")).unwrap_or_default())]
    Held { pid: Option<u32> },
    #[error("session lock {}: {source}", path.display())]
    Io { path: PathBuf, source: io::Error },
}

/// Released when dropped, or by the OS when the process dies.
#[derive(Debug)]
pub struct SessionLock {
    file: File,
    pid_file: PathBuf,
}

impl SessionLock {
    pub fn acquire(paths: &ConfigPaths) -> Result<Self, LockError> {
        let lock_file = paths.config_file.with_file_name("session.lock");
        let pid_file = paths.config_file.with_file_name("session.pid");
        let io_err = |path: &Path| {
            let path = path.to_owned();
            move |source| LockError::Io { path, source }
        };
        if let Some(dir) = lock_file.parent() {
            fs::create_dir_all(dir).map_err(io_err(dir))?;
        }
        let file = OpenOptions::new()
            .create(true)
            .truncate(false)
            .write(true)
            .open(&lock_file)
            .map_err(io_err(&lock_file))?;
        match file.try_lock() {
            Ok(()) => {}
            Err(TryLockError::WouldBlock) => {
                return Err(LockError::Held {
                    pid: read_pid(&pid_file),
                });
            }
            Err(TryLockError::Error(e)) => return Err(io_err(&lock_file)(e)),
        }
        fs::write(&pid_file, format!("{}\n", std::process::id())).map_err(io_err(&pid_file))?;
        Ok(Self { file, pid_file })
    }
}

impl Drop for SessionLock {
    fn drop(&mut self) {
        // Runs before the lock file closes, so it never deletes the next holder's PID.
        let _ = fs::remove_file(&self.pid_file);
        // Closing alone leaves it held while a child forked before its exec still has the descriptor.
        let _ = self.file.unlock();
    }
}

fn read_pid(path: &Path) -> Option<u32> {
    fs::read_to_string(path).ok()?.trim().parse().ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_second_acquire_is_refused_with_the_pid_until_the_first_is_dropped() {
        let dir = tempfile::tempdir().unwrap();
        let paths = ConfigPaths::in_dir(&dir.path().join("nested"));
        let first = SessionLock::acquire(&paths).unwrap();
        let pid = read_pid(&dir.path().join("nested/session.pid"));
        assert_eq!(pid, Some(std::process::id()));

        let err = SessionLock::acquire(&paths).unwrap_err();
        assert!(
            matches!(err, LockError::Held { pid: Some(p) } if p == std::process::id()),
            "{err:?}"
        );
        assert_eq!(
            err.to_string(),
            format!(
                "another dstreamy session is live (pid {})",
                std::process::id()
            )
        );

        drop(first);
        assert!(!dir.path().join("nested/session.pid").exists());
        let again = SessionLock::acquire(&paths).unwrap();
        drop(again);
        SessionLock::acquire(&paths).unwrap();
    }

    #[test]
    fn a_dropped_lock_is_free_while_a_copy_of_its_descriptor_lives() {
        let dir = tempfile::tempdir().unwrap();
        let paths = ConfigPaths::in_dir(dir.path());
        let held = SessionLock::acquire(&paths).unwrap();
        let forked_copy = held.file.try_clone().unwrap();
        drop(held);
        SessionLock::acquire(&paths).unwrap();
        drop(forked_copy);
    }

    #[test]
    fn a_held_lock_without_a_pid_file_names_no_pid() {
        let dir = tempfile::tempdir().unwrap();
        let paths = ConfigPaths::in_dir(dir.path());
        let _held = SessionLock::acquire(&paths).unwrap();
        fs::remove_file(dir.path().join("session.pid")).unwrap();
        let err = SessionLock::acquire(&paths).unwrap_err();
        assert!(matches!(err, LockError::Held { pid: None }), "{err:?}");
        assert_eq!(err.to_string(), "another dstreamy session is live");
    }
}
