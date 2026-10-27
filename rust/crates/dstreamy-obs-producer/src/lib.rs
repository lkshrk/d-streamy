//! The OBS plugin's libobs-free half, testable without libobs: the packet ring, the producer connection, and the dock's control logic.

#[cfg(unix)]
pub mod client;
pub mod control;
pub mod dock;
pub mod ring;
pub mod settings;
mod types;

#[cfg(target_os = "linux")]
pub use client::block_sigpipe_on_this_thread;
#[cfg(unix)]
pub use client::{Connection, StartError, attach, run};
#[cfg(unix)]
pub use dstreamy_ipc::Endpoint;
pub use ring::{Oversized, Packet, Ring, RingStats, Track};
pub use types::{End, Hello, Level, Log, MAX_SERVICE_TEXT};
