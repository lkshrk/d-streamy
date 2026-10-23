//! Local IPC between `dstreamy serve` and its clients: the transport and the wire codecs.

mod auth;
pub mod control;
mod error;
pub mod producer;
#[cfg(unix)]
mod unix;

#[cfg(unix)]
use unix as sys;

#[cfg(not(unix))]
compile_error!("dstreamy-ipc has no transport for this platform yet (named pipes: issue 15)");

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

/// Where the service listens: a socket path on Unix.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Endpoint {
    location: PathBuf,
}

impl Endpoint {
    pub fn new(location: impl Into<PathBuf>) -> Self {
        Self {
            location: location.into(),
        }
    }

    pub fn default_for_user() -> Result<Self, IpcError> {
        sys::default_location().map(Self::new)
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

    pub fn pid(&self) -> Option<u32> {
        self.pid
    }

    pub fn is_current_user(&self) -> bool {
        authorize(&sys::own_principal(), &self.principal)
    }
}

pub struct Listener {
    inner: sys::Listener,
    endpoint: Endpoint,
    own: sys::Principal,
    rejected: AtomicU64,
}

impl Listener {
    pub async fn bind(endpoint: &Endpoint) -> Result<Self, IpcError> {
        Self::bind_with(endpoint, sys::Access::Private).await
    }

    /// Opens the endpoint to every user so tests can prove [`authorize`] alone refuses others.
    #[cfg(feature = "test-support")]
    #[doc(hidden)]
    pub async fn bind_permissive_for_tests(endpoint: &Endpoint) -> Result<Self, IpcError> {
        Self::bind_with(endpoint, sys::Access::EveryoneForTests).await
    }

    async fn bind_with(endpoint: &Endpoint, access: sys::Access) -> Result<Self, IpcError> {
        Ok(Self {
            inner: sys::Listener::bind(endpoint.as_path(), access).await?,
            endpoint: endpoint.clone(),
            own: sys::own_principal(),
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

pub async fn connect(endpoint: &Endpoint) -> Result<Stream, IpcError> {
    Ok(Stream {
        inner: sys::connect(endpoint.as_path()).await?,
    })
}

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
