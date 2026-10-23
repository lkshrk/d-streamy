use std::ffi::{CStr, OsStr};
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
    #[cfg(any(test, feature = "test-support"))]
    EveryoneForTests,
}

pub(crate) fn own_principal() -> Principal {
    // SAFETY: geteuid has no preconditions and cannot fail.
    unsafe { libc::geteuid() }
}

pub(crate) fn default_location() -> Result<PathBuf, IpcError> {
    let uid = own_principal();
    resolve_location(
        std::env::var_os(RUNTIME_DIR_VAR).as_deref(),
        passwd_home(uid).as_deref(),
        |base| check_base(base, uid),
    )
}

pub(crate) fn resolve_location(
    runtime_dir: Option<&OsStr>,
    home: Option<&Path>,
    trust: impl Fn(&Path) -> Result<(), String>,
) -> Result<PathBuf, IpcError> {
    let in_dir = |dir: &Path| dir.join(DIR_NAME).join(SOCKET_NAME);
    let runtime = runtime_dir
        .map(Path::new)
        .filter(|dir| dir.is_absolute())
        .filter(|dir| fits_sun_path(&in_dir(dir)))
        .filter(|dir| trust(dir).is_ok());
    if let Some(dir) = runtime {
        return Ok(in_dir(dir));
    }
    let home = home
        .filter(|home| home.is_absolute())
        .ok_or(IpcError::NoHomeDirectory)?;
    let base = home.join(HOME_FALLBACK);
    let path = in_dir(&base);
    if !fits_sun_path(&path) {
        return Err(IpcError::PathTooLong(path));
    }
    trust(&base).map_err(|reason| IpcError::InsecureDirectory { path: base, reason })?;
    Ok(path)
}

/// The home directory from the user database, not `$HOME`, which `sudo -E` carries over.
fn passwd_home(uid: u32) -> Option<PathBuf> {
    let mut buf = vec![0 as libc::c_char; 1024];
    loop {
        let mut pwd = std::mem::MaybeUninit::<libc::passwd>::uninit();
        let mut found = std::ptr::null_mut();
        // SAFETY: every pointer is valid for the stated length for the duration of the call.
        let rc = unsafe {
            libc::getpwuid_r(
                uid,
                pwd.as_mut_ptr(),
                buf.as_mut_ptr(),
                buf.len(),
                &mut found,
            )
        };
        if rc == libc::ERANGE && buf.len() < 1 << 20 {
            buf.resize(buf.len() * 2, 0);
            continue;
        }
        if rc != 0 || found.is_null() {
            return None;
        }
        // SAFETY: on success `found` points at `pwd`, whose strings live in `buf`.
        let dir = unsafe { (*found).pw_dir };
        if dir.is_null() {
            return None;
        }
        // SAFETY: pw_dir is a NUL-terminated string inside `buf`.
        let dir = unsafe { CStr::from_ptr(dir) };
        return Some(PathBuf::from(OsStr::from_bytes(dir.to_bytes())));
    }
}

/// Trusted when every existing component, symlink targets included, is ours or root's and writable by no one else.
pub(crate) fn check_base(base: &Path, uid: u32) -> Result<(), String> {
    check_components(base, uid, 0)
}

fn check_components(path: &Path, uid: u32, depth: u32) -> Result<(), String> {
    if depth > 8 {
        return Err(format!("{}: too many symbolic links", path.display()));
    }
    if !path.is_absolute() {
        return Err(format!("{} is not absolute", path.display()));
    }
    let mut current = PathBuf::new();
    for part in path.components() {
        if part == std::path::Component::ParentDir {
            return Err(format!("{} contains ..", path.display()));
        }
        current.push(part);
        let meta = match fs::symlink_metadata(&current) {
            Ok(meta) => meta,
            Err(e) if e.kind() == ErrorKind::NotFound => return Ok(()),
            Err(e) => return Err(format!("{}: {e}", current.display())),
        };
        if meta.uid() != uid && meta.uid() != 0 {
            return Err(format!(
                "{} is owned by uid {}",
                current.display(),
                meta.uid()
            ));
        }
        if meta.file_type().is_symlink() {
            let target =
                fs::read_link(&current).map_err(|e| format!("{}: {e}", current.display()))?;
            let target = current.parent().unwrap_or(Path::new("/")).join(target);
            check_components(&target, uid, depth + 1)?;
            current =
                fs::canonicalize(&target).map_err(|e| format!("{}: {e}", target.display()))?;
            continue;
        }
        if !meta.is_dir() {
            return Err(format!("{} is not a directory", current.display()));
        }
        if meta.mode() & 0o022 != 0 {
            return Err(format!("{} is writable by other users", current.display()));
        }
    }
    Ok(())
}

