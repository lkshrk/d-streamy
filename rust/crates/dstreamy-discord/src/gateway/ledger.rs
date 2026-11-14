use std::fs::File;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};
use tracing::warn;

const LOCK_FILE: &str = ".lock";
const LOCK_WAIT: Duration = Duration::from_secs(2);
const LOCK_RETRY: Duration = Duration::from_millis(20);

/// Rolling window of IDENTIFY send times (wall clock), shared by clones and, with a file, across processes.
#[derive(Debug, Clone)]
pub struct IdentifyLedger {
    window: Duration,
    inner: Arc<Mutex<LedgerState>>,
}

#[derive(Debug, Default)]
struct LedgerState {
    file: Option<PathBuf>,
    /// The file's stamps as last read under the lock, counted when the lock cannot be taken.
    known: Vec<u64>,
    /// Every stamp without a file; with one, stamps not written to it yet.
    unsaved: Vec<u64>,
}

#[derive(Serialize, Deserialize)]
struct LedgerFile {
    /// Unix milliseconds of each IDENTIFY.
    identifies: Vec<u64>,
}

impl Default for IdentifyLedger {
    fn default() -> Self {
        Self::new(Duration::from_secs(24 * 60 * 60))
    }
}

impl IdentifyLedger {
    pub fn new(window: Duration) -> Self {
        Self {
            window,
            inner: Arc::default(),
        }
    }

    /// Keeps the stamps owner-only in `file`; writes also delete fully expired ledger files beside it.
    pub fn persisted_at(self, file: PathBuf) -> Self {
        self.state().file = Some(file);
        self
    }

    /// IDENTIFYs within the window; may block briefly on the file lock.
    pub fn recent(&self) -> usize {
        self.transact(None).stamps.len()
    }

    /// `Err` with the wait until one more IDENTIFY fits under `limit`.
    pub(crate) async fn allows(&self, limit: u32) -> Result<(), Duration> {
        let outcome = self.transact_blocking(None).await;
        outcome.check(limit, self.window)
    }

    /// Records an IDENTIFY if one more fits under `limit`, as one locked step; else the wait as in `allows`.
    pub(crate) async fn record_within(&self, limit: u32) -> Result<(), Duration> {
        let outcome = self.transact_blocking(Some(limit)).await;
        if outcome.recorded {
            Ok(())
        } else {
            outcome.check(limit, self.window).and(Err(self.window))
        }
    }

    async fn transact_blocking(&self, record_below: Option<u32>) -> Outcome {
        if self.state().file.is_none() {
            return self.transact(record_below);
        }
        let ledger = self.clone();
        match tokio::task::spawn_blocking(move || ledger.transact(record_below)).await {
            Ok(outcome) => outcome,
            Err(e) => {
                warn!("counting IDENTIFYs failed: {e}");
                Outcome {
                    stamps: Vec::new(),
                    recorded: false,
                    now: now_ms(),
                }
            }
        }
    }

    fn transact(&self, record_below: Option<u32>) -> Outcome {
        let mut state = self.state();
        let now = now_ms();
        let window = window_ms(self.window);
        let wants_record = |stamps: &[u64]| record_below.is_some_and(|l| stamps.len() < l as usize);
        let Some(file) = state.file.clone() else {
            let (mut stamps, _) = live(state.unsaved.clone(), now, window);
            let recorded = wants_record(&stamps);
            if recorded {
                stamps.push(now);
            }
            state.unsaved.clone_from(&stamps);
            return Outcome {
                stamps,
                recorded,
                now,
            };
        };
        let dir = file.parent().map(Path::to_path_buf).unwrap_or_default();
        let Some(_lock) = DirLock::acquire(&dir) else {
            let mut merged = state.known.clone();
            merged.extend_from_slice(&state.unsaved);
            let (mut stamps, _) = live(merged, now, window);
            let recorded = wants_record(&stamps);
            if recorded {
                stamps.push(now);
                state.unsaved.push(now);
            }
            return Outcome {
                stamps,
                recorded,
                now,
            };
        };
        let mut merged = read_ledger(&file);
        let pending = state.unsaved.len();
        merged.extend_from_slice(&state.unsaved);
        let (mut stamps, clamped) = live(merged, now, window);
        let recorded = wants_record(&stamps);
        if recorded {
            stamps.push(now);
        }
        if recorded || clamped || pending > 0 {
            match write_ledger(&file, &stamps) {
                Ok(()) => {
                    state.unsaved.clear();
                    prune_expired(&dir, &file, now, window);
                }
                Err(e) => {
                    warn!(path = %file.display(), "saving the IDENTIFY count failed: {e}");
                    if recorded {
                        state.unsaved.push(now);
                    }
                }
            }
        }
        state.known.clone_from(&stamps);
        Outcome {
            stamps,
            recorded,
            now,
        }
    }

