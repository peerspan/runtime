// SPDX-License-Identifier: Apache-2.0
// Copyright The Peerspan Authors

//! Timestamps of a run's steps, for measuring where its time goes.
//!
//! The supervisor and the launcher stamp steps with the host's monotonic clock; the launcher
//! writes its stamps as `name nanoseconds` lines to a pipe the supervisor reads afterwards.

use std::fs::File;
use std::io::Write;
use std::os::fd::{FromRawFd, RawFd};

/// The host's monotonic clock, in nanoseconds; the same clock in every process.
pub fn monotonic_ns() -> u64 {
    let mut now = libc::timespec {
        tv_sec: 0,
        tv_nsec: 0,
    };
    // SAFETY: now is a valid out-pointer.
    unsafe { libc::clock_gettime(libc::CLOCK_MONOTONIC, &raw mut now) };
    u64::try_from(now.tv_sec).unwrap_or(0) * 1_000_000_000 + u64::try_from(now.tv_nsec).unwrap_or(0)
}

/// Writes the launcher's stamps to the descriptor the supervisor passed, if any.
#[derive(Debug)]
pub struct PhaseWriter(Option<File>);

impl PhaseWriter {
    /// Takes ownership of `fd`.
    ///
    /// # Safety
    ///
    /// `fd`, if given, must be an open descriptor that nothing else owns.
    pub unsafe fn from_fd(fd: Option<RawFd>) -> PhaseWriter {
        // SAFETY: forwarded from this function's contract.
        PhaseWriter(fd.map(|fd| unsafe { File::from_raw_fd(fd) }))
    }

    /// Stamps a step; measurement is best effort, so a failed write is ignored.
    pub fn mark(&mut self, name: &str) {
        if let Some(file) = self.0.as_mut() {
            let _ = writeln!(file, "{name} {}", monotonic_ns());
        }
    }
}

/// Parses `name nanoseconds` lines, skipping any that do not parse.
pub fn parse(text: &str) -> Vec<(String, u64)> {
    text.lines()
        .filter_map(|line| {
            let (name, ns) = line.split_once(' ')?;
            Some((name.to_owned(), ns.parse().ok()?))
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_reads_stamps_and_skips_garbage() {
        let parsed = parse("launcher 10\njailed 25\nbroken\nseccomp x\n");
        assert_eq!(
            parsed,
            [("launcher".to_owned(), 10), ("jailed".to_owned(), 25)]
        );
    }
}
