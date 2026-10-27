// SPDX-License-Identifier: Apache-2.0
// Copyright The Peerspan Authors

//! Runs one command in one microVM: serves the workspace, starts the launcher, hands the
//! command and the workspace archive to the guest, and reports what happens as [`Event`]s.

use std::fs::{self, File};
use std::io::{self, ErrorKind, Read};
use std::net::Shutdown;
use std::os::fd::{AsRawFd, RawFd};
use std::os::unix::fs::DirBuilderExt;
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::Mutex;
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use std::os::unix::process::CommandExt;

use anyhow::{Context, bail};
use peerspan_runtime_protocol::{self as protocol, DATA_CHUNK, Message, WORKSPACE_TAG};

use crate::cgroup::{self, Limits, RunCgroup};
use crate::net::{self, Level, RunFilter};
use crate::phases;

use crate::launcher::{AGENT_SOCKET, WORKSPACE_SOCKET};

/// How long the workspace server has to create its socket.
const SERVER_READY_TIMEOUT: Duration = Duration::from_secs(5);
/// How long the guest has, from launch, to boot and connect.
const GUEST_CONNECT_TIMEOUT: Duration = Duration::from_secs(60);
/// How long the launcher has to exit after the guest reports the command's exit.
const SHUTDOWN_TIMEOUT: Duration = Duration::from_secs(10);
const POLL_INTERVAL: Duration = Duration::from_millis(20);
/// Each run's VMM runs as its own user and group, numbered from this base, which is far above
/// the IDs distributions assign so that it never matches a real account.
const RUN_ID_BASE: u32 = 0x7000_0000;
/// Low bits that vary between runs; a collision needs two concurrent runs with equal bits.
const RUN_ID_MASK: u32 = 0x00ff_ffff;
/// Console lines shown when the VM fails.
const CONSOLE_TAIL_LINES: usize = 30;

/// How often the watchdog checks for cancellation and the timeout.
const WATCH_INTERVAL: Duration = Duration::from_millis(50);

/// One command to run in one VM.
#[derive(Debug)]
pub struct VmSpec {
    /// Host directory shared as the guest's workspace. With an archive it must be empty.
    pub workspace: PathBuf,
    /// A tar archive, optionally gzip-compressed, unpacked into the workspace inside the VM.
    pub archive: Option<PathBuf>,
    pub guest: PathBuf,
    pub libkrun: PathBuf,
    pub virtiofsd: PathBuf,
    /// The passt binary that carries the guest's network traffic.
    pub passt: PathBuf,
    /// What the guest may reach.
    pub network: Level,
    pub cpus: u8,
    pub memory_mib: u32,
    /// How long the command may run once started.
    pub timeout: Option<Duration>,
    pub console_log: Option<PathBuf>,
    /// The cgroup v2 directory each run's own cgroup is created in.
    pub cgroup_root: PathBuf,
    /// Extra kernel command-line arguments, for experiments.
    pub kernel_args: Option<String>,
    /// The command; its `archive` flag is set from `archive` above.
    pub command: protocol::Command,
}

/// Something the run reported.
#[derive(Debug)]
pub enum Event<'data> {
    /// The command started.
    Started,
    Stdout(&'data [u8]),
    Stderr(&'data [u8]),
    /// A piece of `changes.tar`, when the command asked for changes.
    Changes(&'data [u8]),
    /// A piece of `deleted.txt`, when the command asked for changes.
    Deleted(&'data [u8]),
}

/// How a run ended.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Outcome {
    /// The command's exit status, if it exited.
    pub exit_code: Option<i32>,
    /// The signal that ended it; 9 after cancellation.
    pub signal: Option<i32>,
    pub timed_out: bool,
    /// From the start of the command to its end; zero if it never started.
    pub duration: Duration,
    /// Why the run failed outside the command, if it did.
    pub error: Option<String>,
    /// How long setup took and how much memory the run used.
    pub metrics: Metrics,
}

/// Measurements of one run, for tracking boot time and memory.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Metrics {
    /// From starting the launcher until the guest connected: VMM setup, kernel boot and init.
    pub connect: Duration,
    /// From starting the launcher until the command started.
    pub start: Option<Duration>,
    /// From the start of `run` until the VM was torn down.
    pub total: Duration,
    /// The most host memory the VMM and the workspace server used together.
    pub memory_peak_bytes: Option<u64>,
    /// When the supervisor and the launcher reached each step, since the start of `run`.
    pub phases: Vec<(String, Duration)>,
    /// When guest init reached each step, by the guest's boot clock.
    pub guest_phases: Vec<(String, Duration)>,
}

/// What the conversation with the guest has reached, shared with the watchdog.
#[derive(Debug, Default)]
struct Progress {
    /// When the command started.
    started: Option<Instant>,
    /// Host stamps of the conversation's steps.
    stamps: Vec<(String, u64)>,
    /// The guest's own phases, in microseconds of its boot clock.
    guest_phases: Vec<(String, u64)>,
}

/// Why the watchdog stopped a run.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Stop {
    Cancelled,
    TimedOut,
}

