//! The OBS plugin's libobs-free half, testable without libobs: the packet ring and the producer connection.

#[cfg(unix)]
pub mod client;
pub mod ring;
mod types;

#[cfg(unix)]
pub use client::{Connection, StartError, attach, block_sigpipe_on_this_thread, run};
#[cfg(unix)]
pub use dstreamy_ipc::Endpoint;
pub use ring::{Packet, Ring, RingStats, Track};
pub use types::{End, Hello, Level, Log};
