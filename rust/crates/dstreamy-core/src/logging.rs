use std::fs::{self, File, OpenOptions};
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, PoisonError};

pub const DEFAULT_MAX_LOG_BYTES: u64 = 10 * 1024 * 1024;
pub const DEFAULT_KEPT_LOGS: usize = 1;

/// Log file capped at `max_bytes` that rotates to `<path>.1..=keep`; write errors are swallowed.
#[derive(Clone)]
pub struct RotatingFileWriter {
    path: Arc<PathBuf>,
    inner: Arc<Mutex<Inner>>,
}

struct Inner {
    max_bytes: u64,
    keep: usize,
    file: Option<File>,
    bytes: u64,
}

impl RotatingFileWriter {
    pub fn new(path: impl Into<PathBuf>, max_bytes: u64, keep: usize) -> Self {
        Self {
            path: Arc::new(path.into()),
            inner: Arc::new(Mutex::new(Inner {
                max_bytes: max_bytes.max(1),
                keep,
                file: None,
                bytes: 0,
            })),
        }
    }

    pub fn with_defaults(path: impl Into<PathBuf>) -> Self {
        Self::new(path, DEFAULT_MAX_LOG_BYTES, DEFAULT_KEPT_LOGS)
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    pub fn rotated_path(&self, n: usize) -> PathBuf {
        rotated(&self.path, n)
    }

    fn append(&self, buf: &[u8]) {
        let mut inner = self.inner.lock().unwrap_or_else(PoisonError::into_inner);
        if inner.file.is_none() && inner.open(&self.path).is_err() {
            return;
        }
        if inner.bytes > 0 && inner.bytes + buf.len() as u64 > inner.max_bytes {
            inner.rotate(&self.path);
            if inner.file.is_none() && inner.open(&self.path).is_err() {
                return;
            }
        }
        let written = inner.file.as_mut().map(|f| f.write_all(buf));
        match written {
            Some(Ok(())) => inner.bytes += buf.len() as u64,
            // Reopened on the next write, so a deleted or replaced file recovers.
            _ => inner.file = None,
        }
    }
}

impl Inner {
    fn open(&mut self, path: &Path) -> io::Result<()> {
        if let Some(dir) = path.parent() {
            fs::create_dir_all(dir)?;
        }
        let file = OpenOptions::new().create(true).append(true).open(path)?;
        self.bytes = file.metadata()?.len();
        self.file = Some(file);
        Ok(())
    }

    fn rotate(&mut self, path: &Path) {
        self.file = None;
        self.bytes = 0;
        if self.keep == 0 {
            let _ = File::create(path);
            return;
        }
        for n in (1..self.keep).rev() {
            let _ = fs::rename(rotated(path, n), rotated(path, n + 1));
        }
        let _ = fs::rename(path, rotated(path, 1));
    }
}

fn rotated(path: &Path, n: usize) -> PathBuf {
    let mut name = path.as_os_str().to_owned();
    name.push(format!(".{n}"));
    PathBuf::from(name)
}

impl Write for RotatingFileWriter {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        self.append(buf);
        Ok(buf.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

/// Drops every write after the output first fails (EPIPE), so logging can never loop on it.
pub struct QuietOnError<W> {
    inner: Arc<Mutex<W>>,
    broken: Arc<AtomicBool>,
}

impl<W> Clone for QuietOnError<W> {
    fn clone(&self) -> Self {
        Self {
            inner: self.inner.clone(),
            broken: self.broken.clone(),
        }
    }
}

impl<W: Write> QuietOnError<W> {
    pub fn new(inner: W) -> Self {
        Self {
            inner: Arc::new(Mutex::new(inner)),
            broken: Arc::new(AtomicBool::new(false)),
        }
    }

    pub fn is_broken(&self) -> bool {
        self.broken.load(Ordering::Relaxed)
    }
}

impl<W: Write> Write for QuietOnError<W> {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        if self.is_broken() {
            return Ok(buf.len());
        }
        let mut inner = self.inner.lock().unwrap_or_else(PoisonError::into_inner);
        match inner.write_all(buf).and_then(|()| inner.flush()) {
            Err(e) if e.kind() != io::ErrorKind::WouldBlock => {
                self.broken.store(true, Ordering::Relaxed);
            }
            _ => {}
        }
        Ok(buf.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn line(n: usize) -> Vec<u8> {
        format!("{n:09}\n").into_bytes()
    }

    #[test]
    fn appends_and_reopens_an_existing_file_at_its_size() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("logs/stream.log");
        let mut w = RotatingFileWriter::new(&path, 1000, 1);
        w.write_all(&line(1)).unwrap();
        drop(w);
        let mut w = RotatingFileWriter::new(&path, 25, 1);
        w.write_all(&line(2)).unwrap();
        assert_eq!(fs::read(&path).unwrap(), [line(1), line(2)].concat());
        w.write_all(&line(3)).unwrap();
        assert_eq!(fs::read(&path).unwrap(), line(3));
        assert_eq!(
            fs::read(w.rotated_path(1)).unwrap(),
            [line(1), line(2)].concat()
        );
    }

    #[test]
    fn rotation_keeps_at_most_keep_old_files_and_caps_each() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("stream.log");
        let mut w = RotatingFileWriter::new(&path, 30, 2);
        for n in 0..20 {
            w.write_all(&line(n)).unwrap();
        }
        for p in [path.clone(), w.rotated_path(1), w.rotated_path(2)] {
            assert!(fs::metadata(&p).unwrap().len() <= 30, "{p:?}");
        }
        assert!(!w.rotated_path(3).exists());
        assert_eq!(fs::read(&path).unwrap(), [line(18), line(19)].concat());
        assert_eq!(
            fs::read(w.rotated_path(2)).unwrap(),
            [line(12), line(13), line(14)].concat()
        );
    }

    #[test]
    fn keep_zero_truncates_in_place() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("stream.log");
        let mut w = RotatingFileWriter::new(&path, 15, 0);
        for n in 0..5 {
            w.write_all(&line(n)).unwrap();
        }
        assert_eq!(fs::read(&path).unwrap(), line(4));
        assert!(!w.rotated_path(1).exists());
    }

