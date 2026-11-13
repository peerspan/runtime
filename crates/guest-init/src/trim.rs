// SPDX-License-Identifier: Apache-2.0
// Copyright The Peerspan Authors

//! Gives idle page cache back, so a run's host memory follows what the guest uses.
//!
//! Free page reporting returns free guest memory to the host, but page cache is not free: after
//! a build or a walk over a repository the guest keeps gigabytes of file pages, and the guest
//! kernel drops them only under memory pressure, which nothing in a VM of its own size causes.
//! So the host would keep the high-water mark of the run's last heavy step.
//!
//! Init moves itself, and so the run's command, into the cgroup [`RUN_CGROUP`], since the root
//! cgroup has no `memory.reclaim`. Once the guest has been quiet for [`QUIET`] (little CPU, no
//! memory stalls), this thread asks the kernel to reclaim page cache there, a quarter of
//! what is above a floor per [`TICK`]. It stops as soon as the guest is busy or stalls on
//! memory, so a run still reading its files keeps them. An agent waiting on its model is quiet;
//! a build is not.
//!
//! Reclaimed cache lies scattered between pages still in use, so little of it forms the 4 MiB
//! blocks free page reporting waits for by default. While trimming, and for [`FINE_TICKS`]
//! after, reporting goes down to [`FINE_REPORTING_ORDER`] so the host gets those pieces back
//! too. It goes back up when the guest is busy, where reporting every small free would cost
//! refaults.
//!
//! The design follows smolvm's guest memory trim (smol-machines/smolvm, Apache-2.0).

use std::fs;
use std::io::{self, Write};
use std::thread;
use std::time::Duration;

/// How often the guest is sampled.
pub const TICK: Duration = Duration::from_secs(5);
/// Quiet time before the first reclaim.
const QUIET: Duration = Duration::from_secs(60);
/// CPU the guest may use, in cores, and still be quiet.
const QUIET_CORES: f64 = 0.25;
/// Share of a tick the guest may spend stalled on memory and still be quiet.
const STALL_LIMIT: f64 = 0.001;
/// Smallest and largest single reclaim request.
const MIN_STEP: u64 = 32 << 20;
const MAX_STEP: u64 = 2 << 30;
/// Cache growth since the last trim that makes another one worthwhile.
const REARM_GROWTH: u64 = 64 << 20;
/// Reporting order while trimming: 16 KiB blocks.
const FINE_REPORTING_ORDER: &str = "2";
/// Ticks after the last reclaim that reporting stays fine.
const FINE_TICKS: u32 = 3;

/// The cgroup the run is in, with the memory controller.
const RUN_CGROUP: &str = "/sys/fs/cgroup/run";
const REPORTING_ORDER_PATH: &str = "/sys/module/page_reporting/parameters/page_reporting_order";

/// Page cache left in place: the binaries and libraries that are running.
fn floor(total: u64) -> u64 {
    (total / 32).clamp(64 << 20, 256 << 20)
}

/// One tick's view of the guest.
#[derive(Debug, Clone, Copy)]
pub struct Sample {
    /// CPU used over the tick, in cores, without this thread's own.
    pub busy_cores: f64,
    /// Share of the tick some task was stalled on memory, without this thread's reclaim.
    pub stall: f64,
    /// Bytes of file-backed page cache.
    pub file: u64,
    /// Bytes of guest memory.
    pub total: u64,
}

/// What the trimmer does this tick.
#[derive(Debug, PartialEq, Eq)]
pub enum Action {
    /// Nothing to do; `fine` says whether reporting should be fine.
    Wait { fine: bool },
    /// Reclaim this many bytes of page cache, with reporting fine.
    Reclaim(u64),
}

/// The trim policy, free of I/O so it can be tested.
#[derive(Debug, Default)]
pub struct Trimmer {
    quiet: Duration,
    /// A trim is under way: it goes on each quiet tick until the floor.
    trimming: bool,
    /// Cache left by the last trim, which a new one must exceed by [`REARM_GROWTH`].
    trimmed_to: Option<u64>,
    /// Ticks reporting stays fine.
    fine_ticks: u32,
}

