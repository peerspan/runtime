// SPDX-License-Identifier: Apache-2.0
// Copyright The Peerspan Authors

//! Resource limits for each run, through a cgroup v2 of its own.
//!
//! The guest's memory and vCPUs are bounded by libkrun, but the processes behind it (the VMM
//! and the workspace server) are host processes. Each run's cgroup caps their memory, CPU time,
//! process count and disk I/O, so a run cannot exhaust its host, and kills them all at teardown.
//!
//! A supervisor holds an exclusive lock on its run's cgroup directory for as long as the run
//! lasts, and the kernel releases it when the supervisor dies however it dies. A cgroup whose
//! lock is free was left by a supervisor that died before tearing it down; [`stale`] finds
//! them. Creating a cgroup and taking its lock happen under a shared lock on the root, and
//! [`stale`] takes the root exclusively, so it never sees a new cgroup before its lock is held.

use std::fs::{self, File, OpenOptions};
use std::io::{self, Write};
use std::os::fd::{AsRawFd, RawFd};
use std::os::unix::fs::{MetadataExt, OpenOptionsExt};
use std::path::{Path, PathBuf};
use std::thread;
use std::time::{Duration, Instant};

use anyhow::{Context, bail};

/// Where runs' cgroups go by default: a child of the root cgroup, which may hold processes
/// itself, so nothing else has to move.
pub const DEFAULT_ROOT: &str = "/sys/fs/cgroup/peerspan-runtime";
/// Disk bandwidth for each run unless set otherwise, in MiB/s each for reading and writing:
/// enough for builds and tests, and a fraction of what a solid-state disk sustains.
pub const DEFAULT_IO_MIBPS: u32 = 256;
/// Disk operations per second for each run unless set otherwise, each for reading and writing.
pub const DEFAULT_IO_IOPS: u32 = 10_000;
/// Memory allowed on top of the guest's own, for the VMM process and the workspace server.
const OVERHEAD_BYTES: u64 = 256 << 20;
/// Processes and threads allowed per run: vCPU and device threads, and the workspace server's.
const PIDS_MAX: u32 = 512;
/// The `cpu.max` period, in microseconds.
const CPU_PERIOD_US: u64 = 100_000;
/// How long teardown waits for killed processes to leave the cgroup.
const DRAIN_TIMEOUT: Duration = Duration::from_secs(5);
const CONTROLLERS: [&str; 4] = ["cpu", "io", "memory", "pids"];

/// The limits of one run.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Limits {
    pub cpus: u8,
    pub guest_memory_mib: u32,
    pub io: IoRate,
    /// The disks whose I/O `io` caps, as major and minor device numbers.
    pub disks: Vec<(u32, u32)>,
}

/// A cap on disk I/O, applied to reading and to writing separately on each disk.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct IoRate {
    pub mib_per_second: u32,
    pub ops_per_second: u32,
}

/// A run's cgroup, removed with everything in it when dropped.
#[derive(Debug)]
pub struct RunCgroup {
    path: PathBuf,
    procs: File,
    /// The cgroup's directory, locked exclusively while the run is live.
    _lock: File,
}

impl RunCgroup {
    /// Creates `root/name` with `limits`, creating `root` and enabling its controllers first.
    ///
    /// # Errors
    ///
    /// Fails if cgroup v2 or one of the cpu, io, memory and pids controllers is unavailable;
    /// the run must not start without its limits.
    pub fn create(root: &Path, name: &str, limits: &Limits) -> anyhow::Result<RunCgroup> {
        prepare_root(root)?;
        let root_lock = lock(root, libc::LOCK_SH)?;
        let path = root.join(name);
        fs::create_dir(&path).with_context(|| format!("creating cgroup {}", path.display()))?;
        let lock = lock(&path, libc::LOCK_EX | libc::LOCK_NB)?;
        drop(root_lock);
        let cgroup = RunCgroup::open(path, lock)?;
        let memory = u64::from(limits.guest_memory_mib) * (1 << 20) + OVERHEAD_BYTES;
        let quota = u64::from(limits.cpus) * CPU_PERIOD_US;
        cgroup.set("memory.max", &memory.to_string())?;
        cgroup.set("memory.swap.max", "0")?;
        cgroup.set("pids.max", &PIDS_MAX.to_string())?;
        cgroup.set("cpu.max", &format!("{quota} {CPU_PERIOD_US}"))?;
        let bytes = u64::from(limits.io.mib_per_second) << 20;
        let ops = limits.io.ops_per_second;
        for (major, minor) in &limits.disks {
            cgroup.set(
                "io.max",
                &format!("{major}:{minor} rbps={bytes} wbps={bytes} riops={ops} wiops={ops}"),
            )?;
        }
        Ok(cgroup)
    }

    fn open(path: PathBuf, lock: File) -> anyhow::Result<RunCgroup> {
        Ok(RunCgroup {
            procs: OpenOptions::new()
                .write(true)
                .open(path.join("cgroup.procs"))
                .with_context(|| format!("opening {}/cgroup.procs", path.display()))?,
            path,
            _lock: lock,
        })
    }

    /// The cgroup's directory.
    pub fn path(&self) -> &Path {
        &self.path
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

/// The run cgroups under `root` that no live supervisor holds, locked and ready to drop, which
/// kills what is left in them and removes them.
///
/// # Errors
///
/// Fails if `root` cannot be read or locked; a missing root has nothing stale in it.
pub fn stale(root: &Path) -> anyhow::Result<Vec<RunCgroup>> {
    let entries = match fs::read_dir(root) {
        Ok(entries) => entries,
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(vec![]),
        Err(e) => return Err(e).with_context(|| format!("reading {}", root.display())),
    };
    let _root_lock = lock(root, libc::LOCK_EX)?;
    let mut stale = vec![];
    for entry in entries {
        let entry = entry?;
        if !entry.file_name().to_string_lossy().starts_with("run-") || !entry.path().is_dir() {
            continue;
        }
        // A cgroup removed since the listing is someone else's finished run.
        if let Ok(lock) = lock(&entry.path(), libc::LOCK_EX | libc::LOCK_NB) {
            stale.push(RunCgroup::open(entry.path(), lock)?);
        }
    }
    Ok(stale)
}

/// The disk holding `path`, as major and minor device numbers, for `io.max`, which takes whole
/// disks rather than partitions. None if no block device holds it, as for tmpfs, overlayfs or
/// btrfs, whose files report a device number of their own.
pub fn disk_of(path: &Path) -> Option<(u32, u32)> {
    let dev = fs::metadata(path).ok()?.dev();
    let device = fs::canonicalize(format!(
        "/sys/dev/block/{}:{}",
        libc::major(dev),
        libc::minor(dev)
    ))
    .ok()?;
    let disk = if device.join("partition").exists() {
        device.parent()?
    } else {
        &device
    };
    let numbers = fs::read_to_string(disk.join("dev")).ok()?;
    let (major, minor) = numbers.trim().split_once(':')?;
    Some((major.parse().ok()?, minor.parse().ok()?))
}

/// Opens directory `path` and takes `operation`, a flock(2) operation, on it.
fn lock(path: &Path, operation: libc::c_int) -> anyhow::Result<File> {
    let dir = OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_DIRECTORY)
        .open(path)
        .with_context(|| format!("opening {}", path.display()))?;
    // SAFETY: flock takes a descriptor this function owns and an integer.
    if unsafe { libc::flock(dir.as_raw_fd(), operation) } != 0 {
        return Err(io::Error::last_os_error())
            .with_context(|| format!("locking {}", path.display()));
    }
    Ok(dir)
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
