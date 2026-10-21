//! Domain types, configuration, health scoring, stream metrics and logging for D-Streamy.

pub mod codec;
pub mod config;
pub mod crop;
pub mod health;
pub mod logging;
pub mod metrics;
pub mod stream_config;
pub mod target;

pub use codec::{Codec, UnknownCodec};
pub use config::{Config, ConfigError, ConfigPaths, TOKEN_ENV};
pub use crop::CropRect;
pub use health::{HealthLevel, HealthSample, StreamHealth};
pub use logging::{QuietOnError, RotatingFileWriter};
pub use metrics::{
    FrameIntervalSnapshot, FrameIntervalTracker, MetricsSampler, PipelineStats, StreamMetrics,
};
pub use stream_config::{Fps, InvalidStreamConfig, Resolution, StreamConfig};
pub use target::CaptureTarget;
