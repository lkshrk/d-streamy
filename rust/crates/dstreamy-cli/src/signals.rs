#[cfg(unix)]
use tokio::signal::unix::{Signal, SignalKind, signal};

/// SIGINT and SIGTERM, registered once so that a press during shutdown is never lost.
pub struct Signals {
    #[cfg(unix)]
    streams: Option<(Signal, Signal)>,
}

impl Signals {
    pub fn new() -> Self {
        #[cfg(unix)]
        {
            let streams = signal(SignalKind::interrupt())
                .and_then(|int| Ok((int, signal(SignalKind::terminate())?)))
                .ok();
            Self { streams }
        }
        #[cfg(not(unix))]
        Self {}
    }

    pub async fn recv(&mut self) {
        #[cfg(unix)]
        if let Some((int, term)) = &mut self.streams {
            tokio::select! {
                _ = int.recv() => {}
                _ = term.recv() => {}
            }
            return;
        }
        let _ = tokio::signal::ctrl_c().await;
    }
}

impl Default for Signals {
    fn default() -> Self {
        Self::new()
    }
}
