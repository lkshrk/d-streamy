use std::path::PathBuf;

/// A fresh directory next to the test binary, so tests never write to the shared /tmp.
pub fn fresh(name: &str) -> PathBuf {
    let exe = std::env::current_exe().expect("test binary path");
    let dir = exe
        .parent()
        .and_then(|deps| deps.parent())
        .expect("target directory")
        .join("dstreamy-obs-tests")
        .join(format!("{name}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).expect("create test directory");
    dir
}
