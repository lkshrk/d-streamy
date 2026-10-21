use std::fmt::Display;
use std::io::{self, Write};

use dstreamy_core::QuietOnError;
use serde::Serialize;

/// Stdout and stderr that never fail; once stdout is gone the command should wind down.
#[derive(Clone)]
pub struct Out {
    stdout: QuietOnError<io::Stdout>,
    stderr: QuietOnError<io::Stderr>,
    pub json: bool,
}

impl Out {
    pub fn new(json: bool) -> Self {
        Self {
            stdout: QuietOnError::new(io::stdout()),
            stderr: QuietOnError::new(io::stderr()),
            json,
        }
    }

    pub fn stderr_writer(&self) -> QuietOnError<io::Stderr> {
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
    fn table_pads_all_but_the_last_column() {
        let rows = vec![
            vec!["1".to_owned(), "General".to_owned(), "voice".to_owned()],
            vec!["22".to_owned(), "Gä".to_owned(), "stage".to_owned()],
        ];
        assert_eq!(table(&rows), "1   General  voice\n22  Gä       stage\n");
        assert_eq!(table(&[]), "");
    }
}