    fn state(&self) -> MutexGuard<'_, LedgerState> {
        self.inner.lock().unwrap_or_else(PoisonError::into_inner)
    }
}

struct Outcome {
    /// Within the window, oldest first.
    stamps: Vec<u64>,
    recorded: bool,
    now: u64,
}

impl Outcome {
    fn check(&self, limit: u32, window: Duration) -> Result<(), Duration> {
        let Some(i) = self.stamps.len().checked_sub(limit.max(1) as usize) else {
            return Ok(());
        };
        let expires = self.stamps[i].saturating_add(window_ms(window));
        Err(Duration::from_millis(expires.saturating_sub(self.now)))
    }
}

/// Unlocked explicitly: a forked child holding the descriptor would otherwise keep the lock.
struct DirLock(File);

impl DirLock {
    /// `None` if the lock cannot be had within `LOCK_WAIT`; the caller then counts in memory only.
    fn acquire(dir: &Path) -> Option<Self> {
        let path = dir.join(LOCK_FILE);
        let file = create_private_dir(dir).and_then(|()| {
            File::options()
                .create(true)
                .truncate(false)
                .write(true)
                .open(&path)
        });
        let file = match file {
            Ok(file) => file,
            Err(e) => {
                warn!(path = %path.display(), "opening the IDENTIFY count lock failed: {e}");
                return None;
            }
        };
        let deadline = Instant::now() + LOCK_WAIT;
        loop {
            match file.try_lock() {
                Ok(()) => return Some(Self(file)),
                Err(std::fs::TryLockError::WouldBlock) if Instant::now() < deadline => {
                    std::thread::sleep(LOCK_RETRY);
                }
                Err(e) => {
                    warn!(path = %path.display(), "locking the IDENTIFY count failed: {e}");
                    return None;
                }
            }
        }
    }
}

impl Drop for DirLock {
    fn drop(&mut self) {
        let _ = self.0.unlock();
    }
}

fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| u64::try_from(d.as_millis()).unwrap_or(u64::MAX))
}

fn window_ms(window: Duration) -> u64 {
    u64::try_from(window.as_millis()).unwrap_or(u64::MAX)
}

/// Stamps within the window, oldest first, and whether a future stamp was moved to `now`.
fn live(stamps: Vec<u64>, now: u64, window: u64) -> (Vec<u64>, bool) {
    let mut clamped = false;
    let mut stamps: Vec<u64> = stamps
        .into_iter()
        .map(|at| {
            clamped |= at > now;
            at.min(now)
        })
        .filter(|at| now - at < window)
        .collect();
    stamps.sort_unstable();
    (stamps, clamped)
}

/// A missing file is empty; an unreadable one is reported and counted as empty, then rewritten.
fn read_ledger(file: &Path) -> Vec<u64> {
    let text = match std::fs::read(file) {
        Ok(text) => text,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Vec::new(),
        Err(e) => {
            warn!(path = %file.display(), "reading the IDENTIFY count failed: {e}");
            return Vec::new();
        }
    };
    match serde_json::from_slice::<LedgerFile>(&text) {
        Ok(ledger) => ledger.identifies,
        Err(e) => {
            warn!(path = %file.display(), "the IDENTIFY count is unreadable and starts over: {e}");
            Vec::new()
        }
    }
}