impl Trimmer {
    pub fn next(&mut self, s: &Sample) -> Action {
        if s.busy_cores > QUIET_CORES || s.stall > STALL_LIMIT {
            self.quiet = Duration::ZERO;
            // A trim that activity interrupted resumes after the next quiet minute; only one
            // that reached the floor, or that the kernel could not finish, waits for growth.
            if std::mem::take(&mut self.trimming) {
                self.trimmed_to = None;
            }
            self.fine_ticks = 0;
            return Action::Wait { fine: false };
        }
        self.quiet += TICK;
        let above = s.file.saturating_sub(floor(s.total));
        if above < MIN_STEP {
            self.trimming = false;
            self.trimmed_to = Some(s.file);
        }
        let grown = self.trimmed_to.is_none_or(|to| s.file >= to + REARM_GROWTH);
        if self.quiet < QUIET || above < MIN_STEP || !(self.trimming || grown) {
            let fine = self.fine_ticks > 0;
            self.fine_ticks = self.fine_ticks.saturating_sub(1);
            return Action::Wait { fine };
        }
        self.trimming = true;
        self.fine_ticks = FINE_TICKS;
        Action::Reclaim((above / 4).clamp(MIN_STEP, MAX_STEP).min(above))
    }

    /// Records a reclaim. `complete` is false when the kernel could not reclaim it all, which
    /// ends the trim until the cache grows again.
    pub fn reclaimed(&mut self, complete: bool, file_after: u64) {
        self.trimmed_to = Some(file_after);
        self.trimming &= complete;
    }
}

/// Moves init into [`RUN_CGROUP`] and starts the trim thread. Without cgroup2's memory
/// controller, the run goes on with its cache.
pub fn spawn() {
    let setup = || -> io::Result<()> {
        fs::write("/sys/fs/cgroup/cgroup.subtree_control", "+memory")?;
        fs::create_dir(RUN_CGROUP)?;
        fs::write(format!("{RUN_CGROUP}/cgroup.procs"), "0")
    };
    if setup().is_ok() {
        thread::spawn(run);
    }
}

fn run() {
    let mut trimmer = Trimmer::default();
    let mut probe = Probe::new();
    let mut saved_order: Option<String> = None;
    loop {
        thread::sleep(TICK);
        let Some(sample) = probe.sample() else {
            continue;
        };
        let action = trimmer.next(&sample);
        let fine = !matches!(action, Action::Wait { fine: false });
        match (fine, &saved_order) {
            (true, None) => saved_order = lower_reporting_order(),
            (false, Some(order)) => {
                let _ = fs::write(REPORTING_ORDER_PATH, order);
                saved_order = None;
            }
            _ => {}
        }
        if let Action::Reclaim(bytes) = action {
            let complete = probe.reclaim(bytes);
            trimmer.reclaimed(complete, probe.file_bytes().unwrap_or(sample.file));
        }
    }
}

/// Sets reporting fine and returns the order it was at.
fn lower_reporting_order() -> Option<String> {
    let order = fs::read_to_string(REPORTING_ORDER_PATH).ok()?;
    fs::write(REPORTING_ORDER_PATH, FINE_REPORTING_ORDER).ok()?;
    Some(order.trim().to_owned())
}

/// Reads the guest's counters and turns two readings into a [`Sample`].
struct Probe {
    last: Option<Counters>,
    /// Stall during this thread's reclaims since the last sample, in microseconds.
    own_stall_us: u64,
}

#[derive(Clone, Copy)]
struct Counters {
    /// Busy and total CPU time over all CPUs, in clock ticks.
    busy: u64,
    all: u64,
    cpus: u64,
    /// This thread's CPU time, in microseconds.
    own_us: u64,
    /// `some` memory stall, in microseconds.
    stall_us: u64,
}

impl Probe {
    fn new() -> Self {
        Probe {
            last: None,
            own_stall_us: 0,
        }
    }

