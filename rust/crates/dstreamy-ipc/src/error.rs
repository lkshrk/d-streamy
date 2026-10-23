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
    #[error("the service at {path} runs as uid {uid}, not as this user")]
    ImpostorServer { path: PathBuf, uid: u32 },
    #[error("{0} exists and is not a socket")]
    NotASocket(PathBuf),
    #[error("endpoint path {0} is too long for a local socket")]
    PathTooLong(PathBuf),
    #[error("no runtime directory and no home directory to place the endpoint in")]
    NoHomeDirectory,
    #[error(transparent)]
    Io(#[from] io::Error),
}
