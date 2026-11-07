use std::fmt::Display;
use std::io::{self, Write};
use std::sync::mpsc::{self, SyncSender, TrySendError};
use std::time::{Duration, Instant};

use dstreamy_core::QuietOnError;
use serde::Serialize;

const STDERR_RECORDS: usize = 64;
const STDERR_RECORD_BYTES: usize = 8 * 1024;
const STDERR_DRAIN: Duration = Duration::from_millis(100);

enum Diagnostic {
    Bytes(Vec<u8>),
    Drain(mpsc::Sender<()>),
}

#[derive(Clone)]
struct QueuedStderr(SyncSender<Diagnostic>);

impl QueuedStderr {
    fn new(writer: impl Write + Send + 'static) -> Self {
        let (sender, records) = mpsc::sync_channel(STDERR_RECORDS);
        let _ = std::thread::Builder::new()
            .name("daemon-stderr".into())
            .spawn(move || {
                let mut writer = QuietOnError::new(writer);
                for record in records {
                    match record {
                        Diagnostic::Bytes(bytes) => {
                            let _ = writer.write_all(&bytes);
                            if writer.is_broken() {
                                break;
                            }
                        }
                        Diagnostic::Drain(done) => {
                            let _ = done.send(());
                        }
                    }
                }
            });
        Self(sender)
    }

    fn drain(&self, limit: Duration) {
        let (done, drained) = mpsc::channel();
        let mut record = Diagnostic::Drain(done);
        let deadline = Instant::now() + limit;
        loop {
            match self.0.try_send(record) {
                Ok(()) => break,
                Err(TrySendError::Disconnected(_)) => return,
                Err(TrySendError::Full(pending)) => {
                    record = pending;
                    if Instant::now() >= deadline {
                        return;
                    }
                    std::thread::sleep(Duration::from_millis(1));
                }
            }
        }
        let _ = drained.recv_timeout(deadline.saturating_duration_since(Instant::now()));
    }
}

