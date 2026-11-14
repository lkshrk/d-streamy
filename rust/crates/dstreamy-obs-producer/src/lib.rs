//! The OBS plugin's libobs-free half: the packet ring, the producer connection and the panel's model and control logic.

pub mod client;
pub mod control;
pub mod launch;
pub mod live;
pub mod mixer;
pub mod model;
pub mod panel;
pub mod panel_config;
pub mod ring;
pub mod settings;
pub mod state_view;
pub mod status_bar;
#[cfg(test)]
mod test_support;
mod types;

#[cfg(target_os = "linux")]
pub use client::block_sigpipe_on_this_thread;
pub use client::{Connection, Reports, StartError, attach, run};
pub use dstreamy_ipc::Endpoint;
pub use ring::{Oversized, Packet, Ring, RingStats, Track};
pub use types::{DetachReason, End, Hello, Level, Log, MAX_SERVICE_TEXT, SessionEnd, StopCode};

/// How to start the service, appended to every "service is not running" message.
#[cfg(target_os = "linux")]
pub const START_SERVICE_HINT: &str =
    "start it with `dstreamy service install`, or run `dstreamy serve`";
#[cfg(not(target_os = "linux"))]
pub const START_SERVICE_HINT: &str = "start it by running `dstreamy serve`";