    fn sample(&mut self) -> Option<Sample> {
        let now = counters()?;
        let last = self.last.replace(now)?;
        let own_stall = std::mem::take(&mut self.own_stall_us);
        let all = now.all.saturating_sub(last.all).max(1);
        let tick_us = all as f64 / now.cpus as f64 / clock_ticks() as f64 * 1e6;
        let own_ticks = now.own_us.saturating_sub(last.own_us) as f64 * clock_ticks() as f64 / 1e6;
        let busy = (now.busy.saturating_sub(last.busy) as f64 - own_ticks).max(0.0);
        let stall = now
            .stall_us
            .saturating_sub(last.stall_us)
            .saturating_sub(own_stall);
        let (file, total) = meminfo()?;
        Some(Sample {
            busy_cores: busy / all as f64 * now.cpus as f64,
            stall: stall as f64 / tick_us,
            file,
            total,
        })
    }

    fn file_bytes(&self) -> Option<u64> {
        meminfo().map(|(file, _)| file)
    }

    /// Asks for `bytes` of reclaim; false if the kernel could not reclaim them all.
    fn reclaim(&mut self, bytes: u64) -> bool {
        let before = memory_stall_us();
        let result = fs::OpenOptions::new()
            .write(true)
            .open(format!("{RUN_CGROUP}/memory.reclaim"))
            .and_then(|mut f| f.write_all(bytes.to_string().as_bytes()));
        if let (Some(before), Some(after)) = (before, memory_stall_us()) {
            self.own_stall_us += after.saturating_sub(before);
        }
        result.is_ok()
    }
}

fn counters() -> Option<Counters> {
    let stat = fs::read_to_string("/proc/stat").ok()?;
    let mut lines = stat.lines();
    let fields: Vec<u64> = lines
        .next()?
        .strip_prefix("cpu ")?
        .split_whitespace()
        .filter_map(|v| v.parse().ok())
        .collect();
    // user nice system idle iowait irq softirq steal
    let all: u64 = fields.iter().take(8).sum();
    let idle = fields.get(3)? + fields.get(4)?;
    let cpus = lines.filter(|l| l.starts_with("cpu")).count().max(1) as u64;
    Some(Counters {
        busy: all - idle,
        all,
        cpus,
        own_us: thread_cpu_us(),
        stall_us: memory_stall_us()?,
    })
}

fn thread_cpu_us() -> u64 {
    let t = rustix::time::clock_gettime(rustix::time::ClockId::ThreadCPUTime);
    t.tv_sec.unsigned_abs() * 1_000_000 + t.tv_nsec.unsigned_abs() / 1000
}

/// `/proc/stat` counts in USER_HZ, which is 100 on x86-64 and arm64.
fn clock_ticks() -> u64 {
    100
}

/// The `some` total of `/proc/pressure/memory`, in microseconds.
fn memory_stall_us() -> Option<u64> {
    let pressure = fs::read_to_string("/proc/pressure/memory").ok()?;
    pressure
        .lines()
        .find(|l| l.starts_with("some "))?
        .split_whitespace()
        .find_map(|f| f.strip_prefix("total="))?
        .parse()
        .ok()
}

