use std::os::fd::OwnedFd;

use ashpd::desktop::PersistMode;
use ashpd::desktop::Session;
use ashpd::desktop::screencast::{
    CursorMode, OpenPipeWireRemoteOptions, Screencast, SelectSourcesOptions, SourceType,
};
use ashpd::enumflags2::BitFlags;
use dstreamy_media::MediaError;

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

/// An open ScreenCast session; the stream lives until [`PortalStream::close`].
pub(crate) struct PortalStream {
    proxy: Screencast,
    session: Session<Screencast>,
    pub(crate) fd: OwnedFd,
    pub(crate) node_id: u32,
    pub(crate) restore_token: Option<String>,
}

fn portal_err(context: &str) -> impl FnOnce(ashpd::Error) -> MediaError + '_ {
    move |e| MediaError::Backend(format!("ScreenCast portal {context}: {e}"))
}

impl PortalStream {
    /// Asks the portal for one stream; shows the picker unless `restore_token` is still valid.
    pub(crate) async fn open(
        sources: PortalSources,
        restore_token: Option<&str>,
    ) -> Result<Self, MediaError> {
        let proxy = Screencast::new().await.map_err(portal_err("connect"))?;
        let session = proxy
            .create_session(Default::default())
            .await
            .map_err(portal_err("create session"))?;
        match Self::negotiate(&proxy, &session, sources, restore_token).await {
            Ok((fd, node_id, restore_token)) => Ok(Self {
                proxy,
                session,
                fd,
                node_id,
                restore_token,
            }),
            Err(e) => {
                // Ignored: the session may already be gone after a failed or cancelled request.
                let _ = session.close().await;
                Err(e)
            }
        }
    }

    async fn negotiate(
        proxy: &Screencast,
        session: &Session<Screencast>,
        sources: PortalSources,
        restore_token: Option<&str>,
    ) -> Result<(OwnedFd, u32, Option<String>), MediaError> {
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
        let node_id = streams
            .streams()
            .first()
            .ok_or_else(|| MediaError::Backend("ScreenCast portal returned no stream".into()))?
            .pipe_wire_node_id();
        let fd = proxy
            .open_pipe_wire_remote(session, OpenPipeWireRemoteOptions::default())
            .await
            .map_err(portal_err("open PipeWire remote"))?;
        Ok((fd, node_id, streams.restore_token().map(str::to_owned)))
    }

    pub(crate) async fn close(self) {
        // Ignored: the compositor may have ended the session already.
        let _ = self.session.close().await;
        drop(self.proxy);
    }
}
