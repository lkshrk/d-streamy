use std::ffi::{CStr, OsStr};
use std::fs::{self, DirBuilder, File, OpenOptions, Permissions, TryLockError};
use std::io::{self, ErrorKind};
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::{DirBuilderExt, FileTypeExt, MetadataExt, OpenOptionsExt, PermissionsExt};
use std::path::{Component, Path, PathBuf};

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
    default_location_in(std::env::var_os(RUNTIME_DIR_VAR).as_deref())
}

pub(crate) fn default_location_in(runtime_dir: Option<&OsStr>) -> Result<PathBuf, IpcError> {
    let uid = own_principal();
    resolve_location(
        runtime_dir,
        passwd_home(uid).as_deref(),
        |base| check_base(base, uid),
        Path::is_dir,
    )
}

pub(crate) fn resolve_location(
    runtime_dir: Option<&OsStr>,
    home: Option<&Path>,
    trust: impl Fn(&Path) -> Result<PathBuf, String>,
    exists: impl Fn(&Path) -> bool,
) -> Result<PathBuf, IpcError> {
    let in_dir = |dir: &Path| dir.join(DIR_NAME).join(SOCKET_NAME);
    let runtime = runtime_dir
        .map(Path::new)
        .filter(|dir| dir.is_absolute())
        .and_then(|dir| trust(dir).ok())
        .filter(|dir| exists(dir))
        .map(|dir| in_dir(&dir))
        .filter(|path| fits_sun_path(path));
    if let Some(path) = runtime {
        return Ok(path);
    }
    let home = home
        .filter(|home| home.is_absolute())
        .ok_or(IpcError::NoHomeDirectory)?;
    let base = home.join(HOME_FALLBACK);
    let resolved = trust(&base).map_err(|reason| IpcError::InsecureDirectory {
        path: base.clone(),
        reason,
    })?;
    let path = in_dir(&resolved);
    if !fits_sun_path(&path) {
        return Err(IpcError::PathTooLong(path));
    }
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

/// Checks every existing component (ours or root's, writable by no one else) and returns the resolved path to use.
pub(crate) fn check_base(base: &Path, uid: u32) -> Result<PathBuf, String> {
    if base.components().any(|c| c == Component::ParentDir) {
        return Err(format!("{} contains ..", base.display()));
    }
    check_components(base, uid, &mut 0)
}

/// Total symlinks followed per check, as Linux's MAXSYMLINKS; bounds fan-out across nested targets.
const MAX_SYMLINKS: u32 = 40;

fn check_components(path: &Path, uid: u32, links: &mut u32) -> Result<PathBuf, String> {
    if !path.is_absolute() {
        return Err(format!("{} is not absolute", path.display()));
    }
    let mut current = PathBuf::new();
    let mut parts = path.components();
    while let Some(part) = parts.next() {
        if part == Component::ParentDir {
            if !current.pop() || current.as_os_str().is_empty() {
                return Err(format!("{} climbs above /", path.display()));
            }
            continue;
        }
        current.push(part);
        let meta = match fs::symlink_metadata(&current) {
            Ok(meta) => meta,
            Err(e) if e.kind() == ErrorKind::NotFound => {
                for rest in parts {
                    if rest == Component::ParentDir {
                        return Err(format!(
                            "{} has .. after a missing component",
                            path.display()
                        ));
                    }
                    current.push(rest);
                }
                return Ok(current);
            }
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
            *links += 1;
            if *links > MAX_SYMLINKS {
                return Err(format!("{}: too many symbolic links", path.display()));
            }
            current = check_components(&target, uid, links)?;
            if !current.is_dir() {
                return Err(format!("{} does not lead to a directory", target.display()));
            }
            continue;
        }
        if !meta.is_dir() {
            return Err(format!("{} is not a directory", current.display()));
        }
        if meta.mode() & 0o022 != 0 {
            return Err(format!("{} is writable by other users", current.display()));
        }
    }
    Ok(current)
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
struct DirLock(File);

impl Drop for DirLock {
    fn drop(&mut self) {
        // Closing alone leaves it held while a child forked before its exec still has the descriptor.
        let _ = self.0.unlock();
    }
}

fn lock_dir(dir: &Path, socket: &Path) -> Result<DirLock, IpcError> {
    let file = OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .mode(0o600)
        .custom_flags(libc::O_NOFOLLOW)
        .open(dir.join(LOCK_NAME))?;
    match file.try_lock() {
        Ok(()) => Ok(DirLock(file)),
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
        resolve_location(
            runtime.map(OsStr::new),
            Some(Path::new(HOME)),
            |b| Ok(b.to_owned()),
            |_| true,
        )
        .unwrap()
    }

    /// `path` with its longest existing prefix canonicalized, the form `check_base` returns.
    fn resolved(path: &Path) -> PathBuf {
        let existing = path.ancestors().find(|p| p.exists()).unwrap();
        let rest = path.strip_prefix(existing).unwrap();
        fs::canonicalize(existing).unwrap().join(rest)
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
        let err =
            resolve_location(None, Some(&home), |b| Ok(b.to_owned()), Path::is_dir).unwrap_err();
        assert!(matches!(err, IpcError::PathTooLong(p) if p.starts_with(&home)));
    }

    #[test]
    fn no_home_and_no_runtime_dir_is_an_error() {
        let err = resolve_location(None, None, |b| Ok(b.to_owned()), Path::is_dir).unwrap_err();
        assert!(matches!(err, IpcError::NoHomeDirectory), "{err}");
        let err = resolve_location(
            None,
            Some(Path::new("relative")),
            |b| Ok(b.to_owned()),
            Path::is_dir,
        )
        .unwrap_err();
        assert!(matches!(err, IpcError::NoHomeDirectory), "{err}");
    }

    #[test]
    fn a_dropped_dir_lock_is_free_while_a_copy_of_its_descriptor_lives() {
        let tmp = tempfile::tempdir().unwrap();
        let socket = tmp.path().join("serve.sock");
        let held = lock_dir(tmp.path(), &socket).unwrap();
        let forked_copy = held.0.try_clone().unwrap();
        drop(held);
        lock_dir(tmp.path(), &socket).unwrap();
        drop(forked_copy);
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
        let path = resolve_location(
            Some(OsStr::new("/tmp")),
            Some(home.path()),
            |b| check_base(b, uid),
            Path::is_dir,
        )
        .unwrap();
        assert_eq!(
            path,
            resolved(&home.path().join(HOME_FALLBACK).join("dstreamy/serve.sock"))
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
        let path = resolve_location(
            Some(runtime.path().as_os_str()),
            Some(home),
            |b| {
                if b == runtime.path() {
                    check_base(b, uid.wrapping_add(1))
                } else {
                    Ok(b.to_owned())
                }
            },
            Path::is_dir,
        )
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
        let err = resolve_location(
            None,
            Some(Path::new("/home/u")),
            |_| Err("nope".into()),
            Path::is_dir,
        )
        .unwrap_err();
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

    fn world_writable_tempdir() -> tempfile::TempDir {
        let dir = trusted_tempdir();
        fs::set_permissions(dir.path(), Permissions::from_mode(0o777)).unwrap();
        dir
    }

    #[test]
    fn symlink_from_a_private_dir_into_a_writable_dir_is_refused() {
        let private = trusted_tempdir();
        let writable = world_writable_tempdir();
        let trusted = trusted_tempdir();
        let link = private.path().join("state");
        std::os::unix::fs::symlink(writable.path(), &link).unwrap();
        let err = check_base(&link, own_principal()).unwrap_err();
        assert!(err.contains("writable by other users"), "{err}");
        fs::remove_file(&link).unwrap();
        std::os::unix::fs::symlink(trusted.path(), &link).unwrap();
        check_base(&link, own_principal()).unwrap();
    }

    #[test]
    fn relative_dotdot_link_to_a_trusted_dir_is_accepted() {
        let root = trusted_tempdir();
        let dotfiles = root.path().join("dotfiles/state");
        DirBuilder::new()
            .recursive(true)
            .mode(0o755)
            .create(&dotfiles)
            .unwrap();
        let local = root.path().join("home/.local");
        DirBuilder::new()
            .recursive(true)
            .mode(0o755)
            .create(&local)
            .unwrap();
        std::os::unix::fs::symlink("../../dotfiles/state", local.join("state")).unwrap();
        check_base(&local.join("state"), own_principal()).unwrap();
    }

    #[test]
    fn relative_link_resolving_into_a_writable_dir_is_refused() {
        let root = trusted_tempdir();
        let shared = root.path().join("shared");
        DirBuilder::new().mode(0o777).create(&shared).unwrap();
        fs::set_permissions(&shared, Permissions::from_mode(0o777)).unwrap();
        fs::create_dir(shared.join("state")).unwrap();
        let local = root.path().join("home");
        DirBuilder::new().mode(0o755).create(&local).unwrap();
        std::os::unix::fs::symlink("../shared/state", local.join("state")).unwrap();
        let err = check_base(&local.join("state"), own_principal()).unwrap_err();
        assert!(err.contains("writable by other users"), "{err}");
    }

    #[test]
    fn dotdot_chain_through_a_writable_dir_is_refused() {
        let root = trusted_tempdir();
        let shared = root.path().join("shared");
        DirBuilder::new().mode(0o777).create(&shared).unwrap();
        fs::set_permissions(&shared, Permissions::from_mode(0o777)).unwrap();
        fs::create_dir(root.path().join("safe")).unwrap();
        let link = root.path().join("link");
        std::os::unix::fs::symlink("shared/../safe", &link).unwrap();
        let err = check_base(&link, own_principal()).unwrap_err();
        assert!(err.contains("writable by other users"), "{err}");
        let up = root.path().join("up");
        std::os::unix::fs::symlink("/../../tmp/x", &up).unwrap();
        let err = check_base(&up, own_principal()).unwrap_err();
        assert!(err.contains("climbs above /"), "{err}");
    }

    #[test]
    fn dotdot_in_the_base_itself_is_refused() {
        let home = passwd_home(own_principal()).unwrap();
        let probe = home.join("no-such-dir-xyz/../../../tmp");
        let err = check_base(&probe, own_principal()).unwrap_err();
        assert!(err.contains("contains .."), "{err}");
        let uid = own_principal();
        let fallback = trusted_tempdir();
        let path = resolve_location(
            Some(probe.as_os_str()),
            Some(fallback.path()),
            |b| check_base(b, uid),
            Path::is_dir,
        )
        .unwrap();
        assert!(!path.starts_with("/tmp"), "{}", path.display());
        assert!(
            path.components().all(|c| c != Component::ParentDir),
            "{}",
            path.display()
        );
    }

    #[test]
    fn dotdot_after_a_missing_component_is_refused_inside_a_link() {
        let root = trusted_tempdir();
        let writable = root.path().join("writable");
        DirBuilder::new().mode(0o777).create(&writable).unwrap();
        fs::set_permissions(&writable, Permissions::from_mode(0o777)).unwrap();
        let trusted = root.path().join("trusted");
        fs::create_dir(&trusted).unwrap();
        let link = root.path().join("link");
        std::os::unix::fs::symlink("trusted/missing/../../writable", &link).unwrap();
        let err = check_base(&link, own_principal()).unwrap_err();
        assert!(err.contains("after a missing component"), "{err}");
        let err = check_base(&trusted.join("missing/../../writable"), own_principal()).unwrap_err();
        assert!(err.contains("contains .."), "{err}");
    }

    #[test]
    fn missing_private_dir_under_a_trusted_base_resolves_and_binds() {
        let root = trusted_tempdir();
        let link = root.path().join("link");
        std::os::unix::fs::symlink(root.path(), &link).unwrap();
        let resolved = check_base(&link.join("not-yet/deeper"), own_principal()).unwrap();
        assert_eq!(resolved, root.path().join("not-yet/deeper"));
        let socket = resolved.join(DIR_NAME).join(SOCKET_NAME);
        prepare_private_dir(socket_dir(&socket)).unwrap();
        check_private_dir(socket_dir(&socket), own_principal()).unwrap();
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

    #[test]
    fn resolved_runtime_dir_is_what_resolve_location_returns() {
        let root = trusted_tempdir();
        let real = root.path().join("real");
        fs::create_dir(&real).unwrap();
        let link = root.path().join("link");
        std::os::unix::fs::symlink("real", &link).unwrap();
        let uid = own_principal();
        let path = resolve_location(
            Some(link.as_os_str()),
            Some(Path::new("/home/u")),
            |b| check_base(b, uid),
            Path::is_dir,
        )
        .unwrap();
        assert_eq!(path, resolved(&real.join(DIR_NAME).join(SOCKET_NAME)));
        let home = trusted_tempdir();
        let state = home.path().join(HOME_FALLBACK);
        fs::create_dir_all(state.parent().unwrap()).unwrap();
        std::os::unix::fs::symlink(&real, &state).unwrap();
        let path = resolve_location(
            None,
            Some(home.path()),
            |b| check_base(b, uid),
            Path::is_dir,
        )
        .unwrap();
        assert_eq!(path, resolved(&real.join(DIR_NAME).join(SOCKET_NAME)));
    }

    /// `D/e{n}` points at `D/e{n-1}` repeated `fan` times and `e0` at `D`: 1 + fan·cost(n-1) links per level.
    fn link_tree(root: &Path, levels: usize, fan: usize) -> PathBuf {
        let dir = root.join("D");
        fs::create_dir(&dir).unwrap();
        std::os::unix::fs::symlink(&dir, dir.join("e0")).unwrap();
        for n in 1..=levels {
            let target = (0..fan).fold(dir.clone(), |p, _| p.join(format!("e{}", n - 1)));
            std::os::unix::fs::symlink(target, dir.join(format!("e{n}"))).unwrap();
        }
        dir.join(format!("e{levels}"))
    }

    #[test]
    fn symlink_total_is_bounded_across_nesting_levels() {
        let root = trusted_tempdir();
        let wide = link_tree(root.path(), 7, 6);
        let started = std::time::Instant::now();
        let err = check_base(&wide, own_principal()).unwrap_err();
        assert!(err.contains("too many symbolic links"), "{err}");
        assert!(
            started.elapsed() < std::time::Duration::from_millis(200),
            "{:?}",
            started.elapsed()
        );
        let small = trusted_tempdir();
        let narrow = link_tree(small.path(), 2, 3);
        assert_eq!(
            check_base(&narrow, own_principal()).unwrap(),
            small.path().join("D")
        );
    }

    #[test]
    fn missing_runtime_dir_falls_back_to_the_home_base() {
        let parent = trusted_tempdir();
        let missing = parent.path().join("1000");
        let home = trusted_tempdir();
        let expected = resolved(&home.path().join(HOME_FALLBACK).join("dstreamy/serve.sock"));
        let uid = own_principal();
        let resolve = || {
            resolve_location(
                Some(missing.as_os_str()),
                Some(home.path()),
                |b| check_base(b, uid),
                Path::is_dir,
            )
            .unwrap()
        };
        assert_eq!(resolve(), expected);
        fs::write(&missing, b"").unwrap();
        assert_eq!(resolve(), expected, "a file was used as the runtime dir");
    }
}
