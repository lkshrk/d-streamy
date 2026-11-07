use std::ffi::{OsStr, OsString, c_void};
use std::fmt;
use std::io;
use std::os::windows::ffi::{OsStrExt, OsStringExt};
use std::os::windows::io::AsRawHandle;
use std::path::{Path, PathBuf};
use std::pin::Pin;
use std::sync::OnceLock;
use std::task::{Context, Poll};
use std::time::{Duration, Instant};

use tokio::io::{AsyncRead, AsyncWrite, ReadBuf};
use tokio::net::windows::named_pipe::{
    ClientOptions, NamedPipeClient, NamedPipeServer, ServerOptions,
};
use windows_sys::Win32::Foundation::{
    CloseHandle, ERROR_ACCESS_DENIED, ERROR_FILE_NOT_FOUND, ERROR_PIPE_BUSY, HANDLE, LocalFree,
};
use windows_sys::Win32::Security::Authorization::{
    ConvertSidToStringSidW, ConvertStringSecurityDescriptorToSecurityDescriptorW, SDDL_REVISION_1,
};
use windows_sys::Win32::Security::{
    GetLengthSid, GetTokenInformation, PSECURITY_DESCRIPTOR, SECURITY_ATTRIBUTES,
    SECURITY_MAX_SID_SIZE, TOKEN_QUERY, TOKEN_USER, TokenUser,
};
use windows_sys::Win32::System::Pipes::{GetNamedPipeClientProcessId, GetNamedPipeServerProcessId};
use windows_sys::Win32::System::Threading::{
    GetCurrentProcess, OpenProcess, OpenProcessToken, PROCESS_QUERY_LIMITED_INFORMATION,
};

use crate::{IpcError, Peer};

const PIPE_PREFIX: &str = r"\\.\pipe\";
const PIPE_NAME: &str = "dstreamy-serve";
const BUSY_RETRY: Duration = Duration::from_millis(20);
const BUSY_WAIT: Duration = Duration::from_secs(5);

/// A user's SID, copied out of its token so a [`Peer`] stays `Copy`.
#[derive(Clone, Copy, PartialEq, Eq)]
pub(crate) struct Principal {
    len: u8,
    bytes: [u8; SECURITY_MAX_SID_SIZE as usize],
}

impl Principal {
    /// # Safety
    /// `sid` must point at a valid SID.
    unsafe fn copy_from(sid: *mut c_void) -> io::Result<Self> {
        // SAFETY: the caller guarantees a valid SID.
        let len = unsafe { GetLengthSid(sid) } as usize;
        if len == 0 || len > SECURITY_MAX_SID_SIZE as usize {
            return Err(io::Error::other(format!("unexpected SID length {len}")));
        }
        let mut bytes = [0; SECURITY_MAX_SID_SIZE as usize];
        // SAFETY: GetLengthSid reported `len` readable bytes at `sid`.
        bytes[..len].copy_from_slice(unsafe { std::slice::from_raw_parts(sid.cast::<u8>(), len) });
        Ok(Self {
            len: len as u8,
            bytes,
        })
    }

    fn as_string(&self) -> String {
        let mut wide = std::ptr::null_mut();
        let mut sid = self.bytes;
        // SAFETY: `sid` holds a valid SID copied from a token; the call allocates `wide` with LocalAlloc.
        if unsafe { ConvertSidToStringSidW(sid.as_mut_ptr().cast(), &mut wide) } == 0 {
            return format!("an unreadable SID ({})", io::Error::last_os_error());
        }
        // SAFETY: on success `wide` is a NUL-terminated UTF-16 string owned by us.
        let len = (0..).take_while(|&i| unsafe { *wide.add(i) } != 0).count();
        // SAFETY: `len` UTF-16 units precede the terminator.
        let text = OsString::from_wide(unsafe { std::slice::from_raw_parts(wide, len) });
        // SAFETY: `wide` came from LocalAlloc inside ConvertSidToStringSidW.
        unsafe { LocalFree(wide.cast()) };
        text.to_string_lossy().into_owned()
    }
}

impl fmt::Debug for Principal {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.as_string())
    }
}

impl fmt::Display for Principal {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.as_string())
    }
}

struct OwnedHandle(HANDLE);

impl Drop for OwnedHandle {
    fn drop(&mut self) {
        // SAFETY: the handle was opened by us and is closed once.
        unsafe { CloseHandle(self.0) };
    }
}

