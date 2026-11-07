//! Local IPC between `dstreamy serve` and its clients: the transport and the wire codecs.

mod auth;
pub mod control;
mod error;
pub mod producer;
#[cfg(unix)]
mod unix;
#[cfg(windows)]
mod windows;

#[cfg(unix)]
use unix as sys;
#[cfg(windows)]
use windows as sys;

#[cfg(not(any(unix, windows)))]
compile_error!("dstreamy-ipc has no transport for this platform");

use std::fmt;
use std::io;
use std::path::{Path, PathBuf};
use std::pin::Pin;
use std::sync::atomic::{AtomicU64, Ordering};
use std::task::{Context, Poll};

use tokio::io::{AsyncRead, AsyncWrite, ReadBuf};

pub use auth::authorize;
pub use control::{ControlCodec, Malformed};
pub use error::IpcError;
pub use producer::{Frame, Kind, ProducerCodec, ProducerError};

/// Where the service listens: a socket path on Unix, a named pipe on Windows.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Endpoint {
    location: PathBuf,
}

impl Endpoint {
    /// Uses an explicit path; normal bind, accept and connect still enforce directory and peer trust.
    pub fn new(location: impl Into<PathBuf>) -> Self {
        Self {
            location: location.into(),
        }
    }

    /// Selects a trusted per-user socket path, falling back under home when the runtime path is unusable.
    pub fn default_for_user() -> Result<Self, IpcError> {
        sys::default_location().map(Self::new)
    }

    /// Where `serve` listens by default in a process whose runtime directory is `runtime_dir`.
    pub fn default_for_runtime_dir(
        runtime_dir: Option<&std::ffi::OsStr>,
    ) -> Result<Self, IpcError> {
        sys::default_location_in(runtime_dir).map(Self::new)
    }

    pub fn as_path(&self) -> &Path {
        &self.location
    }
}

impl fmt::Display for Endpoint {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.location.display().fmt(f)
    }
}

/// The connecting process as the kernel reported it at connect time.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Peer {
    principal: sys::Principal,
    pid: Option<u32>,
}

impl Peer {
    pub(crate) fn new(principal: sys::Principal, pid: Option<u32>) -> Self {
        Self { principal, pid }
    }

    pub(crate) fn principal(&self) -> sys::Principal {
        self.principal
    }

    /// Kernel-reported PID when available; peer authorization uses user credentials instead.
    pub fn pid(&self) -> Option<u32> {
        self.pid
    }

    pub fn is_current_user(&self) -> bool {
        authorize(&sys::own_principal(), &self.principal)
    }
}

/// The service side of the socket: binds in the per-user directory and drops peers of other users before reading (ADR 0004 IPC).
pub struct Listener {
    inner: sys::Listener,
    endpoint: Endpoint,
    own: sys::Principal,
    rejected: AtomicU64,
}

impl Listener {
    /// Binds privately, refusing unsafe directories or an already-running listener.
    pub async fn bind(endpoint: &Endpoint) -> Result<Self, IpcError> {
        Self::bind_with(endpoint, sys::Access::Private, sys::own_principal()).await
    }

    /// Opens the endpoint to every user so tests can prove [`authorize`] alone refuses others.
    #[cfg(any(test, feature = "test-support"))]
    #[doc(hidden)]
    pub async fn bind_permissive_for_tests(endpoint: &Endpoint) -> Result<Self, IpcError> {
        Self::bind_with(
            endpoint,
            sys::Access::EveryoneForTests,
            sys::own_principal(),
        )
        .await
    }

    /// Binds normally but authorizes peers against `own` instead of this process's user.
    #[cfg(all(unix, any(test, feature = "test-support")))]
    #[doc(hidden)]
    pub async fn bind_as_for_tests(endpoint: &Endpoint, own: u32) -> Result<Self, IpcError> {
        Self::bind_with(endpoint, sys::Access::Private, own).await
    }

    async fn bind_with(
        endpoint: &Endpoint,
        access: sys::Access,
        own: sys::Principal,
    ) -> Result<Self, IpcError> {
        Ok(Self {
            inner: sys::Listener::bind(endpoint.as_path(), access).await?,
            endpoint: endpoint.clone(),
            own,
            rejected: AtomicU64::new(0),
        })
    }

    /// Waits for a connection from the current user, dropping others before any read; cancel safe.
    pub async fn accept(&self) -> io::Result<(Stream, Peer)> {
        loop {
            let (stream, peer) = self.inner.accept().await?;
            match peer {
                Ok(peer) if authorize(&self.own, &peer.principal()) => {
                    return Ok((Stream { inner: stream }, peer));
                }
                _ => {
                    self.rejected.fetch_add(1, Ordering::Relaxed);
                    drop(stream);
                }
            }
        }
    }

    pub fn rejected_peers(&self) -> u64 {
        self.rejected.load(Ordering::Relaxed)
    }

    pub fn endpoint(&self) -> &Endpoint {
        &self.endpoint
    }
}

/// Connects to a service at `endpoint`, refusing one that runs as another user.
pub async fn connect(endpoint: &Endpoint) -> Result<Stream, IpcError> {
    Ok(Stream {
        inner: sys::connect(endpoint.as_path(), sys::own_principal()).await?,
    })
}

/// Connects normally but expects the server to run as `own` instead of this process's user.
#[cfg(all(unix, any(test, feature = "test-support")))]
#[doc(hidden)]
pub async fn connect_as_for_tests(endpoint: &Endpoint, own: u32) -> Result<Stream, IpcError> {
    Ok(Stream {
        inner: sys::connect(endpoint.as_path(), own).await?,
    })
}

/// Refuses a server whose process the kernel cannot attribute, or that runs as another user.
pub(crate) fn verify_server(
    path: &Path,
    own: sys::Principal,
    server: io::Result<Peer>,
) -> Result<(), IpcError> {
    let server = server.map_err(|e| IpcError::UnverifiedServer {
        path: path.to_owned(),
        reason: e.to_string(),
    })?;
    if !authorize(&own, &server.principal()) {
        return Err(IpcError::ImpostorServer {
            path: path.to_owned(),
            owner: sys::owner_name(server.principal()),
        });
    }
    Ok(())
}

/// One accepted or connected socket, usable with tokio's `AsyncRead` and `AsyncWrite`.
pub struct Stream {
    inner: sys::Stream,
}

impl AsyncRead for Stream {
    fn poll_read(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut ReadBuf<'_>,
    ) -> Poll<io::Result<()>> {
        Pin::new(&mut self.get_mut().inner).poll_read(cx, buf)
    }
}

impl AsyncWrite for Stream {
    fn poll_write(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &[u8],
    ) -> Poll<io::Result<usize>> {
        Pin::new(&mut self.get_mut().inner).poll_write(cx, buf)
    }

    fn poll_flush(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        Pin::new(&mut self.get_mut().inner).poll_flush(cx)
    }

    fn poll_shutdown(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        Pin::new(&mut self.get_mut().inner).poll_shutdown(cx)
    }
}
