use std::ffi::OsStr;
use std::fs::{self, DirBuilder, File, OpenOptions, Permissions, TryLockError};
use std::io::{self, ErrorKind};
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::{DirBuilderExt, FileTypeExt, MetadataExt, OpenOptionsExt, PermissionsExt};
use std::path::{Path, PathBuf};

use tokio::net::{UnixListener, UnixStream};

use crate::{IpcError, Peer};

pub(crate) type Stream = UnixStream;
pub(crate) type Principal = u32;

const DIR_NAME: &str = "dstreamy";
const SOCKET_NAME: &str = "serve.sock";
const LOCK_NAME: &str = "serve.lock";
const PRIVATE_MODE: u32 = 0o700;

/// `sizeof(sun_path)` minus the terminating NUL: 108 on Linux (`unix(7)`), 104 on macOS (`sys/un.h`).
#[cfg(target_vendor = "apple")]
const MAX_PATH_LEN: usize = 103;
#[cfg(not(target_vendor = "apple"))]
const MAX_PATH_LEN: usize = 107;

#[cfg(target_vendor = "apple")]
const RUNTIME_DIR_VAR: &str = "TMPDIR";
#[cfg(not(target_vendor = "apple"))]
const RUNTIME_DIR_VAR: &str = "XDG_RUNTIME_DIR";

#[cfg(target_vendor = "apple")]
const HOME_FALLBACK: &str = "Library/Application Support";
#[cfg(not(target_vendor = "apple"))]
const HOME_FALLBACK: &str = ".local/state";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Access {
    Private,
    #[cfg(feature = "test-support")]
    EveryoneForTests,
}

pub(crate) fn own_principal() -> Principal {
    // SAFETY: geteuid has no preconditions and cannot fail.
    unsafe { libc::geteuid() }
}

pub(crate) fn default_location() -> Result<PathBuf, IpcError> {
    resolve_location(
        std::env::var_os(RUNTIME_DIR_VAR).as_deref(),
        std::env::home_dir().as_deref(),
    )
}

pub(crate) fn resolve_location(
    runtime_dir: Option<&OsStr>,
    home: Option<&Path>,
) -> Result<PathBuf, IpcError> {
    let in_dir = |dir: &Path| dir.join(DIR_NAME).join(SOCKET_NAME);
    let runtime = runtime_dir
        .map(Path::new)
        .filter(|dir| dir.is_absolute())
        .map(in_dir)
        .filter(|path| fits_sun_path(path));
    if let Some(path) = runtime {
        return Ok(path);
    }
    let home = home
        .filter(|home| home.is_absolute())
        .ok_or(IpcError::NoHomeDirectory)?;
    let path = in_dir(&home.join(HOME_FALLBACK));
    if fits_sun_path(&path) {
        Ok(path)
    } else {
        Err(IpcError::PathTooLong(path))
    }
}

fn fits_sun_path(path: &Path) -> bool {
    path.as_os_str().as_bytes().len() <= MAX_PATH_LEN
}

fn socket_dir(path: &Path) -> &Path {
    path.parent()
        .filter(|dir| !dir.as_os_str().is_empty())
        .unwrap_or(Path::new("."))
}

fn peer_of(stream: &UnixStream) -> io::Result<Peer> {
    let cred = stream.peer_cred()?;
    Ok(Peer::new(
        cred.uid(),
        cred.pid().and_then(|pid| u32::try_from(pid).ok()),
    ))
}

pub(crate) struct Listener {
    inner: UnixListener,
}

impl Listener {
    pub(crate) async fn bind(path: &Path, access: Access) -> Result<Self, IpcError> {
        if !fits_sun_path(path) {
            return Err(IpcError::PathTooLong(path.to_owned()));
        }
        let dir = socket_dir(path);
        match access {
            Access::Private => prepare_private_dir(dir)?,
            #[cfg(feature = "test-support")]
            Access::EveryoneForTests => {
                fs::create_dir_all(dir)?;
                fs::set_permissions(dir, Permissions::from_mode(0o711))?;
            }
        }
        let _lock = lock_dir(dir, path)?;
        remove_stale(path).await?;
        let inner = match UnixListener::bind(path) {
            Ok(inner) => inner,
            Err(e) if e.kind() == ErrorKind::AddrInUse => {
                return Err(IpcError::AlreadyRunning(path.to_owned()));
            }
            Err(e) => return Err(e.into()),
        };
        #[cfg(feature = "test-support")]
        if access == Access::EveryoneForTests {
            fs::set_permissions(path, Permissions::from_mode(0o777))?;
        }
        Ok(Self { inner })
    }