fn write_ledger(file: &Path, stamps: &[u64]) -> std::io::Result<()> {
    let dir = file.parent().unwrap_or(Path::new("."));
    create_private_dir(dir)?;
    let mut name = file.file_name().unwrap_or_default().to_owned();
    name.push(format!(
        ".{}.{:x}.tmp",
        std::process::id(),
        rand::random::<u64>()
    ));
    let tmp = file.with_file_name(name);
    let mut options = File::options();
    options.create_new(true).write(true);
    #[cfg(unix)]
    std::os::unix::fs::OpenOptionsExt::mode(&mut options, 0o600);
    let written = (|| {
        let mut out = options.open(&tmp)?;
        let body = serde_json::to_vec(&LedgerFile {
            identifies: stamps.to_vec(),
        })?;
        out.write_all(&body)?;
        out.sync_all()?;
        drop(out);
        std::fs::rename(&tmp, file)
    })();
    if written.is_err() {
        let _ = std::fs::remove_file(&tmp);
    }
    written
}

/// Removes the other ledger files of `dir` with no stamp left in the window; needs the dir lock.
fn prune_expired(dir: &Path, keep: &Path, now: u64, window: u64) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    for path in entries.filter_map(|e| e.ok().map(|e| e.path())) {
        if path == keep || path.extension().is_none_or(|ext| ext != "json") {
            continue;
        }
        if live(read_ledger(&path), now, window).0.is_empty() {
            let _ = std::fs::remove_file(&path);
        }
    }
}

fn create_private_dir(dir: &Path) -> std::io::Result<()> {
    if dir.as_os_str().is_empty() {
        return Ok(());
    }
    let mut builder = std::fs::DirBuilder::new();
    builder.recursive(true);
    #[cfg(unix)]
    std::os::unix::fs::DirBuilderExt::mode(&mut builder, 0o700);
    builder.create(dir)
}

#[cfg(test)]
mod tests {
    use super::*;

    const HOUR: u64 = 60 * 60 * 1000;
    const DAY: Duration = Duration::from_secs(24 * 60 * 60);

    fn ledger_file(dir: &tempfile::TempDir) -> PathBuf {
        dir.path().join("identify").join("digest.json")
    }

    fn persisted(dir: &tempfile::TempDir) -> IdentifyLedger {
        IdentifyLedger::new(DAY).persisted_at(ledger_file(dir))
    }

    #[tokio::test]
    async fn a_persisted_ledger_is_shared_by_every_ledger_on_the_file() {
        let dir = tempfile::tempdir().unwrap();
        let (first, second) = (persisted(&dir), persisted(&dir));
        first.record_within(100).await.unwrap();
        second.record_within(100).await.unwrap();
        first.record_within(100).await.unwrap();
        assert_eq!(first.recent(), 3);
        assert_eq!(second.recent(), 3);
        assert_eq!(persisted(&dir).recent(), 3);
    }