fn fits_sun_path(path: &Path) -> bool {
    path.as_os_str().as_bytes().len() <= MAX_PATH_LEN
}

fn socket_dir(path: &Path) -> &Path {
    path.parent()
        .filter(|dir| !dir.as_os_str().is_empty())
        .unwrap_or(Path::new("."))
}

#[cfg(not(target_vendor = "apple"))]
fn peer_of(stream: &UnixStream) -> io::Result<Peer> {
    let cred = stream.peer_cred()?;
    Ok(Peer::new(
        cred.uid(),
        cred.pid().and_then(|pid| u32::try_from(pid).ok()),
    ))
}

/// tokio asks `LOCAL_PEEREPID` first, which is ENOTCONN once the peer closed (XNU `uipc_usrreq.c:852-856`).
#[cfg(target_vendor = "apple")]
fn peer_of(stream: &UnixStream) -> io::Result<Peer> {
    use std::os::fd::AsRawFd;
    let (mut uid, mut gid) = (0, 0);
    // SAFETY: both out-pointers are valid; getpeereid reads the credentials cached at connect.
    if unsafe { libc::getpeereid(stream.as_raw_fd(), &mut uid, &mut gid) } != 0 {
        return Err(io::Error::last_os_error());
    }
    let pid = stream
        .peer_cred()
        .ok()
        .and_then(|cred| cred.pid())
        .and_then(|pid| u32::try_from(pid).ok());
    Ok(Peer::new(uid, pid))
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
            #[cfg(any(test, feature = "test-support"))]
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
        #[cfg(any(test, feature = "test-support"))]
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
        DirBuilder::new()
            .recursive(true)
            .mode(PRIVATE_MODE)
            .create(parent)?;
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
    let opened = OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_DIRECTORY | libc::O_NOFOLLOW)
        .open(dir);
    let meta = match opened {
        Ok(file) => file.metadata()?,
        Err(e) if e.raw_os_error() == Some(libc::ELOOP) => {
            return Err(insecure("is a symbolic link".into()));
        }
        Err(e) if e.raw_os_error() == Some(libc::ENOTDIR) => {
            return Err(insecure("not a directory".into()));
        }
        Err(e) => return Err(e.into()),
    };
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