/// The user SID of the token of `process`.
fn token_user(process: HANDLE) -> io::Result<Principal> {
    let mut token = std::ptr::null_mut();
    // SAFETY: `process` is a valid process handle; `token` receives a new handle.
    if unsafe { OpenProcessToken(process, TOKEN_QUERY, &mut token) } == 0 {
        return Err(io::Error::last_os_error());
    }
    let token = OwnedHandle(token);
    let mut buf = vec![0u64; 16];
    loop {
        let size = (buf.len() * 8) as u32;
        let mut needed = 0;
        // SAFETY: `buf` is writable for `size` bytes and 8-byte aligned, as TOKEN_USER needs.
        let ok = unsafe {
            GetTokenInformation(
                token.0,
                TokenUser,
                buf.as_mut_ptr().cast(),
                size,
                &mut needed,
            )
        };
        if ok != 0 {
            break;
        }
        let err = io::Error::last_os_error();
        if needed > size && buf.len() < 1024 {
            buf.resize(needed.div_ceil(8) as usize, 0);
            continue;
        }
        return Err(err);
    }
    // SAFETY: GetTokenInformation(TokenUser) filled `buf` with a TOKEN_USER whose SID points into `buf`.
    unsafe { Principal::copy_from((*buf.as_ptr().cast::<TOKEN_USER>()).User.Sid) }
}

fn process_user(pid: u32) -> io::Result<Principal> {
    // SAFETY: OpenProcess has no pointer arguments; a null result is checked.
    let process = unsafe { OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, 0, pid) };
    if process.is_null() {
        return Err(io::Error::last_os_error());
    }
    let process = OwnedHandle(process);
    token_user(process.0)
}

pub(crate) fn own_principal() -> Principal {
    static OWN: OnceLock<Principal> = OnceLock::new();
    // SAFETY: GetCurrentProcess returns a pseudo handle that needs no closing.
    *OWN.get_or_init(|| {
        token_user(unsafe { GetCurrentProcess() }).expect("this process's token has a user SID")
    })
}

