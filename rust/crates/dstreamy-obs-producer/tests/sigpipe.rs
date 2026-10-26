//! Its own process: SIGPIPE is set back to the default (terminate), as in a C host such as OBS.
#![cfg(unix)]

use std::io::{ErrorKind, IoSlice, Write};
use std::os::unix::net::UnixStream;

use dstreamy_obs_producer::block_sigpipe_on_this_thread;

#[test]
fn a_write_to_a_closed_peer_fails_instead_of_killing_the_process() {
    // SAFETY: restores the default disposition, as a non-Rust host has it; no handler is installed.
    unsafe { libc::signal(libc::SIGPIPE, libc::SIG_DFL) };
    let err = std::thread::spawn(|| {
        block_sigpipe_on_this_thread().unwrap();
        let (mut ours, theirs) = UnixStream::pair().unwrap();
        drop(theirs);
        // std's plain `write` passes MSG_NOSIGNAL; `writev`, which tokio's framed writer uses, does not.
        ours.write_vectored(&[IoSlice::new(&[0; 64])]).unwrap_err()
    })
    .join()
    .unwrap();
    assert_eq!(err.kind(), ErrorKind::BrokenPipe);
}