    #[tokio::test]
    async fn recording_stops_at_the_limit_and_says_when_it_frees() {
        let dir = tempfile::tempdir().unwrap();
        let now = now_ms();
        write_ledger(
            &ledger_file(&dir),
            &[now - 25 * HOUR, now - 23 * HOUR, now - HOUR],
        )
        .unwrap();
        let ledger = persisted(&dir);
        assert_eq!(ledger.recent(), 2);
        assert_eq!(ledger.allows(3).await, Ok(()));
        let wait = ledger.allows(2).await.unwrap_err();
        assert!(
            wait > Duration::from_secs(55 * 60) && wait <= Duration::from_secs(60 * 60),
            "{wait:?}"
        );
        assert!(ledger.record_within(2).await.is_err());
        assert_eq!(ledger.recent(), 2);
        assert_eq!(ledger.record_within(3).await, Ok(()));
        assert!(ledger.record_within(3).await.is_err());
        assert_eq!(read_ledger(&ledger_file(&dir)).len(), 3);
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn the_ledger_file_is_owner_only() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().unwrap();
        persisted(&dir).record_within(1).await.unwrap();
        let file = ledger_file(&dir);
        let mode = |p: &Path| std::fs::metadata(p).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode(&file), 0o600);
        assert_eq!(mode(file.parent().unwrap()), 0o700);
    }

    #[tokio::test]
    async fn a_corrupt_or_missing_file_counts_as_empty_and_is_rewritten() {
        let dir = tempfile::tempdir().unwrap();
        let file = ledger_file(&dir);
        let ledger = persisted(&dir);
        assert_eq!(ledger.recent(), 0);
        std::fs::create_dir_all(file.parent().unwrap()).unwrap();
        std::fs::write(&file, b"{not json").unwrap();
        assert_eq!(ledger.recent(), 0);
        ledger.record_within(1).await.unwrap();
        assert_eq!(read_ledger(&file).len(), 1);
    }

    #[test]
    fn a_future_stamp_is_written_back_as_now_so_it_expires_one_window_later() {
        let dir = tempfile::tempdir().unwrap();
        let file = ledger_file(&dir);
        let year = 365 * 24 * HOUR;
        let before = now_ms();
        write_ledger(&file, &[before + year]).unwrap();
        let ledger = IdentifyLedger::new(Duration::from_secs(60)).persisted_at(file.clone());
        assert_eq!(ledger.recent(), 1);
        let stored = read_ledger(&file);
        assert_eq!(stored.len(), 1);
        assert!(stored[0] >= before && stored[0] <= now_ms(), "{stored:?}");
        write_ledger(&file, &[stored[0] - 61_000]).unwrap();
        assert_eq!(ledger.recent(), 0);
    }

    #[test]
    fn expired_ledger_files_of_other_tokens_are_removed_on_write() {
        let dir = tempfile::tempdir().unwrap();
        let file = ledger_file(&dir);
        let other = file.with_file_name("other.json");
        let busy = file.with_file_name("busy.json");
        let now = now_ms();
        write_ledger(&other, &[now - 25 * HOUR]).unwrap();
        write_ledger(&busy, &[now - HOUR]).unwrap();
        persisted(&dir).transact(Some(1));
        assert!(!other.exists());
        assert!(busy.exists());
        assert!(file.exists());
    }

    #[test]
    fn a_held_lock_falls_back_to_counting_in_memory_without_writing() {
        let dir = tempfile::tempdir().unwrap();
        let file = ledger_file(&dir);
        let ledger = persisted(&dir);
        assert!(ledger.transact(Some(5)).recorded);
        let held = DirLock::acquire(file.parent().unwrap()).unwrap();
        let started = Instant::now();
        let outcome = ledger.transact(Some(5));
        assert!(started.elapsed() < LOCK_WAIT + Duration::from_secs(1));
        assert!(outcome.recorded);
        assert_eq!(outcome.stamps.len(), 2);
        assert_eq!(read_ledger(&file).len(), 1);
        drop(held);
        assert_eq!(ledger.recent(), 2);
        assert_eq!(read_ledger(&file).len(), 2);
    }

    #[test]
    fn concurrent_writers_never_pass_the_limit_or_lose_stamps() {
        let dir = tempfile::tempdir().unwrap();
        let file = ledger_file(&dir);
        let threads: Vec<_> = (0..8)
            .map(|_| {
                let ledger = IdentifyLedger::new(DAY).persisted_at(file.clone());
                std::thread::spawn(move || {
                    (0..10)
                        .filter(|_| ledger.transact(Some(50)).recorded)
                        .count()
                })
            })
            .collect();
        let recorded: usize = threads.into_iter().map(|t| t.join().unwrap()).sum();
        assert_eq!(recorded, 50);
        assert_eq!(read_ledger(&file).len(), 50);
    }

    #[test]
    fn a_memory_ledger_counts_within_its_window() {
        let ledger = IdentifyLedger::new(Duration::from_millis(100));
        assert!(ledger.transact(Some(1)).recorded);
        assert!(!ledger.transact(Some(1)).recorded);
        std::thread::sleep(Duration::from_millis(150));
        assert!(ledger.transact(Some(1)).recorded);
    }
}
