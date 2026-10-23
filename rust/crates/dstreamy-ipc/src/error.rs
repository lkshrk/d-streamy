use std::io;
use std::path::PathBuf;

#[derive(Debug, thiserror::Error)]
pub enum IpcError {
    #[error("already running at {0}")]
    AlreadyRunning(PathBuf),
    #[error("no service is listening at {0}")]
    NotRunning(PathBuf),
    #[error("refusing to use {path}: {reason}")]
    InsecureDirectory { path: PathBuf, reason: String },
    #[error("{0} exists and is not a socket")]
    NotASocket(PathBuf),
    #[error("endpoint path {0} is too long for a local socket")]
    PathTooLong(PathBuf),
    #[error(transparent)]
    Io(#[from] io::Error),
}
