use std::os::fd::OwnedFd;

use ashpd::desktop::PersistMode;
use ashpd::desktop::Session;
use ashpd::desktop::screencast::{
    CursorMode, OpenPipeWireRemoteOptions, Screencast, SelectSourcesOptions, SourceType,
};
use ashpd::enumflags2::BitFlags;
use dstreamy_media::MediaError;
use tokio::runtime::Handle;

/// What the ScreenCast portal dialog offers the user to pick.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum PortalSources {
    Monitor,
    Window,
    #[default]
    MonitorOrWindow,
}

impl PortalSources {
    fn flags(self) -> BitFlags<SourceType> {
        match self {
            Self::Monitor => SourceType::Monitor.into(),
            Self::Window => SourceType::Window.into(),
            Self::MonitorOrWindow => SourceType::Monitor | SourceType::Window,
        }
    }
}

/// An open ScreenCast session; the stream lives until [`PortalStream::close`] or drop.
pub(crate) struct PortalStream {
    proxy: Screencast,
    session: Option<Session<Screencast>>,
    runtime: Handle,
    pub(crate) node_id: u32,
    /// The stream size the portal reports, in compositor coordinates that may differ from frame pixels.
    pub(crate) size: Option<(u32, u32)>,
    pub(crate) restore_token: Option<String>,
}

fn portal_err(context: &str) -> impl FnOnce(ashpd::Error) -> MediaError + '_ {
    move |e| MediaError::Backend(format!("ScreenCast portal {context}: {e}"))
}

impl PortalStream {
    /// Asks the portal for one stream and its PipeWire fd; shows the picker unless `restore_token` is still valid.
    pub(crate) async fn open(
        sources: PortalSources,
        restore_token: Option<&str>,
    ) -> Result<(Self, OwnedFd), MediaError> {
        let proxy = Screencast::new().await.map_err(portal_err("connect"))?;
        let session = proxy
            .create_session(Default::default())
            .await
            .map_err(portal_err("create session"))?;
        let mut stream = Self {
            proxy,
            session: Some(session),
            runtime: Handle::current(),
            node_id: 0,
            size: None,
            restore_token: None,
        };
        let negotiated = stream.negotiate(sources, restore_token).await;
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
        sources: PortalSources,
        restore_token: Option<&str>,
    ) -> Result<(OwnedFd, u32, Option<(u32, u32)>, Option<String>), MediaError> {
        let proxy = &self.proxy;
        let session = self
            .session
            .as_ref()
            .expect("session is open until close or drop");
        let options = SelectSourcesOptions::default()
            .set_cursor_mode(CursorMode::Embedded)
            .set_sources(sources.flags())
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

    pub(crate) async fn close(mut self) {
        if let Some(session) = &self.session {
            // Ignored: the compositor may have ended the session already.
            let _ = session.close().await;
        }
        self.session = None;
    }
}

impl Drop for PortalStream {
    fn drop(&mut self) {
        if let Some(session) = self.session.take() {
            // Ignored as in `close`; a runtime that already shut down cancels the task.
            self.runtime.spawn(async move {
                let _ = session.close().await;
            });
        }
    }
}