    pub(crate) async fn accept(&self) -> io::Result<(Stream, io::Result<Peer>)> {
        let (stream, _) = self.inner.accept().await?;
        let peer = peer_of(&stream);
        Ok((stream, peer))
    }
}

/// Held from the stale check until bind returns, so two services starting together can't both unlink.
fn lock_dir(dir: &Path, socket: &Path) -> Result<File, IpcError> {
    let file = OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .mode(0o600)
        .custom_flags(libc::O_NOFOLLOW)
        .open(dir.join(LOCK_NAME))?;
    match file.try_lock() {
        Ok(()) => Ok(file),
        Err(TryLockError::WouldBlock) => Err(IpcError::AlreadyRunning(socket.to_owned())),
        Err(TryLockError::Error(e)) => Err(e.into()),
    }
}

fn prepare_private_dir(dir: &Path) -> Result<(), IpcError> {
    if let Some(parent) = dir.parent().filter(|p| !p.as_os_str().is_empty()) {
        fs::create_dir_all(parent)?;
    }
    match DirBuilder::new().mode(PRIVATE_MODE).create(dir) {
        Ok(()) => fs::set_permissions(dir, Permissions::from_mode(PRIVATE_MODE))?,
        Err(e) if e.kind() == ErrorKind::AlreadyExists => {}
        Err(e) => return Err(e.into()),
    }
    check_private_dir(dir, own_principal())
}

pub(crate) fn check_private_dir(dir: &Path, uid: u32) -> Result<(), IpcError> {
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
    let own = own_principal();
    match check_private_dir(socket_dir(path), own) {
        Err(IpcError::Io(e)) if e.kind() == ErrorKind::NotFound => {
            return Err(IpcError::NotRunning(path.to_owned()));
        }
        checked => checked?,
    }
    let stream = match UnixStream::connect(path).await {
        Ok(stream) => stream,
        Err(e) if matches!(e.kind(), ErrorKind::NotFound | ErrorKind::ConnectionRefused) => {
            return Err(IpcError::NotRunning(path.to_owned()));
        }
        Err(e) => return Err(e.into()),
    };
    let server = peer_of(&stream)?;
    if !crate::authorize(&own, &server.principal()) {
        return Err(IpcError::ImpostorServer {
            path: path.to_owned(),
            uid: server.principal(),
        });
    }
    Ok(stream)
}

#[cfg(test)]
mod tests {
    use super::*;

    const HOME: &str = "/home/u";

    fn home_fallback() -> PathBuf {
        Path::new(HOME)
            .join(HOME_FALLBACK)
            .join("dstreamy/serve.sock")
    }

    fn resolve(runtime: Option<&str>) -> PathBuf {
        resolve_location(runtime.map(OsStr::new), Some(Path::new(HOME))).unwrap()
    }

    #[test]
    fn runtime_dir_holds_the_socket() {
        assert_eq!(
            resolve(Some("/run/user/1000")),
            Path::new("/run/user/1000/dstreamy/serve.sock")
        );
    }

    #[test]
    fn missing_or_relative_runtime_dir_falls_back_to_home() {
        assert_eq!(resolve(None), home_fallback());
        assert_eq!(resolve(Some("")), home_fallback());
        assert_eq!(resolve(Some("run/user")), home_fallback());
    }

    #[test]
    fn sun_path_overflow_falls_back_to_home_never_tmp() {
        let long = format!("/var/folders/{}/T", "x".repeat(MAX_PATH_LEN));
        let path = resolve(Some(&long));
        assert_eq!(path, home_fallback());
        assert!(!path.starts_with("/tmp"));
    }

    #[test]
    fn longest_fitting_runtime_dir_is_kept() {
        let suffix = "/dstreamy/serve.sock".len();
        let dir = format!("/{}", "d".repeat(MAX_PATH_LEN - suffix - 1));
        let path = resolve(Some(&dir));
        assert_eq!(path.as_os_str().len(), MAX_PATH_LEN);
        assert!(path.starts_with(&dir));
        assert_eq!(resolve(Some(&format!("{dir}d"))), home_fallback());
    }

    #[test]
    fn over_long_home_is_path_too_long() {
        let home = PathBuf::from(format!("/home/{}", "h".repeat(MAX_PATH_LEN)));
        let err = resolve_location(None, Some(&home)).unwrap_err();
        assert!(matches!(err, IpcError::PathTooLong(p) if p.starts_with(&home)));
    }

    #[test]
    fn no_home_and_no_runtime_dir_is_an_error() {
        let err = resolve_location(None, None).unwrap_err();
        assert!(matches!(err, IpcError::NoHomeDirectory), "{err}");
        let err = resolve_location(None, Some(Path::new("relative"))).unwrap_err();
        assert!(matches!(err, IpcError::NoHomeDirectory), "{err}");
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
