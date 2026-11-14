use std::os::fd::OwnedFd;
use std::sync::Arc;

use ashpd::desktop::screencast::{
    CursorMode, OpenPipeWireRemoteOptions, Screencast, SelectSourcesOptions, SourceType,
};
use ashpd::desktop::{PersistMode, ResponseError, Session};
use dstreamy_media::MediaError;
use futures_util::StreamExt;
use tokio::runtime::Handle;
use tokio::sync::oneshot;
use tokio::task::JoinHandle;

/// An open ScreenCast session; the stream lives until [`PortalStream::close`] or drop.
pub(crate) struct PortalStream {
    proxy: Screencast,
    /// Shared with the task that watches for the session's `Closed` signal.
    session: Option<Arc<Session<Screencast>>>,
    runtime: Handle,
    /// Resolves when the compositor or the user ends the session; taken once by [`PortalStream::closed`].
    closed: Option<oneshot::Receiver<()>>,
    closed_watch: JoinHandle<()>,
    pub(crate) node_id: u32,
    /// The stream size the portal reports, in compositor coordinates that may differ from frame pixels.
    pub(crate) size: Option<(u32, u32)>,
    pub(crate) restore_token: Option<String>,
}

fn portal_err(context: &str) -> impl FnOnce(ashpd::Error) -> MediaError + '_ {
    move |e| match e {
        ashpd::Error::Response(ResponseError::Cancelled) => MediaError::Cancelled,
        e => MediaError::Backend(format!("ScreenCast portal {context}: {e}")),
    }
}

fn connect_err(e: ashpd::Error) -> MediaError {
    MediaError::Backend(format!(
        "ScreenCast portal connect: {e}; is xdg-desktop-portal running with a backend for your \
         desktop (xdg-desktop-portal-gnome, -kde, -wlr or -hyprland)?"
    ))
}

impl PortalStream {
    /// Asks the portal for one stream and its PipeWire fd; shows the picker unless `restore_token` is still valid.
    pub(crate) async fn open(restore_token: Option<&str>) -> Result<(Self, OwnedFd), MediaError> {
        let proxy = Screencast::new().await.map_err(connect_err)?;
        let session = proxy
            .create_session(Default::default())
            .await
            .map_err(portal_err("create session"))?;
        let session = Arc::new(session);
        let (closed_tx, closed_rx) = oneshot::channel();
        let (subscribed_tx, subscribed_rx) = oneshot::channel();
        let closed_watch = tokio::spawn({
            let session = Arc::clone(&session);
            async move {
                let signals = session.receive_closed().await;
                let _ = subscribed_tx.send(signals.as_ref().map(drop).map_err(|e| e.to_string()));
                let Ok(signals) = signals else {
                    return;
                };
                let mut signals = std::pin::pin!(signals);
                if signals.next().await.is_some() {
                    let _ = closed_tx.send(());
                }
            }
        });
        // Subscribed before the picker shows, so a session closed right after it is never missed.
        match subscribed_rx.await {
            Ok(Ok(())) => {}
            Ok(Err(e)) => {
                let _ = session.close().await;
                return Err(MediaError::Backend(format!(
                    "ScreenCast portal watch session: {e}"
                )));
            }
            Err(_) => {}
        }
        let mut stream = Self {
            proxy,
            session: Some(session),
            runtime: Handle::current(),
            closed: Some(closed_rx),
            closed_watch,
            node_id: 0,
            size: None,
            restore_token: None,
        };
        let negotiated = stream.negotiate(restore_token).await;
        match negotiated {
            Ok((fd, node_id, size, restore_token)) => {
                stream.node_id = node_id;
                stream.size = size;
                stream.restore_token = restore_token;
                Ok((stream, fd))
            }
            Err(e) => {
                stream.close().await;
                Err(e)
            }
        }
    }

    async fn negotiate(
        &self,
        restore_token: Option<&str>,
    ) -> Result<(OwnedFd, u32, Option<(u32, u32)>, Option<String>), MediaError> {
        let proxy = &self.proxy;
        let session = self
            .session
            .as_ref()
            .expect("session is open until close or drop");
        // Older portals lack the property; they all accept Embedded.
        let cursor_modes = proxy
            .available_cursor_modes()
            .await
            .unwrap_or_else(|_| CursorMode::Embedded.into());
        let cursor_mode = if cursor_modes.contains(CursorMode::Embedded) {
            CursorMode::Embedded
        } else {
            CursorMode::Hidden
        };
        let options = SelectSourcesOptions::default()
            .set_cursor_mode(cursor_mode)
            .set_sources(SourceType::Monitor | SourceType::Window)
            .set_multiple(false)
            .set_persist_mode(PersistMode::ExplicitlyRevoked)
            .set_restore_token(restore_token);
        proxy
            .select_sources(session, options)
            .await
            .map_err(portal_err("select sources"))?
            .response()
            .map_err(portal_err("select sources"))?;
        let streams = proxy
            .start(session, None, Default::default())
            .await
            .map_err(portal_err("start"))?
            .response()
            .map_err(portal_err("start"))?;
        let stream = streams
            .streams()
            .first()
            .ok_or_else(|| MediaError::Backend("ScreenCast portal returned no stream".into()))?;
        let node_id = stream.pipe_wire_node_id();
        let size = stream
            .size()
            .and_then(|(w, h)| Some((u32::try_from(w).ok()?, u32::try_from(h).ok()?)));
        let fd = proxy
            .open_pipe_wire_remote(session, OpenPipeWireRemoteOptions::default())
            .await
            .map_err(portal_err("open PipeWire remote"))?;
        Ok((
            fd,
            node_id,
            size,
            streams.restore_token().map(str::to_owned),
        ))
    }

    /// Resolves with `Ok` when the session ends on its own; `Err` once this stream is closed or dropped.
    pub(crate) fn closed(&mut self) -> Option<oneshot::Receiver<()>> {
        self.closed.take()
    }

    pub(crate) async fn close(mut self) {
        self.closed_watch.abort();
        if let Some(session) = &self.session {
            // Ignored: the compositor may have ended the session already.
            let _ = session.close().await;
        }
        self.session = None;
    }
}

impl Drop for PortalStream {
    fn drop(&mut self) {
        self.closed_watch.abort();
        if let Some(session) = self.session.take() {
            // Ignored as in `close`; a runtime that already shut down cancels the task.
            self.runtime.spawn(async move {
                let _ = session.close().await;
            });
        }
    }
}