pub(crate) fn owner_name(principal: Principal) -> String {
    principal.to_string()
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Access {
    Private,
    #[cfg(any(test, feature = "test-support"))]
    EveryoneForTests,
}

/// One pipe per user: named pipes share a global namespace, so the SID keeps users apart.
pub(crate) fn default_location() -> Result<PathBuf, IpcError> {
    Ok(PathBuf::from(format!(
        "{PIPE_PREFIX}{PIPE_NAME}-{}",
        own_principal()
    )))
}

/// Windows has no runtime directory; the pipe name depends on the user alone.
pub(crate) fn default_location_in(_runtime_dir: Option<&OsStr>) -> Result<PathBuf, IpcError> {
    default_location()
}

fn check_pipe_path(path: &Path) -> Result<(), IpcError> {
    let ok = path
        .to_str()
        .and_then(|p| p.strip_prefix(PIPE_PREFIX))
        .is_some_and(|name| !name.is_empty() && !name.contains(['\\', '/']));
    if ok {
        Ok(())
    } else {
        Err(IpcError::InsecureDirectory {
            path: path.to_owned(),
            reason: format!("an endpoint on Windows is a named pipe, {PIPE_PREFIX}<name>"),
        })
    }
}

/// A security descriptor from SDDL, freed with the listener.
struct SecurityDescriptor(PSECURITY_DESCRIPTOR);

// SAFETY: the descriptor is immutable after creation and only read by CreateNamedPipeW.
unsafe impl Send for SecurityDescriptor {}
// SAFETY: as above; concurrent reads of an immutable descriptor are fine.
unsafe impl Sync for SecurityDescriptor {}

impl SecurityDescriptor {
    fn from_sddl(sddl: &str) -> io::Result<Self> {
        let wide: Vec<u16> = OsStr::new(sddl).encode_wide().chain(Some(0)).collect();
        let mut descriptor = std::ptr::null_mut();
        // SAFETY: `wide` is NUL-terminated; the call allocates `descriptor` with LocalAlloc.
        let ok = unsafe {
            ConvertStringSecurityDescriptorToSecurityDescriptorW(
                wide.as_ptr(),
                SDDL_REVISION_1,
                &mut descriptor,
                std::ptr::null_mut(),
            )
        };
        if ok == 0 {
            return Err(io::Error::last_os_error());
        }
        Ok(Self(descriptor))
    }
}

impl Drop for SecurityDescriptor {
    fn drop(&mut self) {
        // SAFETY: allocated by ConvertStringSecurityDescriptorToSecurityDescriptorW, freed once.
        unsafe { LocalFree(self.0) };
    }
}

pub(crate) enum Stream {
    Server(NamedPipeServer),
    Client(NamedPipeClient),
}

impl AsyncRead for Stream {
    fn poll_read(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut ReadBuf<'_>,
    ) -> Poll<io::Result<()>> {
        match self.get_mut() {
            Stream::Server(s) => Pin::new(s).poll_read(cx, buf),
            Stream::Client(c) => Pin::new(c).poll_read(cx, buf),
        }
    }
}

impl AsyncWrite for Stream {
    fn poll_write(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &[u8],
    ) -> Poll<io::Result<usize>> {
        match self.get_mut() {
            Stream::Server(s) => Pin::new(s).poll_write(cx, buf),
            Stream::Client(c) => Pin::new(c).poll_write(cx, buf),
        }
    }

    fn poll_flush(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        match self.get_mut() {
            Stream::Server(s) => Pin::new(s).poll_flush(cx),
            Stream::Client(c) => Pin::new(c).poll_flush(cx),
        }
    }

    fn poll_shutdown(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        match self.get_mut() {
            Stream::Server(s) => Pin::new(s).poll_shutdown(cx),
            Stream::Client(c) => Pin::new(c).poll_shutdown(cx),
        }
    }
}

pub(crate) struct Listener {
    path: PathBuf,
    descriptor: SecurityDescriptor,
    /// The instance the next client connects to; replaced as soon as one connects.
    next: tokio::sync::Mutex<NamedPipeServer>,
}

impl Listener {
    pub(crate) async fn bind(path: &Path, access: Access) -> Result<Self, IpcError> {
        check_pipe_path(path)?;
        let sddl = match access {
            Access::Private => format!("D:P(A;;GA;;;{})", own_principal()),
            #[cfg(any(test, feature = "test-support"))]
            Access::EveryoneForTests => "D:P(A;;GA;;;WD)".to_owned(),
        };
        let descriptor = SecurityDescriptor::from_sddl(&sddl)?;
        let first = match create_instance(path, &descriptor, true) {
            Ok(server) => server,
            // A pipe of that name exists, ours or another user's; joining it would share its clients.
            Err(e) if e.raw_os_error() == Some(ERROR_ACCESS_DENIED as i32) => {
                return Err(IpcError::AlreadyRunning(path.to_owned()));
            }
            Err(e) => return Err(e.into()),
        };
        Ok(Self {
            path: path.to_owned(),
            descriptor,
            next: tokio::sync::Mutex::new(first),
        })
    }

    pub(crate) async fn accept(&self) -> io::Result<(Stream, io::Result<Peer>)> {
        let mut next = self.next.lock().await;
        next.connect().await?;
        let fresh = create_instance(&self.path, &self.descriptor, false)?;
        let connected = std::mem::replace(&mut *next, fresh);
        let peer = client_of(&connected);
        Ok((Stream::Server(connected), peer))
    }
}

fn create_instance(
    path: &Path,
    descriptor: &SecurityDescriptor,
    first: bool,
) -> io::Result<NamedPipeServer> {
    let mut attributes = SECURITY_ATTRIBUTES {
        nLength: std::mem::size_of::<SECURITY_ATTRIBUTES>() as u32,
        lpSecurityDescriptor: descriptor.0,
        bInheritHandle: 0,
    };
    let mut options = ServerOptions::new();
    options
        .first_pipe_instance(first)
        .reject_remote_clients(true);
    // SAFETY: `attributes` is a valid SECURITY_ATTRIBUTES whose descriptor outlives the call.
    unsafe {
        options.create_with_security_attributes_raw(
            path,
            (&mut attributes as *mut SECURITY_ATTRIBUTES).cast(),
        )
    }
}

fn client_of(server: &NamedPipeServer) -> io::Result<Peer> {
    let mut pid = 0;
    // SAFETY: the handle belongs to a connected pipe instance; `pid` is a valid out-pointer.
    if unsafe { GetNamedPipeClientProcessId(server.as_raw_handle(), &mut pid) } == 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(Peer::new(process_user(pid)?, Some(pid)))
}

fn server_of(client: &NamedPipeClient) -> io::Result<Peer> {
    let mut pid = 0;
    // SAFETY: the handle belongs to an open pipe client; `pid` is a valid out-pointer.
    if unsafe { GetNamedPipeServerProcessId(client.as_raw_handle(), &mut pid) } == 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(Peer::new(process_user(pid)?, Some(pid)))
}

pub(crate) async fn connect(path: &Path, own: Principal) -> Result<Stream, IpcError> {
    check_pipe_path(path)?;
    let deadline = Instant::now() + BUSY_WAIT;
    let client = loop {
        match ClientOptions::new().open(path) {
            Ok(client) => break client,
            Err(e) if e.raw_os_error() == Some(ERROR_FILE_NOT_FOUND as i32) => {
                return Err(IpcError::NotRunning(path.to_owned()));
            }
            Err(e)
                if e.raw_os_error() == Some(ERROR_PIPE_BUSY as i32)
                    && Instant::now() < deadline =>
            {
                tokio::time::sleep(BUSY_RETRY).await;
            }
            Err(e) => return Err(e.into()),
        }
    };
    crate::verify_server(path, own, server_of(&client))?;
    Ok(Stream::Client(client))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_default_pipe_is_per_user() {
        let path = default_location().unwrap();
        let name = path.to_str().unwrap();
        assert!(name.starts_with(r"\\.\pipe\dstreamy-serve-S-1-"), "{name}");
        check_pipe_path(&path).unwrap();
    }

    #[test]
    fn only_named_pipe_paths_are_endpoints() {
        for bad in [
            r"C:\Users\u\serve.sock",
            r"\\.\pipe\",
            r"\\.\pipe\a\b",
            "serve",
        ] {
            assert!(check_pipe_path(Path::new(bad)).is_err(), "{bad}");
        }
    }

    #[test]
    fn own_principal_round_trips_through_its_string() {
        let own = own_principal();
        assert!(own.to_string().starts_with("S-1-"), "{own}");
        assert_eq!(own, own_principal());
    }

    fn unique_pipe() -> PathBuf {
        PathBuf::from(format!(
            r"\\.\pipe\dstreamy-test-{}-{}",
            std::process::id(),
            rand_suffix()
        ))
    }

    fn rand_suffix() -> u128 {
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    }

    #[tokio::test]
    async fn a_connection_is_authorized_and_carries_bytes_both_ways() {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        let path = unique_pipe();
        let listener = Listener::bind(&path, Access::Private).await.unwrap();
        let accept = tokio::spawn(async move {
            let (mut stream, peer) = listener.accept().await.unwrap();
            let peer = peer.unwrap();
            assert_eq!(peer.principal(), own_principal());
            assert_eq!(peer.pid(), Some(std::process::id()));
            let mut buf = [0; 4];
            stream.read_exact(&mut buf).await.unwrap();
            stream.write_all(&buf).await.unwrap();
            listener
        });
        let mut client = connect(&path, own_principal()).await.unwrap();
        client.write_all(b"ping").await.unwrap();
        let mut echoed = [0; 4];
        client.read_exact(&mut echoed).await.unwrap();
        assert_eq!(&echoed, b"ping");
        drop(accept.await.unwrap());
    }

    #[tokio::test]
    async fn a_second_listener_on_the_same_pipe_is_already_running() {
        let path = unique_pipe();
        let _first = Listener::bind(&path, Access::Private).await.unwrap();
        let err = Listener::bind(&path, Access::Private).await.err().unwrap();
        assert!(matches!(err, IpcError::AlreadyRunning(_)), "{err}");
    }

    #[tokio::test]
    async fn connecting_without_a_listener_is_not_running() {
        let err = connect(&unique_pipe(), own_principal())
            .await
            .err()
            .unwrap();
        assert!(matches!(err, IpcError::NotRunning(_)), "{err}");
    }

    #[tokio::test]
    async fn each_accept_leaves_a_fresh_instance_for_the_next_client() {
        let path = unique_pipe();
        let listener = std::sync::Arc::new(Listener::bind(&path, Access::Private).await.unwrap());
        let server = listener.clone();
        let accepted = tokio::spawn(async move {
            let mut streams = Vec::new();
            for _ in 0..3 {
                streams.push(server.accept().await.unwrap().0);
            }
            streams
        });
        let mut clients = Vec::new();
        for _ in 0..3 {
            clients.push(connect(&path, own_principal()).await.unwrap());
        }
        assert_eq!(accepted.await.unwrap().len(), 3);
    }
}
