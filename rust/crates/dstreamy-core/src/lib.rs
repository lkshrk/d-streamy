//! Domain types, configuration, health scoring, stream metrics and logging for D-Streamy.

use std::sync::{Mutex, MutexGuard, PoisonError};

pub mod accounts;
pub mod codec;
pub mod config;
pub mod crop;
pub mod health;
pub mod logging;
pub mod metrics;
pub mod stream_config;
pub mod target;

pub use accounts::{AccountInfo, AccountUser, Accounts, ResolvedToken, TokenSource};
pub use codec::{Codec, UnknownCodec};
pub use config::{Config, ConfigError, ConfigPaths, IdleSetting, ServeConfig, TOKEN_ENV};
pub use crop::CropRect;
pub use health::{HealthLevel, HealthSample, StreamHealth};
pub use logging::{QuietOnError, RotatingFileWriter};
pub use metrics::{
    FrameIntervalSnapshot, FrameIntervalTracker, MetricsSampler, PipelineStats, StreamMetrics,
};
pub use stream_config::{Fps, InvalidStreamConfig, Resolution, StreamConfig};
pub use target::CaptureTarget;

/// Locks `m`, keeping the data of a thread that panicked while holding it.
pub fn lock<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    m.lock().unwrap_or_else(PoisonError::into_inner)
}