impl Write for QueuedStderr {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        const TRUNCATED: &[u8] = b"... [truncated]\n";
        let bytes = if buf.len() > STDERR_RECORD_BYTES {
            let mut end = STDERR_RECORD_BYTES - TRUNCATED.len();
            while end > 0 && buf[end] & 0xc0 == 0x80 {
                end -= 1;
            }
            let mut bytes = buf[..end].to_vec();
            bytes.extend_from_slice(TRUNCATED);
            bytes
        } else {
            buf.to_vec()
        };
        let _ = self.0.try_send(Diagnostic::Bytes(bytes));
        Ok(buf.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

#[derive(Clone)]
enum Stderr {
    Direct(QuietOnError<io::Stderr>),
    Queued(QueuedStderr),
}

impl Write for Stderr {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        match self {
            Self::Direct(writer) => writer.write(buf),
            Self::Queued(writer) => writer.write(buf),
        }
    }
    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

/// Stdout and stderr that never fail; once stdout is gone the command should wind down.
#[derive(Clone)]
pub struct Out {
    stdout: QuietOnError<io::Stdout>,
    stderr: Stderr,
    pub json: bool,
}

impl Out {
    pub fn new(json: bool) -> Self {
        Self {
            stdout: QuietOnError::new(io::stdout()),
            stderr: Stderr::Direct(QuietOnError::new(io::stderr())),
            json,
        }
    }

    /// Daemon diagnostics enqueue bounded records so a blocked parent cannot hold up shutdown.
    pub fn for_daemon(json: bool) -> Self {
        Self {
            stdout: QuietOnError::new(io::stdout()),
            stderr: Stderr::Queued(QueuedStderr::new(io::stderr())),
            json,
        }
    }

    /// Best-effort bounded drain after the command and runtime have ended; never called by a writer.
    pub fn drain_stderr(&self) {
        if let Stderr::Queued(writer) = &self.stderr {
            writer.drain(STDERR_DRAIN);
        }
    }

    pub fn stderr_writer(&self) -> impl Write + Clone + Send + Sync + 'static {
        self.stderr.clone()
    }

    pub fn line(&self, text: impl Display) {
        let _ = self
            .stdout
            .clone()
            .write_all(format!("{text}\n").as_bytes());
    }

    pub fn raw(&self, text: &str) {
        let _ = self.stdout.clone().write_all(text.as_bytes());
    }

    pub fn json_line(&self, value: &impl Serialize) {
        match serde_json::to_string(value) {
            Ok(text) => self.line(text),
            Err(e) => self.error(format!("serializing output: {e}")),
        }
    }

    pub fn err_raw(&self, text: &str) {
        let _ = self.stderr.clone().write_all(text.as_bytes());
    }

    pub fn error(&self, text: impl Display) {
        self.err_raw(&format!("error: {text}\n"));
    }

    pub fn warn(&self, text: impl Display) {
        self.err_raw(&format!("warning: {text}\n"));
    }

    pub fn note(&self, text: impl Display) {
        self.err_raw(&format!("{text}\n"));
    }

    pub fn stdout_broken(&self) -> bool {
        self.stdout.is_broken()
    }
}

/// Left-aligned columns separated by two spaces; the last column is not padded.
pub fn table(rows: &[Vec<String>]) -> String {
    let columns = rows.iter().map(Vec::len).max().unwrap_or(0);
    let widths: Vec<usize> = (0..columns)
        .map(|c| {
            rows.iter()
                .filter_map(|r| r.get(c))
                .map(|s| s.chars().count())
                .max()
                .unwrap_or(0)
        })
        .collect();
    let mut out = String::new();
    for row in rows {
        let last = row.len().saturating_sub(1);
        for (c, cell) in row.iter().enumerate() {
            if c == last {
                out.push_str(cell);
            } else {
                out.push_str(cell);
                let pad = widths[c] - cell.chars().count() + 2;
                out.extend(std::iter::repeat_n(' ', pad));
            }
        }
        out.push('\n');
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn queued_stderr_bounds_record_count_and_bytes_without_waiting_for_a_reader() {
        let (sender, records) = mpsc::sync_channel(STDERR_RECORDS);
        let mut writer = QueuedStderr(sender);
        let text = "€".repeat(STDERR_RECORD_BYTES);
        for _ in 0..1_000 {
            assert_eq!(writer.write(text.as_bytes()).unwrap(), text.len());
        }
        writer.flush().unwrap();
        let records: Vec<_> = records.try_iter().collect();
        assert_eq!(records.len(), STDERR_RECORDS);
        for record in records {
            let Diagnostic::Bytes(bytes) = record else {
                panic!("unexpected drain")
            };
            assert!(bytes.len() <= STDERR_RECORD_BYTES);
            assert!(
                std::str::from_utf8(&bytes)
                    .unwrap()
                    .ends_with("[truncated]\n")
            );
        }
    }

    #[cfg(unix)]
    #[test]
    fn queued_stderr_drains_writable_diagnostics_and_goes_quiet_after_epipe() {
        use std::io::Read;
        let (mut reader, sink) = std::os::unix::net::UnixStream::pair().unwrap();
        reader
            .set_read_timeout(Some(Duration::from_secs(2)))
            .unwrap();
        let mut writer = QueuedStderr::new(sink);
        writer.write_all(b"actionable diagnostic\n").unwrap();
        writer.drain(Duration::from_secs(2));
        let mut text = [0; 22];
        reader.read_exact(&mut text).unwrap();
        assert_eq!(&text, b"actionable diagnostic\n");
        drop(reader);
        writer.write_all(b"broken\n").unwrap();
        writer.drain(Duration::from_secs(2));
        assert!(matches!(
            writer.0.try_send(Diagnostic::Bytes(Vec::new())),
            Err(TrySendError::Disconnected(_))
        ));
        writer.write_all(b"quiet\n").unwrap();
    }

    #[test]
    fn table_pads_all_but_the_last_column() {
        let rows = vec![
            vec!["1".to_owned(), "General".to_owned(), "voice".to_owned()],
            vec!["22".to_owned(), "Gä".to_owned(), "stage".to_owned()],
        ];
        assert_eq!(table(&rows), "1   General  voice\n22  Gä       stage\n");
        assert_eq!(table(&[]), "");
    }
}
