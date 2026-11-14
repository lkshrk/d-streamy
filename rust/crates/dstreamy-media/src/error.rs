/// A capture, encode or source failure, with the message shown to the user.
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
    /// The user dismissed the system's picker or permission dialog; nothing is wrong.
    #[error("cancelled in the screen sharing dialog")]
    Cancelled,
}
