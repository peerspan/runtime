// SPDX-License-Identifier: Apache-2.0
// Copyright The Peerspan Authors

//! Logs that a run's processes write, kept to a size, so a guest cannot fill the host's disk
//! through its console or by provoking its servers' error messages.

use std::fs::File;
use std::io::{self, ErrorKind, PipeWriter, Read, Write};
use std::path::{Path, PathBuf};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

/// How long [`CappedLog::tail`] waits for the copy to catch up with writers that have exited.
const SETTLE_TIMEOUT: Duration = Duration::from_millis(500);
/// Lines [`CappedLog::tail`] returns.
const TAIL_LINES: usize = 30;

/// A log file that keeps the first `limit` bytes written to its pipe, notes the cut, and reads
/// and discards the rest, so writers are never blocked or broken.
#[derive(Debug)]
pub struct CappedLog {
    path: PathBuf,
    copier: JoinHandle<()>,
}

impl CappedLog {
    /// Creates the file and returns the log with the pipe its writers get. The copy ends when
    /// every copy of the writer is closed.
    ///
    /// # Errors
    ///
    /// Fails if the file, the pipe or the copying thread cannot be created.
    pub fn create(path: &Path, limit: u64) -> io::Result<(CappedLog, PipeWriter)> {
        let file = File::create(path)?;
        let (reader, writer) = io::pipe()?;
        let copier = thread::Builder::new()
            .name("log".into())
            .spawn(move || copy_capped(reader, file, limit))?;
        Ok((
            CappedLog {
                path: path.to_owned(),
                copier,
            },
            writer,
        ))
    }

    /// The log's last lines, introduced as `what`. Waits briefly for the copy to finish, so
    /// that what writers that have just exited wrote is included.
    pub fn tail(&self, what: &str) -> String {
        let deadline = Instant::now() + SETTLE_TIMEOUT;
        while !self.copier.is_finished() && Instant::now() < deadline {
            thread::sleep(Duration::from_millis(10));
        }
        let Ok(log) = std::fs::read_to_string(&self.path) else {
            return format!("no {what} at {}", self.path.display());
        };
        let lines: Vec<&str> = log.lines().collect();
        let tail = lines[lines.len().saturating_sub(TAIL_LINES)..].join("\n");
        format!("{what} (last lines of {}):\n{tail}", self.path.display())
    }
}

fn copy_capped(mut reader: impl Read, mut file: impl Write, limit: u64) {
    let mut buf = vec![0; 64 << 10];
    let mut kept = 0;
    let mut cut = false;
    loop {
        let n = match reader.read(&mut buf) {
            Ok(0) => return,
            Ok(n) => n,
            Err(e) if e.kind() == ErrorKind::Interrupted => continue,
            Err(_) => return,
        };
        if cut {
            continue;
        }
        let room = usize::try_from(limit - kept).unwrap_or(usize::MAX);
        let take = n.min(room);
        if file.write_all(&buf[..take]).is_err() {
            cut = true;
            continue;
        }
        kept += take as u64;
        if take < n {
            cut = true;
            let _ = writeln!(file, "\n[peerspan-runtime: log cut at {limit} bytes]");
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn keeps_the_first_bytes_and_notes_the_cut() {
        let mut out = Vec::new();
        copy_capped(&b"0123456789"[..], &mut out, 4);
        assert_eq!(out, b"0123\n[peerspan-runtime: log cut at 4 bytes]\n");
    }

    #[test]
    fn leaves_a_log_within_the_limit_alone() {
        let mut out = Vec::new();
        copy_capped(&b"0123"[..], &mut out, 4);
        assert_eq!(out, b"0123");
    }
}