/// Runs the command, passing each event to `on_event`, and returns how it ended. Setting
/// `cancel` stops the VM.
///
/// # Errors
///
/// Fails if the VM cannot be set up or the guest never connects. A failure after the guest
/// connects is reported in [`Outcome::error`] instead.
pub fn run(
    spec: &VmSpec,
    cancel: &AtomicBool,
    on_event: &mut dyn FnMut(Event<'_>) -> io::Result<()>,
) -> anyhow::Result<Outcome> {
    let options = spec;
    let began = Instant::now();
    let began_ns = phases::monotonic_ns();
    let mut stamps: Vec<(String, u64)> = Vec::new();
    let mut mark = |name: &str| stamps.push((name.to_owned(), phases::monotonic_ns()));
    if let Some(archive) = &options.archive
        && fs::read_dir(&options.workspace)?.next().is_some()
    {
        bail!(
            "the workspace must be empty to unpack {} into it",
            archive.display()
        );
    }
    let workspace = fs::canonicalize(&options.workspace)
        .with_context(|| format!("workspace {}", options.workspace.display()))?;
    let guest = GuestImage::open(&options.guest)?;
    // SAFETY: geteuid has no preconditions.
    if unsafe { libc::geteuid() } != 0 {
        bail!("peerspan-runtime must run as root to confine its VMs");
    }
    remove_stale_runs(&options.cgroup_root);
    let run_dir = RunDir::create()?;
    let id = run_dir.id;
    let sockets = run_dir.path.join("sockets");
    fs::DirBuilder::new().mode(0o700).create(&sockets)?;
    chown(&sockets, id)?;
    let jail = run_dir.path.join("jail");
    fs::DirBuilder::new().mode(0o700).create(&jail)?;
    let cgroup = RunCgroup::create(
        &options.cgroup_root,
        &format!("run-{id}"),
        Limits {
            cpus: options.cpus,
            guest_memory_mib: options.memory_mib,
        },
    )?;
    mark("cgroup");
    let console_path = options
        .console_log
        .clone()
        .unwrap_or_else(|| run_dir.path.join("console.log"));

    let workspace_socket = sockets.join(WORKSPACE_SOCKET);
    let _server = ChildGuard(
        join_cgroup(&mut Command::new(&options.virtiofsd), &cgroup)
            .arg(format!("--socket-path={}", workspace_socket.display()))
            .arg(format!("--shared-dir={}", workspace.display()))
            .arg(format!("--tag={WORKSPACE_TAG}"))
            .arg("--cache=auto")
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(File::create(run_dir.path.join("virtiofsd.log"))?)
            .spawn()
            .with_context(|| format!("starting {}", options.virtiofsd.display()))?,
    );
    wait_for_path(&workspace_socket, SERVER_READY_TIMEOUT)
        .context("workspace server did not create its socket")?;
    chown(&workspace_socket, id)?;
    mark("workspace_server");

    // Declared after the cgroup and dropped before it: passt first, then the filter, which
    // names the cgroup by its path.
    let mut network = None;
    if options.network > Level::None {
        let filter = RunFilter::install(cgroup.path(), options.network)?;
        let (guest_end, passt_end) = UnixStream::pair()?;
        let mut passt_command = Command::new(&options.passt);
        join_cgroup(&mut passt_command, &cgroup);
        pass_fd(&mut passt_command, passt_end.as_raw_fd(), PASST_FD);
        let passt = ChildGuard(
            passt_command
                .arg("--fd")
                .arg(PASST_FD.to_string())
                .args(net::passt_args())
                .stdin(Stdio::null())
                .stdout(Stdio::null())
                .stderr(File::create(run_dir.path.join("passt.log"))?)
                .spawn()
                .with_context(|| format!("starting {}", options.passt.display()))?,
        );
        drop(passt_end);
        network = Some((guest_end, passt, filter));
        mark("network");
    }

    let agent_socket = sockets.join(AGENT_SOCKET);
    let listener = UnixListener::bind(&agent_socket)
        .with_context(|| format!("binding {}", agent_socket.display()))?;
    chown(&agent_socket, id)?;

    let console = File::create(&console_path)
        .with_context(|| format!("creating {}", console_path.display()))?;
    let (mut phase_reader, phase_writer) = io::pipe()?;
    let launched = Instant::now();
    mark("launched");
    let mut launcher_command =
        Command::new(std::env::current_exe().context("locating this executable")?);
    join_cgroup(&mut launcher_command, &cgroup);
    pass_fd(&mut launcher_command, phase_writer.as_raw_fd(), PHASES_FD);
    if let Some((guest_end, _, _)) = &network {
        pass_fd(&mut launcher_command, guest_end.as_raw_fd(), NET_FD);
    }
    let mut launcher = ChildGuard(
        launcher_command
            .arg("launch")
            .arg("--phases-fd")
            .arg(PHASES_FD.to_string())
            .arg("--libkrun")
            .arg(&options.libkrun)
            .arg("--kernel")
            .arg(&guest.kernel)
            .arg("--initrd")
            .arg(&guest.initrd)
            .arg("--rootfs")
            .arg(&guest.rootfs)
            .arg("--sockets")
            .arg(&sockets)
            .arg("--jail")
            .arg(&jail)
            .arg("--uid")
            .arg(id.to_string())
            .arg("--gid")
            .arg(id.to_string())
            .arg("--cpus")
            .arg(options.cpus.to_string())
            .arg("--memory-mib")
            .arg(options.memory_mib.to_string())
            .args(
                options
                    .kernel_args
                    .iter()
                    .flat_map(|extra| ["--kernel-args", extra.as_str()]),
            )
            .args(
                network
                    .iter()
                    .flat_map(|_| ["--net-fd".to_owned(), NET_FD.to_string()]),
            )
            .stdin(Stdio::null())
            .stdout(console.try_clone()?)
            .stderr(console)
            .spawn()
            .context("starting the launcher")?,
    );
    drop(phase_writer);
    let _network = network.map(|(_guest_end, passt, filter)| (passt, filter));

    let stream = match accept_guest(&listener, &mut launcher.0, cancel) {
        Ok(stream) => stream,
        Err(e) => return Err(e.context(console_tail(&console_path))),
    };
    let connect = launched.elapsed();
    mark("connected");
    let progress = Mutex::new(Progress::default());
    let mut outcome = serve_guest(spec, stream, &cgroup, &progress, cancel, on_event);
    if cgroup.oom_killed() && outcome.exit_code.is_none() && !outcome.timed_out {
        outcome.error = Some("the VM exceeded its memory limit and was killed".into());
    }
    let shut_down = wait_with_timeout(&mut launcher.0, SHUTDOWN_TIMEOUT)?.is_some();
    if !shut_down {
        eprintln!("peerspan-runtime: the VM did not shut down; stopping it");
    }
    mark("vm_exited");
    let progress = progress.into_inner().expect("lock poisoned");
    if shut_down {
        // Every writer has exited, so this reads to the end without blocking.
        let mut text = String::new();
        let _ = phase_reader.read_to_string(&mut text);
        stamps.extend(phases::parse(&text));
    }
    stamps.extend(progress.stamps);
    stamps.sort_by_key(|(_, ns)| *ns);
    let since_start = |ns: u64| Duration::from_nanos(ns.saturating_sub(began_ns));
    outcome.metrics = Metrics {
        connect,
        start: progress.started.map(|at| at.duration_since(launched)),
        total: began.elapsed(),
        memory_peak_bytes: cgroup.memory_peak(),
        phases: stamps
            .into_iter()
            .map(|(name, ns)| (name, since_start(ns)))
            .collect(),
        guest_phases: progress
            .guest_phases
            .into_iter()
            .map(|(name, micros)| (name, Duration::from_micros(micros)))
            .collect(),
    };
    Ok(outcome)
}

/// Hands the guest its command and archive, and relays its messages until it reports an exit,
/// the watchdog stops it, or the connection ends.
fn serve_guest(
    spec: &VmSpec,
    mut stream: UnixStream,
    cgroup: &RunCgroup,
    progress: &Mutex<Progress>,
    cancel: &AtomicBool,
    on_event: &mut dyn FnMut(Event<'_>) -> io::Result<()>,
) -> Outcome {
    let stopped: Mutex<Option<Stop>> = Mutex::new(None);
    let done = AtomicBool::new(false);
    let mut outcome = Outcome::default();
    thread::scope(|scope| {
        let watched = stream.try_clone();
        scope.spawn(|| {
            let Ok(watched) = watched else { return };
            while !done.load(Ordering::Relaxed) {
                let timed_out = spec.timeout.is_some_and(|limit| {
                    progress
                        .lock()
                        .expect("lock poisoned")
                        .started
                        .is_some_and(|at| at.elapsed() >= limit)
                });
                let stop = if cancel.load(Ordering::Relaxed) {
                    Some(Stop::Cancelled)
                } else if timed_out {
                    Some(Stop::TimedOut)
                } else {
                    None
                };
                if let Some(stop) = stop {
                    *stopped.lock().expect("lock poisoned") = Some(stop);
                    cgroup.kill();
                    let _ = watched.shutdown(Shutdown::Both);
                    return;
                }
                thread::sleep(WATCH_INTERVAL);
            }
        });
        let result = converse(spec, &mut stream, progress, on_event);
        done.store(true, Ordering::Relaxed);
        let ran_for = progress
            .lock()
            .expect("lock poisoned")
            .started
            .map(|at| at.elapsed());
        outcome.duration = ran_for.unwrap_or_default();
        match (*stopped.lock().expect("lock poisoned"), result) {
            (Some(Stop::Cancelled), _) => outcome.signal = Some(libc::SIGKILL),
            (Some(Stop::TimedOut), _) => outcome.timed_out = true,
            (None, Ok(Ended::Exited { code, signal })) => {
                outcome.exit_code = code;
                outcome.signal = signal;
            }
            (None, Ok(Ended::Failed(reason))) => outcome.error = Some(reason),
            (None, Err(e)) => outcome.error = Some(format!("{e:#}")),
        }
    });
    outcome
}

/// How the guest ended the conversation.
enum Ended {
    Exited {
        code: Option<i32>,
        signal: Option<i32>,
    },
    Failed(String),
}

fn converse(
    spec: &VmSpec,
    stream: &mut UnixStream,
    progress: &Mutex<Progress>,
    on_event: &mut dyn FnMut(Event<'_>) -> io::Result<()>,
) -> anyhow::Result<Ended> {
    let stamp = |name: &str| {
        let mut progress = progress.lock().expect("lock poisoned");
        progress
            .stamps
            .push((name.to_owned(), phases::monotonic_ns()));
    };
    let mut command = spec.command.clone();
    command.archive = spec.archive.is_some();
    command.network = spec.network > Level::None;
    Message::Run(command)
        .write_to(stream)
        .context("sending the command")?;
    if let Some(archive) = &spec.archive {
        send_archive(archive, stream).context("sending the workspace archive")?;
    }
    let mut failure = None;
    loop {
        let message = Message::read_from(stream).context("reading from the guest")?;
        match message {
            Some(Message::Started) => {
                progress.lock().expect("lock poisoned").started = Some(Instant::now());
                stamp("started");
                on_event(Event::Started)?;
            }
            Some(Message::Phases(phases)) => {
                progress.lock().expect("lock poisoned").guest_phases = phases;
            }
            Some(Message::Stdout(bytes)) => on_event(Event::Stdout(&bytes))?,
            Some(Message::Stderr(bytes)) => on_event(Event::Stderr(&bytes))?,
            Some(Message::Changes(bytes)) => on_event(Event::Changes(&bytes))?,
            Some(Message::Deleted(bytes)) => on_event(Event::Deleted(&bytes))?,
            Some(Message::Failed(reason)) => failure = Some(reason),
            Some(Message::Exited { code, signal }) => {
                stamp("exited");
                return Ok(match failure {
                    Some(reason) => Ended::Failed(reason),
                    None => Ended::Exited { code, signal },
                });
            }
            Some(other) => bail!("unexpected message from the guest: {other:?}"),
            None => bail!("the VM exited before the command reported"),
        }
    }
}

fn send_archive(path: &Path, stream: &mut UnixStream) -> io::Result<()> {
    let mut file = File::open(path)?;
    let mut buf = vec![0u8; DATA_CHUNK];
    loop {
        let n = file.read(&mut buf)?;
        if n == 0 {
            return Message::ArchiveEnd.write_to(stream);
        }
        Message::ArchiveData(buf[..n].to_vec()).write_to(stream)?;
    }
}

/// The descriptor number the launcher receives the phase pipe on.
const PHASES_FD: RawFd = 3;
/// The descriptor number the launcher receives its end of the guest's network on.
const NET_FD: RawFd = 4;
/// The descriptor number passt receives its end of the guest's network on.
const PASST_FD: RawFd = 3;

/// Makes `fd` available to `command`'s process as `target`.
fn pass_fd(command: &mut Command, fd: RawFd, target: RawFd) {
    // SAFETY: the hook only calls dup2(2) and fcntl(2), which are async-signal-safe, on a
    // descriptor that stays open until after the child has started. dup2 clears close-on-exec
    // on the copy; when the descriptor already has the target number, fcntl clears it instead.
    unsafe {
        command.pre_exec(move || {
            let ok = if fd == target {
                libc::fcntl(fd, libc::F_SETFD, 0) == 0
            } else {
                libc::dup2(fd, target) == target
            };
            if ok {
                Ok(())
            } else {
                Err(io::Error::last_os_error())
            }
        });
    }
}

/// Makes `command`'s process join `cgroup` before it runs any code of its own, so no work of its
/// escapes the limits.
/// Puts a child in the run's cgroup, and has it killed if the supervisor dies first.
fn join_cgroup<'cmd>(command: &'cmd mut Command, cgroup: &RunCgroup) -> &'cmd mut Command {
    let fd = cgroup.procs_fd();
    // SAFETY: getpid has no preconditions.
    let supervisor = unsafe { libc::getpid() };
    // SAFETY: the hook only calls write(2), prctl(2) and getppid(2), which are
    // async-signal-safe, and the descriptor stays open until after the child has started.
    unsafe {
        command.pre_exec(move || {
            if libc::write(fd, b"0".as_ptr().cast(), 1) != 1
                || libc::prctl(libc::PR_SET_PDEATHSIG, libc::SIGKILL, 0, 0, 0) != 0
            {
                return Err(io::Error::last_os_error());
            }
            // The supervisor died before the death signal was set.
            if libc::getppid() != supervisor {
                return Err(io::Error::from(ErrorKind::Interrupted));
            }
            Ok(())
        })
    }
}

/// Waits for the guest to connect, failing early if the launcher exits first or the run is
/// cancelled.
fn accept_guest(
    listener: &UnixListener,
    launcher: &mut Child,
    cancel: &AtomicBool,
) -> anyhow::Result<UnixStream> {
    listener.set_nonblocking(true)?;
    let deadline = Instant::now() + GUEST_CONNECT_TIMEOUT;
    loop {
        match listener.accept() {
            Ok((stream, _)) => {
                stream.set_nonblocking(false)?;
                return Ok(stream);
            }
            Err(e) if e.kind() == ErrorKind::WouldBlock => {}
            Err(e) => return Err(e).context("accepting the guest connection"),
        }
        if let Some(status) = launcher.try_wait()? {
            bail!("the VM exited before the guest connected ({status})");
        }
        if cancel.load(Ordering::Relaxed) {
            bail!("cancelled before the guest connected");
        }
        if Instant::now() >= deadline {
            bail!("the guest did not connect within {GUEST_CONNECT_TIMEOUT:?}");
        }
        thread::sleep(POLL_INTERVAL);
    }
}

fn wait_for_path(path: &Path, timeout: Duration) -> anyhow::Result<()> {
    let deadline = Instant::now() + timeout;
    while !path.exists() {
        if Instant::now() >= deadline {
            bail!("{} did not appear within {timeout:?}", path.display());
        }
        thread::sleep(POLL_INTERVAL);
    }
    Ok(())
}

fn wait_with_timeout(
    child: &mut Child,
    timeout: Duration,
) -> io::Result<Option<std::process::ExitStatus>> {
    let deadline = Instant::now() + timeout;
    loop {
        if let Some(status) = child.try_wait()? {
            return Ok(Some(status));
        }
        if Instant::now() >= deadline {
            return Ok(None);
        }
        thread::sleep(POLL_INTERVAL);
    }
}

fn console_tail(path: &Path) -> String {
    let Ok(console) = fs::read_to_string(path) else {
        return format!("no guest console at {}", path.display());
    };
    let lines: Vec<&str> = console.lines().collect();
    let tail = lines[lines.len().saturating_sub(CONSOLE_TAIL_LINES)..].join("\n");
    format!("guest console (last lines of {}):\n{tail}", path.display())
}

/// The files a guest boots from.
#[derive(Debug)]
struct GuestImage {
    kernel: PathBuf,
    initrd: PathBuf,
    rootfs: PathBuf,
}

impl GuestImage {
    fn open(dir: &Path) -> anyhow::Result<GuestImage> {
        let dir =
            fs::canonicalize(dir).with_context(|| format!("guest image {}", dir.display()))?;
        let image = GuestImage {
            kernel: dir.join("vmlinux"),
            initrd: dir.join("initramfs.cpio.gz"),
            rootfs: dir.join("rootfs"),
        };
        for path in [&image.kernel, &image.initrd, &image.rootfs] {
            if !path.exists() {
                bail!("guest image is missing {}", path.display());
            }
        }
        Ok(image)
    }
}

/// A private directory for one run's sockets, jail and logs, removed on drop, and the user and
/// group ID the run's VMM becomes.
#[derive(Debug)]
struct RunDir {
    path: PathBuf,
    id: u32,
}

impl RunDir {
    fn create() -> anyhow::Result<RunDir> {
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map_or(0, |d| d.subsec_nanos());
        let id = RUN_ID_BASE + ((nanos ^ std::process::id().rotate_left(16)) & RUN_ID_MASK);
        let path = RunDir::path_for(&id.to_string());
        fs::DirBuilder::new()
            .mode(0o700)
            .create(&path)
            .with_context(|| format!("creating {}", path.display()))?;
        Ok(RunDir { path, id })
    }

    fn path_for(id: &str) -> PathBuf {
        std::env::temp_dir().join(format!("peerspan-runtime-{id}"))
    }
}

fn chown(path: &Path, id: u32) -> anyhow::Result<()> {
    std::os::unix::fs::chown(path, Some(id), Some(id))
        .with_context(|| format!("giving {} to the run's user", path.display()))
}

/// Tears down runs whose supervisor died without doing it: their processes, network filters,
/// cgroups and run directories. A run that cannot be cleaned up is reported, not fatal.
fn remove_stale_runs(cgroup_root: &Path) {
    let stale = match cgroup::stale(cgroup_root) {
        Ok(stale) if stale.is_empty() => return,
        Ok(stale) => stale,
        Err(e) => {
            eprintln!("peerspan-runtime: looking for stale runs: {e:#}");
            return;
        }
    };
    for cgroup in &stale {
        cgroup.kill();
    }
    let paths: Vec<&Path> = stale.iter().map(RunCgroup::path).collect();
    if let Err(e) = net::remove_stale(&paths) {
        eprintln!("peerspan-runtime: {e:#}");
    }
    for cgroup in stale {
        let name = cgroup
            .path()
            .file_name()
            .map(|n| n.to_string_lossy().into_owned());
        drop(cgroup);
        if let Some(id) = name.as_deref().and_then(|n| n.strip_prefix("run-")) {
            let _ = fs::remove_dir_all(RunDir::path_for(id));
        }
        eprintln!(
            "peerspan-runtime: removed {}, left by a supervisor that died",
            name.unwrap_or_default()
        );
    }
}

impl Drop for RunDir {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.path);
    }
}

/// Kills and reaps a child process when dropped.
#[derive(Debug)]
struct ChildGuard(Child);

impl Drop for ChildGuard {
    fn drop(&mut self) {
        if matches!(self.0.try_wait(), Ok(None)) {
            let _ = self.0.kill();
        }
        let _ = self.0.wait();
    }
}
