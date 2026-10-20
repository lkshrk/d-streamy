#[derive(Debug, thiserror::Error)]
pub enum MediaError {
    #[error("invalid capture config: {0}")]
    InvalidConfig(String),
    #[error("unsupported: {0}")]
    Unsupported(String),
    #[error("capture source is already running")]
    AlreadyRunning,
    #[error("encoder error: {0}")]
    Encoder(String),
    #[error("capture backend error: {0}")]
    Backend(String),
}
