use std::ffi::OsStr;
use std::fs::{self, DirBuilder, Permissions};
use std::io::{self, ErrorKind};
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::{DirBuilderExt, FileTypeExt, MetadataExt, PermissionsExt};
use std::path::{Path, PathBuf};

use tokio::net::{UnixListener, UnixStream};

use crate::IpcError;

pub(crate) type Stream = UnixStream;
pub(crate) type Principal = u32;

const DIR_NAME: &str = "dstreamy";
const SOCKET_NAME: &str = "serve.sock";
const PRIVATE_MODE: u32 = 0o700;

/// `sizeof(sun_path)` minus the terminating NUL: 108 on Linux (`unix(7)`), 104 on macOS (`sys/un.h`).
#[cfg(any(target_os = "macos", target_os = "ios"))]
const MAX_PATH_LEN: usize = 103;
#[cfg(not(any(target_os = "macos", target_os = "ios")))]
const MAX_PATH_LEN: usize = 107;

/// The connecting process as reported by the kernel at connect time.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Peer {
    pub uid: u32,
    pub pid: Option<i32>,
}

impl Peer {
    pub(crate) fn principal(&self) -> Principal {
        self.uid
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Access {
    Private,
    EveryoneForTests,
}

pub(crate) fn own_principal() -> Principal {
    // SAFETY: geteuid has no preconditions and cannot fail.
    unsafe { libc::geteuid() }
}

#[cfg(any(target_os = "macos", target_os = "ios"))]
const RUNTIME_DIR_VAR: &str = "TMPDIR";
#[cfg(not(any(target_os = "macos", target_os = "ios")))]
const RUNTIME_DIR_VAR: &str = "XDG_RUNTIME_DIR";

pub(crate) fn default_location() -> PathBuf {
    resolve_location(
        std::env::var_os(RUNTIME_DIR_VAR).as_deref(),
        own_principal(),
    )
}

pub(crate) fn resolve_location(runtime_dir: Option<&OsStr>, uid: u32) -> PathBuf {
    runtime_dir
        .map(Path::new)
        .filter(|dir| dir.is_absolute())
        .map(|dir| dir.join(DIR_NAME).join(SOCKET_NAME))
        .filter(|path| fits_sun_path(path))
        .unwrap_or_else(|| {
            Path::new("/tmp")
                .join(format!("{DIR_NAME}-{uid}"))
                .join(SOCKET_NAME)
        })
}

fn fits_sun_path(path: &Path) -> bool {
    path.as_os_str().as_bytes().len() <= MAX_PATH_LEN
}

pub(crate) struct Listener {
    inner: UnixListener,
}

impl Listener {
    pub(crate) async fn bind(path: &Path, access: Access) -> Result<Self, IpcError> {
        if !fits_sun_path(path) {
            return Err(IpcError::PathTooLong(path.to_owned()));
        }
        let dir = path
            .parent()
            .filter(|dir| !dir.as_os_str().is_empty())
            .unwrap_or(Path::new("."));
        match access {
            Access::Private => prepare_private_dir(dir)?,
            Access::EveryoneForTests => {
                fs::create_dir_all(dir)?;
                fs::set_permissions(dir, Permissions::from_mode(0o711))?;
            }
        }
        remove_stale(path).await?;
        let inner = match UnixListener::bind(path) {
            Ok(inner) => inner,
            Err(e) if e.kind() == ErrorKind::AddrInUse => {
                return Err(IpcError::AlreadyRunning(path.to_owned()));
            }
            Err(e) => return Err(e.into()),
        };
        if access == Access::EveryoneForTests {
            fs::set_permissions(path, Permissions::from_mode(0o777))?;
        }
        Ok(Self { inner })
    }

    pub(crate) async fn accept(&self) -> io::Result<(Stream, io::Result<Peer>)> {
        let (stream, _) = self.inner.accept().await?;
        let peer = stream.peer_cred().map(|cred| Peer {
            uid: cred.uid(),
            pid: cred.pid(),
        });
        Ok((stream, peer))
    }
}

fn prepare_private_dir(dir: &Path) -> Result<(), IpcError> {
    match DirBuilder::new().mode(PRIVATE_MODE).create(dir) {
        Ok(()) => fs::set_permissions(dir, Permissions::from_mode(PRIVATE_MODE))?,
        Err(e) if e.kind() == ErrorKind::AlreadyExists => {}
        Err(e) => return Err(e.into()),
    }
    check_private_dir(dir, own_principal())
}

fn check_private_dir(dir: &Path, uid: u32) -> Result<(), IpcError> {
    let insecure = |reason: String| IpcError::InsecureDirectory {
        path: dir.to_owned(),
        reason,
    };
    let meta = fs::symlink_metadata(dir)?;
    if !meta.file_type().is_dir() {
        return Err(insecure("not a directory".into()));
    }
    if meta.uid() != uid {
        return Err(insecure(format!("owned by uid {}, not {uid}", meta.uid())));
    }
    let mode = meta.mode() & 0o7777;
    if mode != PRIVATE_MODE {
        return Err(insecure(format!("mode {mode:o}, expected 700")));
    }
    Ok(())
}

async fn remove_stale(path: &Path) -> Result<(), IpcError> {
    match fs::symlink_metadata(path) {
        Ok(meta) if meta.file_type().is_socket() => {}
        Ok(_) => return Err(IpcError::NotASocket(path.to_owned())),
        Err(e) if e.kind() == ErrorKind::NotFound => return Ok(()),
        Err(e) => return Err(e.into()),
    }
    match UnixStream::connect(path).await {
        Ok(_) => Err(IpcError::AlreadyRunning(path.to_owned())),
        Err(e) if e.kind() == ErrorKind::ConnectionRefused => match fs::remove_file(path) {
            Ok(()) => Ok(()),
            Err(e) if e.kind() == ErrorKind::NotFound => Ok(()),
            Err(e) => Err(e.into()),
        },
        Err(e) if e.kind() == ErrorKind::NotFound => Ok(()),
        Err(e) => Err(e.into()),
    }
}

pub(crate) async fn connect(path: &Path) -> Result<Stream, IpcError> {
    if !fits_sun_path(path) {
        return Err(IpcError::PathTooLong(path.to_owned()));
    }
    match UnixStream::connect(path).await {
        Ok(stream) => Ok(stream),
        Err(e) if matches!(e.kind(), ErrorKind::NotFound | ErrorKind::ConnectionRefused) => {
            Err(IpcError::NotRunning(path.to_owned()))
        }
        Err(e) => Err(e.into()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn runtime_dir_holds_the_socket() {
        let path = resolve_location(Some(OsStr::new("/run/user/1000")), 1000);
        assert_eq!(path, Path::new("/run/user/1000/dstreamy/serve.sock"));
    }

    #[test]
    fn missing_or_relative_runtime_dir_falls_back_to_tmp() {
        let fallback = Path::new("/tmp/dstreamy-1000/serve.sock");
        assert_eq!(resolve_location(None, 1000), fallback);
        assert_eq!(resolve_location(Some(OsStr::new("")), 1000), fallback);
        assert_eq!(
            resolve_location(Some(OsStr::new("run/user")), 1000),
            fallback
        );
    }

    #[test]
    fn sun_path_overflow_falls_back_to_tmp() {
        let long = format!("/var/folders/{}/T", "x".repeat(MAX_PATH_LEN));
        let path = resolve_location(Some(OsStr::new(&long)), 501);
        assert_eq!(path, Path::new("/tmp/dstreamy-501/serve.sock"));
    }

    #[test]
    fn longest_fitting_runtime_dir_is_kept() {
        let suffix = "/dstreamy/serve.sock".len();
        let dir = format!("/{}", "d".repeat(MAX_PATH_LEN - suffix - 1));
        let path = resolve_location(Some(OsStr::new(&dir)), 501);
        assert_eq!(path.as_os_str().len(), MAX_PATH_LEN);
        assert!(path.starts_with(&dir));
        let one_more = format!("{dir}d");
        let path = resolve_location(Some(OsStr::new(&one_more)), 501);
        assert_eq!(path, Path::new("/tmp/dstreamy-501/serve.sock"));
    }

    #[test]
    fn foreign_owner_is_refused() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path().join("ipc");
        fs::create_dir(&dir).unwrap();
        fs::set_permissions(&dir, Permissions::from_mode(PRIVATE_MODE)).unwrap();
        check_private_dir(&dir, own_principal()).unwrap();
        let err = check_private_dir(&dir, own_principal().wrapping_add(1)).unwrap_err();
        assert!(matches!(err, IpcError::InsecureDirectory { .. }), "{err}");
    }
}