    #[test]
    fn an_unwritable_path_is_swallowed() {
        let dir = tempfile::tempdir().unwrap();
        let blocker = dir.path().join("file");
        fs::write(&blocker, b"x").unwrap();
        let mut w = RotatingFileWriter::new(blocker.join("stream.log"), 100, 1);
        for n in 0..100 {
            assert_eq!(w.write(&line(n)).unwrap(), 10);
        }
    }

    struct BrokenPipe {
        writes: Arc<std::sync::atomic::AtomicUsize>,
    }

    impl Write for BrokenPipe {
        fn write(&mut self, _: &[u8]) -> io::Result<usize> {
            self.writes.fetch_add(1, Ordering::SeqCst);
            Err(io::ErrorKind::BrokenPipe.into())
        }

        fn flush(&mut self) -> io::Result<()> {
            Ok(())
        }
    }

    struct Busy {
        calls: usize,
        out: Vec<u8>,
    }

    impl Write for Busy {
        fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
            self.calls += 1;
            if self.calls == 1 {
                return Err(io::ErrorKind::WouldBlock.into());
            }
            self.out.extend_from_slice(buf);
            Ok(buf.len())
        }

        fn flush(&mut self) -> io::Result<()> {
            Ok(())
        }
    }

    #[test]
    fn would_block_drops_one_write_without_going_quiet() {
        let mut w = QuietOnError::new(Busy {
            calls: 0,
            out: Vec::new(),
        });
        w.write_all(b"lost\n").unwrap();
        w.write_all(b"kept\n").unwrap();
        assert!(!w.is_broken());
        assert_eq!(w.inner.lock().unwrap().out, b"kept\n");
    }

    #[test]
    fn a_broken_output_is_tried_once_then_dropped() {
        let writes = Arc::default();
        let mut w = QuietOnError::new(BrokenPipe {
            writes: Arc::clone(&writes),
        });
        let mut clone = w.clone();
        for _ in 0..1000 {
            assert_eq!(w.write(b"line\n").unwrap(), 5);
            assert_eq!(clone.write(b"line\n").unwrap(), 5);
        }
        assert!(w.is_broken());
        assert_eq!(writes.load(Ordering::SeqCst), 1);
    }

    #[test]
    fn tracing_writes_through_the_rotating_file() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("stream.log");
        let file = RotatingFileWriter::new(&path, 400, 1);
        let stderr_writes = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let stderr = QuietOnError::new(BrokenPipe {
            writes: Arc::clone(&stderr_writes),
        });
        let subscriber = tracing_subscriber::fmt()
            .with_ansi(false)
            .with_writer({
                let file = file.clone();
                move || file.clone()
            })
            .finish();
        tracing::subscriber::with_default(subscriber, || {
            for n in 0..50 {
                tracing::info!(n, "stream metrics");
            }
        });
        let stderr_subscriber = tracing_subscriber::fmt()
            .with_writer(move || stderr.clone())
            .finish();
        tracing::subscriber::with_default(stderr_subscriber, || {
            for n in 0..50 {
                tracing::error!(n, "broken stderr");
            }
        });
        assert_eq!(stderr_writes.load(Ordering::SeqCst), 1);
        let current = fs::read_to_string(&path).unwrap();
        assert!(current.contains("n=49"), "{current}");
        assert!(fs::metadata(&path).unwrap().len() <= 400);
        assert!(fs::metadata(file.rotated_path(1)).unwrap().len() <= 400);
    }
}