/// The run's file-backed page cache, without tmpfs, and the guest's memory, in bytes.
fn meminfo() -> Option<(u64, u64)> {
    let stat = fs::read_to_string(format!("{RUN_CGROUP}/memory.stat")).ok()?;
    let field = |key: &str| -> Option<u64> {
        stat.lines()
            .find_map(|l| l.strip_prefix(key)?.strip_prefix(' '))?
            .parse()
            .ok()
    };
    let total_kib: u64 = fs::read_to_string("/proc/meminfo")
        .ok()?
        .lines()
        .find_map(|l| l.strip_prefix("MemTotal:"))?
        .trim()
        .trim_end_matches(" kB")
        .parse()
        .ok()?;
    Some((
        field("active_file")? + field("inactive_file")?,
        total_kib << 10,
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    const GIB: u64 = 1 << 30;

    fn quiet(file: u64) -> Sample {
        Sample {
            busy_cores: 0.05,
            stall: 0.0,
            file,
            total: 12 * GIB,
        }
    }

    fn busy(file: u64) -> Sample {
        Sample {
            busy_cores: 3.5,
            ..quiet(file)
        }
    }

    fn ticks_to_quiet() -> u32 {
        (QUIET.as_secs() / TICK.as_secs()) as u32
    }

    #[test]
    fn waits_for_a_quiet_minute_before_reclaiming() {
        let mut t = Trimmer::default();
        for _ in 1..ticks_to_quiet() {
            assert_eq!(t.next(&quiet(8 * GIB)), Action::Wait { fine: false });
        }
        let floor = floor(12 * GIB);
        assert_eq!(
            t.next(&quiet(8 * GIB)),
            Action::Reclaim(MAX_STEP.min((8 * GIB - floor) / 4))
        );
    }

    #[test]
    fn a_busy_tick_starts_the_quiet_minute_again() {
        let mut t = Trimmer::default();
        for _ in 1..ticks_to_quiet() {
            t.next(&quiet(8 * GIB));
        }
        assert_eq!(t.next(&busy(8 * GIB)), Action::Wait { fine: false });
        assert_eq!(t.next(&quiet(8 * GIB)), Action::Wait { fine: false });
    }

    #[test]
    fn a_memory_stall_is_not_quiet() {
        let mut t = Trimmer::default();
        for _ in 0..ticks_to_quiet() {
            t.next(&quiet(8 * GIB));
        }
        let stalled = Sample {
            stall: 0.01,
            ..quiet(8 * GIB)
        };
        assert_eq!(t.next(&stalled), Action::Wait { fine: false });
    }

    #[test]
    fn trims_in_steps_down_to_the_floor_then_stops() {
        let mut t = Trimmer::default();
        for _ in 1..ticks_to_quiet() {
            t.next(&quiet(GIB));
        }
        let floor = floor(12 * GIB);
        let mut file = GIB;
        let mut steps = 0;
        while let Action::Reclaim(bytes) = t.next(&quiet(file)) {
            assert!((MIN_STEP..=MAX_STEP).contains(&bytes));
            file -= bytes;
            t.reclaimed(true, file);
            steps += 1;
            assert!(steps < 20);
        }
        assert!(
            file >= floor && file < floor + MIN_STEP,
            "stopped at {file}"
        );
        // Reporting stays fine for FINE_TICKS ticks after the last reclaim, the first of them
        // the one that ended the loop, then goes back.
        for _ in 1..FINE_TICKS {
            assert_eq!(t.next(&quiet(file)), Action::Wait { fine: true });
        }
        assert_eq!(t.next(&quiet(file)), Action::Wait { fine: false });
        // Quiet with no growth: no new trim.
        assert_eq!(
            t.next(&quiet(file + REARM_GROWTH / 2)),
            Action::Wait { fine: false }
        );
    }

    #[test]
    fn trims_again_only_once_the_cache_has_grown() {
        let mut t = Trimmer::default();
        for _ in 1..ticks_to_quiet() {
            t.next(&quiet(GIB));
        }
        let Action::Reclaim(_) = t.next(&quiet(GIB)) else {
            panic!("no reclaim")
        };
        // The kernel could not reclaim it all: the trim ends there.
        t.reclaimed(false, 900 << 20);
        for _ in 0..FINE_TICKS + 1 {
            assert!(matches!(t.next(&quiet(900 << 20)), Action::Wait { .. }));
        }
        // A build: busy, and the cache grows. The next quiet minute trims again.
        t.next(&busy(2 * GIB));
        for _ in 1..ticks_to_quiet() {
            assert!(matches!(t.next(&quiet(2 * GIB)), Action::Wait { .. }));
        }
        assert!(matches!(t.next(&quiet(2 * GIB)), Action::Reclaim(_)));
    }

    #[test]
    fn an_interrupted_trim_resumes_after_the_next_quiet_minute() {
        let mut t = Trimmer::default();
        for _ in 1..ticks_to_quiet() {
            t.next(&quiet(8 * GIB));
        }
        let Action::Reclaim(bytes) = t.next(&quiet(8 * GIB)) else {
            panic!("no reclaim")
        };
        let left = 8 * GIB - bytes;
        t.reclaimed(true, left);
        t.next(&busy(left));
        for _ in 1..ticks_to_quiet() {
            assert!(matches!(t.next(&quiet(left)), Action::Wait { .. }));
        }
        assert!(matches!(t.next(&quiet(left)), Action::Reclaim(_)));
    }

    #[test]
    fn the_floor_keeps_a_little_cache() {
        assert_eq!(floor(GIB), 64 << 20);
        assert_eq!(floor(4 * GIB), 128 << 20);
        assert_eq!(floor(12 * GIB), 256 << 20);
    }
}
