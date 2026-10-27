// SPDX-License-Identifier: Apache-2.0
// Copyright The Peerspan Authors

//! Resource limits for each run, through a cgroup v2 of its own.
//!
//! The guest's memory and vCPUs are bounded by libkrun, but the processes behind it (the VMM
//! and the workspace server) are host processes. Each run's cgroup caps their memory, CPU time
//! and process count, so a run cannot exhaust its host, and kills them all at teardown.

use std::fs::{self, File, OpenOptions};
use std::io::{self, Write};
use std::os::fd::{AsRawFd, RawFd};
use std::path::{Path, PathBuf};
use std::thread;
use std::time::{Duration, Instant};

use anyhow::{Context, bail};

/// Where runs' cgroups go by default: a child of the root cgroup, which may hold processes
/// itself, so nothing else has to move.
pub const DEFAULT_ROOT: &str = "/sys/fs/cgroup/peerspan-runtime";
/// Memory allowed on top of the guest's own, for the VMM process and the workspace server.
const OVERHEAD_BYTES: u64 = 256 << 20;
/// Processes and threads allowed per run: vCPU and device threads, and the workspace server's.
const PIDS_MAX: u32 = 512;
/// The `cpu.max` period, in microseconds.
const CPU_PERIOD_US: u64 = 100_000;
/// How long teardown waits for killed processes to leave the cgroup.
const DRAIN_TIMEOUT: Duration = Duration::from_secs(5);
const CONTROLLERS: [&str; 3] = ["cpu", "memory", "pids"];

/// The limits of one run.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Limits {
    pub cpus: u8,
    pub guest_memory_mib: u32,
}

/// A run's cgroup, removed with everything in it when dropped.
#[derive(Debug)]
pub struct RunCgroup {
    path: PathBuf,
    procs: File,
}

impl RunCgroup {
    /// Creates `root/name` with `limits`, creating `root` and enabling its controllers first.
    ///
    /// # Errors
    ///
    /// Fails if cgroup v2 or one of the cpu, memory and pids controllers is unavailable; the
    /// run must not start without its limits.
    pub fn create(root: &Path, name: &str, limits: Limits) -> anyhow::Result<RunCgroup> {
        prepare_root(root)?;
        let path = root.join(name);
        fs::create_dir(&path).with_context(|| format!("creating cgroup {}", path.display()))?;
        let cgroup = RunCgroup {
            procs: OpenOptions::new()
                .write(true)
                .open(path.join("cgroup.procs"))
                .with_context(|| format!("opening {}/cgroup.procs", path.display()))?,
            path,
        };
        let memory = u64::from(limits.guest_memory_mib) * (1 << 20) + OVERHEAD_BYTES;
        let quota = u64::from(limits.cpus) * CPU_PERIOD_US;
        cgroup.set("memory.max", &memory.to_string())?;
        cgroup.set("memory.swap.max", "0")?;
        cgroup.set("pids.max", &PIDS_MAX.to_string())?;
        cgroup.set("cpu.max", &format!("{quota} {CPU_PERIOD_US}"))?;
        Ok(cgroup)
    }

    /// A descriptor for `cgroup.procs`; a child that writes `0` to it before exec joins.
    pub fn procs_fd(&self) -> RawFd {
        self.procs.as_raw_fd()
    }

    /// Whether the kernel killed a process here for exceeding the memory limit.
    pub fn oom_killed(&self) -> bool {
        fs::read_to_string(self.path.join("memory.events")).is_ok_and(|events| {
            events.lines().any(|line| {
                line.strip_prefix("oom_kill ")
                    .is_some_and(|n| n.trim() != "0")
            })
        })
    }

    /// The most memory the run's processes have used together, in bytes.
    pub fn memory_peak(&self) -> Option<u64> {
        fs::read_to_string(self.path.join("memory.peak"))
            .ok()?
            .trim()
            .parse()
            .ok()
    }

    /// Kills every process in the cgroup at once.
    pub fn kill(&self) {
        let _ = fs::write(self.path.join("cgroup.kill"), "1");
    }

    fn populated(&self) -> bool {
        fs::read_to_string(self.path.join("cgroup.events"))
            .is_ok_and(|events| events.lines().any(|line| line == "populated 1"))
    }

    fn set(&self, file: &str, value: &str) -> anyhow::Result<()> {
        fs::write(self.path.join(file), value)
            .with_context(|| format!("setting {} of {} to {value}", file, self.path.display()))
    }
}

impl Drop for RunCgroup {
    fn drop(&mut self) {
        self.kill();
        // A killed task leaves cgroup.procs before the cgroup stops counting it, and until then
        // the directory cannot be removed.
        let deadline = Instant::now() + DRAIN_TIMEOUT;
        while Instant::now() < deadline {
            if !self.populated() && fs::remove_dir(&self.path).is_ok() {
                return;
            }
            thread::sleep(Duration::from_millis(10));
        }
        eprintln!(
            "peerspan-runtime: could not remove cgroup {}",
            self.path.display()
        );
    }
}

fn prepare_root(root: &Path) -> anyhow::Result<()> {
    let Some(parent) = root.parent() else {
        bail!("cgroup root {} has no parent", root.display());
    };
    let available = fs::read_to_string(parent.join("cgroup.controllers"))
        .with_context(|| format!("{} is not a cgroup v2 directory", parent.display()))?;
    for controller in CONTROLLERS {
        if !available.split_whitespace().any(|c| c == controller) {
            bail!(
                "the {controller} controller is not available in {}",
                parent.display()
            );
        }
    }
    enable(parent)?;
    match fs::create_dir(root) {
        Ok(()) => {}
        Err(e) if e.kind() == io::ErrorKind::AlreadyExists => {}
        Err(e) => return Err(e).with_context(|| format!("creating cgroup {}", root.display())),
    }
    enable(root)
}

/// Enables the controllers for a cgroup's children, if they are not already.
fn enable(cgroup: &Path) -> anyhow::Result<()> {
    let file = cgroup.join("cgroup.subtree_control");
    let enabled = fs::read_to_string(&file)?;
    let missing: Vec<String> = CONTROLLERS
        .iter()
        .filter(|c| !enabled.split_whitespace().any(|e| e == **c))
        .map(|c| format!("+{c}"))
        .collect();
    if missing.is_empty() {
        return Ok(());
    }
    OpenOptions::new()
        .write(true)
        .open(&file)?
        .write_all(missing.join(" ").as_bytes())
        .with_context(|| format!("enabling {} in {}", missing.join(" "), cgroup.display()))
}