pub(crate) async fn connect(path: &Path, own: Principal) -> Result<Stream, IpcError> {
    if !fits_sun_path(path) {
        return Err(IpcError::PathTooLong(path.to_owned()));
    }
    match check_private_dir(socket_dir(path), own_principal()) {
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
    verify_server(path, own, peer_of(&stream))?;
    Ok(stream)
}

fn verify_server(path: &Path, own: Principal, server: io::Result<Peer>) -> Result<(), IpcError> {
    let server = server.map_err(|e| IpcError::UnverifiedServer {
        path: path.to_owned(),
        reason: e.to_string(),
    })?;
    if !crate::authorize(&own, &server.principal()) {
        return Err(IpcError::ImpostorServer {
            path: path.to_owned(),
            uid: server.principal(),
        });
    }
    Ok(())
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
        resolve_location(runtime.map(OsStr::new), Some(Path::new(HOME)), |_| Ok(())).unwrap()
    }

    fn trusted_tempdir() -> tempfile::TempDir {
        let home = passwd_home(own_principal()).unwrap();
        let dir = tempfile::Builder::new()
            .prefix(".ipc")
            .tempdir_in(home)
            .unwrap();
        fs::set_permissions(dir.path(), Permissions::from_mode(0o755)).unwrap();
        dir
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
        let err = resolve_location(None, Some(&home), |_| Ok(())).unwrap_err();
        assert!(matches!(err, IpcError::PathTooLong(p) if p.starts_with(&home)));
    }

    #[test]
    fn no_home_and_no_runtime_dir_is_an_error() {
        let err = resolve_location(None, None, |_| Ok(())).unwrap_err();
        assert!(matches!(err, IpcError::NoHomeDirectory), "{err}");
        let err = resolve_location(None, Some(Path::new("relative")), |_| Ok(())).unwrap_err();
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

    #[test]
    fn tmp_is_never_a_trusted_base() {
        let err = check_base(Path::new("/tmp"), own_principal()).unwrap_err();
        assert!(err.contains("writable by other users"), "{err}");
        assert!(check_base(Path::new("/tmp/dstreamy-base"), own_principal()).is_err());
    }

    #[test]
    fn untrusted_runtime_dir_falls_back_to_the_home_base() {
        let home = trusted_tempdir();
        let uid = own_principal();
        let path = resolve_location(Some(OsStr::new("/tmp")), Some(home.path()), |b| {
            check_base(b, uid)
        })
        .unwrap();
        assert_eq!(
            path,
            home.path().join(HOME_FALLBACK).join("dstreamy/serve.sock")
        );
    }

    #[test]
    fn base_owned_by_another_uid_is_skipped() {
        let runtime = trusted_tempdir();
        let home = Path::new("/home/u");
        let uid = own_principal();
        check_base(runtime.path(), uid).unwrap();
        let other = check_base(runtime.path(), uid.wrapping_add(1)).unwrap_err();
        assert!(other.contains("owned by uid"), "{other}");
        let path = resolve_location(Some(runtime.path().as_os_str()), Some(home), |b| {
            if b == runtime.path() {
                check_base(b, uid.wrapping_add(1))
            } else {
                Ok(())
            }
        })
        .unwrap();
        assert_eq!(path, home.join(HOME_FALLBACK).join("dstreamy/serve.sock"));
    }

    #[test]
    fn writable_base_is_refused() {
        for mode in [0o775, 0o757, 0o1777] {
            let base = trusted_tempdir();
            fs::set_permissions(base.path(), Permissions::from_mode(mode)).unwrap();
            let err = check_base(&base.path().join("not-yet"), own_principal()).unwrap_err();
            assert!(err.contains("writable by other users"), "{mode:o}: {err}");
        }
    }

    #[test]
    fn untrusted_home_base_is_insecure() {
        let err =
            resolve_location(None, Some(Path::new("/home/u")), |_| Err("nope".into())).unwrap_err();
        assert!(matches!(err, IpcError::InsecureDirectory { .. }), "{err}");
    }

    #[test]
    fn home_comes_from_the_user_database() {
        let home = passwd_home(own_principal()).expect("current user has a passwd entry");
        assert!(home.is_absolute(), "{}", home.display());
    }

    #[test]
    fn server_check_fails_closed() {
        let path = Path::new("/x/serve.sock");
        let own = own_principal();
        verify_server(path, own, Ok(Peer::new(own, None))).unwrap();
        let err =
            verify_server(path, own, Ok(Peer::new(own.wrapping_add(1), Some(7)))).unwrap_err();
        assert!(matches!(err, IpcError::ImpostorServer { uid, .. } if uid == own.wrapping_add(1)));
        let enotconn = io::Error::from_raw_os_error(libc::ENOTCONN);
        let err = verify_server(path, own, Err(enotconn)).unwrap_err();
        assert!(matches!(err, IpcError::UnverifiedServer { .. }), "{err}");
    }

    fn link_home_from(dir: &Path) -> (PathBuf, Result<(), String>) {
        let home = passwd_home(own_principal()).unwrap();
        let link = dir.join(format!("dstreamy-link-{}", std::process::id()));
        let _ = fs::remove_file(&link);
        std::os::unix::fs::symlink(&home, &link).unwrap();
        let result = check_base(&link, own_principal());
        fs::remove_file(&link).unwrap();
        (link, result)
    }

    #[test]
    fn symlink_planted_in_shared_tmp_is_not_trusted_even_when_it_points_home() {
        let (link, result) = link_home_from(Path::new("/tmp"));
        let err = result.expect_err(&format!("{} was trusted", link.display()));
        assert!(err.contains("writable by other users"), "{err}");
    }

    #[test]
    fn symlink_in_a_private_dir_pointing_home_is_trusted() {
        let private = trusted_tempdir();
        let (link, result) = link_home_from(private.path());
        result.unwrap_or_else(|e| panic!("{}: {e}", link.display()));
    }

    #[test]
    fn own_symlink_into_a_trusted_dir_is_followed() {
        let a = trusted_tempdir();
        let b = trusted_tempdir();
        let link = a.path().join("link");
        std::os::unix::fs::symlink(b.path(), &link).unwrap();
        check_base(&link, own_principal()).unwrap();
        fs::set_permissions(b.path(), Permissions::from_mode(0o777)).unwrap();
        let err = check_base(&link.join("sub"), own_principal()).unwrap_err();
        assert!(err.contains("writable by other users"), "{err}");
    }
}
