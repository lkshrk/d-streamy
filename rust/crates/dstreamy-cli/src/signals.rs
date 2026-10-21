#[cfg(unix)]
use tokio::signal::unix::{Signal, SignalKind, signal};
use tokio::sync::mpsc;

/// SIGINT and SIGTERM, registered once so that a press during shutdown is never lost.
pub struct Signals {
    source: Source,
}

enum Source {
    #[cfg(unix)]
    Unix(Signal, Signal),
    CtrlC,
    Channel(mpsc::UnboundedReceiver<()>),
}

impl Signals {
    pub fn new() -> Self {
        #[cfg(unix)]
        if let Ok(int) = signal(SignalKind::interrupt())
            && let Ok(term) = signal(SignalKind::terminate())
        {
            return Self {
                source: Source::Unix(int, term),
            };
        }
        Self {
            source: Source::CtrlC,
        }
    }

    /// Each message on `rx` is one interrupt, for driving a command in-process.
    pub fn from_channel(rx: mpsc::UnboundedReceiver<()>) -> Self {
        Self {
            source: Source::Channel(rx),
        }
    }

    pub async fn recv(&mut self) {
        match &mut self.source {
            #[cfg(unix)]
            Source::Unix(int, term) => {
                tokio::select! {
                    _ = int.recv() => {}
                    _ = term.recv() => {}
                }
            }
            Source::CtrlC => {
                let _ = tokio::signal::ctrl_c().await;
            }
            Source::Channel(rx) => {
                if rx.recv().await.is_none() {
                    std::future::pending::<()>().await;
                }
            }
        }
    }
}

impl Default for Signals {
    fn default() -> Self {
        Self::new()
    }
}
