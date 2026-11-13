// SPDX-License-Identifier: Apache-2.0
// Copyright The Peerspan Authors

//! Runs one command in one microVM: serves the workspace, starts the launcher, hands the
//! command and the workspace archive to the guest, and reports what happens as [`Event`]s.

use std::ffi::OsStr;
use std::fs::{self, File};
use std::io::{self, ErrorKind, Read};
use std::net::Shutdown;
use std::os::fd::{AsRawFd, BorrowedFd, RawFd};
use std::os::unix::fs::{DirBuilderExt, MetadataExt};
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

use rustix::process::{Resource, Rlimit, Signal, setrlimit};

use crate::caplog::CappedLog;
use crate::cgroup::{self, IoRate, Limits, RunCgroup};
use crate::net::{self, Level, RunFilter};
use crate::phases;
use crate::userns::UserNamespace;

use crate::launcher::{AGENT_SOCKET, WORKSPACE_SOCKET};

/// How long a workspace server that may have failed has to exit, when the guest did not connect.
const SERVER_EXIT_GRACE: Duration = Duration::from_millis(500);
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
/// The most of the guest console kept on the host; the guest can write to it at will.
const CONSOLE_LIMIT: u64 = 16 << 20;
/// The most of the workspace server's and passt's logs kept, which a guest can make grow by
/// provoking their errors.
const SERVER_LOG_LIMIT: u64 = 1 << 20;

/// How often the watchdog checks for cancellation and the timeout.
const WATCH_INTERVAL: Duration = Duration::from_millis(50);

/// One command to run in one VM.
#[derive(Debug)]
pub struct VmSpec {
    /// What the guest gets as its workspace.
    pub workspace: Workspace,
    /// Give a workspace directory to the run's own user first. The guest's root acts on the
    /// host as the directory's owner, so this keeps a workspace the runtime created for the run
    /// from being served as root.
    pub own_workspace: bool,
    /// A tar archive, optionally gzip-compressed, unpacked into the workspace inside the VM.
    pub archive: Option<PathBuf>,
    /// The command's standard input, streamed as it is read; without it the command reads an
    /// empty one.
    pub stdin: Option<File>,
    pub guest: PathBuf,
    /// A root filesystem to boot instead of the guest image's own, such as a pulled image's.
    pub rootfs: Option<PathBuf>,
    /// A host directory the guest mounts read-only at the protocol's harness path, such as an
    /// agent tool's binary. The VM's own user must be able to read it.
    pub harness: Option<PathBuf>,
    pub libkrun: PathBuf,
    pub virtiofsd: PathBuf,
    /// The passt binary that carries the guest's network traffic.
    pub passt: PathBuf,
    /// What the guest may reach.
    pub network: Level,
    pub cpus: u8,
    pub memory_mib: u32,
    /// The cap on disk I/O, on the disks holding the workspace and the guest image.
    pub io: IoRate,
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

/// Where the guest's workspace comes from.
#[derive(Debug)]
pub enum Workspace {
    /// A host directory, served by `virtiofsd` confined to it. With an archive it must be
    /// empty.
    Directory(PathBuf),
    /// A workspace of a peerfs space, served by a confined peerfs process of the run's own as
    /// a vhost-user-fs device, so the guest's changes land in the workspace's history.
    Peerfs(PeerfsWorkspace),
}

/// A peerfs workspace, and how to reach the server of its space.
#[derive(Debug)]
pub struct PeerfsWorkspace {
    /// The `peerfs` binary.
    pub peerfs: PathBuf,
    /// The space's directory.
    pub space: PathBuf,
    /// The workspace's name.
    pub name: String,
    /// A branch to fork a new workspace from: the run creates a branch named `name` at this
    /// branch's head, and a workspace `name` following it. Refused if either exists already, so
    /// a fork never runs on someone else's work.
    pub fork_from: Option<String>,
    /// The agent recorded on the run's snapshots and commit.
    pub agent: String,
    /// With a message, commit the workspace to its branch once the run has been snapshotted.
    pub commit_message: Option<String>,
    /// Discard the workspace once it is committed, or had nothing to commit, keeping its branch.
    pub discard: bool,
}

/// The agent recorded on a peerfs workspace's snapshots when the caller names none.
pub const DEFAULT_PEERFS_AGENT: &str = "peerspan-runtime";

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
    /// For a peerfs workspace, the snapshot taken after the command, if it changed anything.
    pub snapshot: Option<String>,
    /// For a peerfs workspace committed after the run, the commit, if there was anything to
    /// commit.
    pub commit: Option<String>,
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
    let workspace_path = match &options.workspace {
        Workspace::Directory(dir) => {
            if let Some(archive) = &options.archive
                && fs::read_dir(dir)?.next().is_some()
            {
                bail!(
                    "the workspace must be empty to unpack {} into it",
                    archive.display()
                );
            }
            dir
        }
        Workspace::Peerfs(peerfs) => {
            if options.archive.is_some() {
                bail!(
                    "an archive is unpacked only into a workspace directory, not a peerfs workspace"
                );
            }
            &peerfs.space
        }
    };
    // The workspace directory, or the peerfs space holding the workspace.
    let workspace = fs::canonicalize(workspace_path)
        .with_context(|| format!("workspace {}", workspace_path.display()))?;
    let mut guest = GuestImage::open(&options.guest)?;
    if let Some(rootfs) = &options.rootfs {
        guest.rootfs = fs::canonicalize(rootfs)
            .with_context(|| format!("root filesystem {}", rootfs.display()))?;
    }
    if !rustix::process::geteuid().is_root() {
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
        &Limits {
            cpus: options.cpus,
            guest_memory_mib: options.memory_mib,
            io: options.io,
            disks: io_disks(&workspace, &guest),
        },
    )?;
    mark("cgroup");
    let console_path = options
        .console_log
        .clone()
        .unwrap_or_else(|| run_dir.path.join("console.log"));

    let workspace_socket = sockets.join(WORKSPACE_SOCKET);
    let mut server = match &options.workspace {
        Workspace::Directory(_) => {
            if options.own_workspace {
                chown(&workspace, id)?;
            }
            start_virtiofsd(options, &workspace, &workspace_socket, &cgroup, &run_dir)?
        }
        Workspace::Peerfs(peerfs) => start_peerfs(peerfs, &workspace_socket, &cgroup, &run_dir)?,
    };
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
        let (passt_log, passt_log_writer) =
            CappedLog::create(&run_dir.path.join("passt.log"), SERVER_LOG_LIMIT)?;
        let passt = ChildGuard(
            passt_command
                .arg("--fd")
                .arg(PASST_FD.to_string())
                .args(net::passt_args())
                .stdin(Stdio::null())
                .stdout(Stdio::null())
                .stderr(passt_log_writer)
                .spawn()
                .with_context(|| format!("starting {}", options.passt.display()))?,
        );
        drop(passt_end);
        network = Some((guest_end, (passt, passt_log), filter));
        mark("network");
    }

    let agent_socket = sockets.join(AGENT_SOCKET);
    let listener = UnixListener::bind(&agent_socket)
        .with_context(|| format!("binding {}", agent_socket.display()))?;
    chown(&agent_socket, id)?;

    let (console, console_writer) = CappedLog::create(&console_path, CONSOLE_LIMIT)
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
            .args(
                options
                    .harness
                    .iter()
                    .flat_map(|dir| [OsStr::new("--harness"), dir.as_os_str()]),
            )
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
            .stdout(console_writer.try_clone()?)
            .stderr(console_writer)
            .spawn()
            .context("starting the launcher")?,
    );
    drop(phase_writer);
    let _network = network.map(|(_guest_end, passt, filter)| (passt, filter));

    let stream = match accept_guest(&listener, &mut launcher.0, cancel) {
        Ok(stream) => stream,
        Err(e) => {
            let e = e.context(console.tail("guest console"));
            // A server that could not start is the likelier cause; its log goes with the run.
            if let WorkspaceServer::Virtiofsd { child, log, owner } = &mut server
                && let Ok(Some(_)) = wait_with_timeout(&mut child.0, SERVER_EXIT_GRACE)
            {
                return Err(e.context(format!(
                    "the workspace server, which runs as the workspace's owner (user {}, group \
                     {}) and must be able to reach {}, failed; {}",
                    owner.uid(),
                    owner.gid(),
                    workspace.display(),
                    log.tail("its log")
                )));
            }
            if let WorkspaceServer::Peerfs(peerfs) = &mut server
                && let Ok(Some(_)) = wait_with_timeout(&mut peerfs.child, SERVER_EXIT_GRACE)
            {
                return Err(e.context(format!(
                    "the peerfs workspace server failed; {}",
                    peerfs.log.tail("its log")
                )));
            }
            return Err(e);
        }
    };
    let connect = launched.elapsed();
    mark("connected");
    let progress = Mutex::new(Progress::default());
    let mut outcome = serve_guest(spec, stream, &cgroup, &progress, cancel, on_event);
    if cgroup.oom_killed() && outcome.exit_code.is_none() && !outcome.timed_out {
        outcome.error = Some("the VM exceeded its memory limit and was killed".into());
    }
    let status = wait_with_timeout(&mut launcher.0, SHUTDOWN_TIMEOUT)?;
    if status.is_some_and(killed_by_seccomp) && outcome.exit_code.is_none() {
        outcome.error = Some(SECCOMP_KILLED.into());
    }
    let shut_down = status.is_some();
    if !shut_down {
        crate::log::warn("the VM did not shut down; stopping it", &[]);
    }
    mark("vm_exited");
    let progress = progress.into_inner().expect("lock poisoned");
    if let Workspace::Peerfs(peerfs) = &options.workspace {
        // Waits for the workspace to be given back.
        drop(server);
        if progress.started.is_some() {
            match snapshot(peerfs, &after_message(&outcome)) {
                Ok(id) => outcome.snapshot = id,
                Err(e) => {
                    outcome.error.get_or_insert_with(|| format!("{e:#}"));
                }
            }
            mark("snapshot");
            // A workspace whose last snapshot failed is not committed: the commit would hold
            // changes nobody snapshotted.
            if let Some(message) = &peerfs.commit_message
                && outcome.error.is_none()
            {
                match commit(peerfs, message) {
                    Ok(id) => outcome.commit = id,
                    Err(e) => outcome.error = Some(format!("{e:#}")),
                }
                mark("commit");
                // Only after a commit: a failed one leaves the work in the workspace for the
                // owner. The work is committed, so a discard that fails costs disk, not results.
                if peerfs.discard && outcome.error.is_none() {
                    if let Err(e) = discard(peerfs) {
                        crate::log::warn(
                            "could not discard the committed peerfs workspace",
                            &[("workspace", &peerfs.name), ("error", &format!("{e:#}"))],
                        );
                    }
                    mark("discard");
                }
            }
        }
    }
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
    command.harness = spec.harness.is_some();
    command.stdin = spec.stdin.is_some();
    Message::Run(command)
        .write_to(stream)
        .context("sending the command")?;
    if let Some(archive) = &spec.archive {
        send_archive(archive, stream).context("sending the workspace archive")?;
    }
    if let Some(input) = &spec.stdin {
        feed_stdin(input.try_clone()?, stream.try_clone()?);
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

/// Streams `input` to the guest as the command's standard input, then its end. The thread is
/// left behind if the input never ends, as a terminal's may not; it stops at the next read
/// once the guest has gone.
fn feed_stdin(mut input: File, mut stream: UnixStream) {
    thread::spawn(move || {
        let mut buf = vec![0u8; DATA_CHUNK];
        loop {
            let n = match input.read(&mut buf) {
                Ok(0) => break,
                Ok(n) => n,
                Err(e) if e.kind() == ErrorKind::Interrupted => continue,
                Err(_) => break,
            };
            if Message::Stdin(buf[..n].to_vec())
                .write_to(&mut stream)
                .is_err()
            {
                return;
            }
        }
        let _ = Message::StdinEnd.write_to(&mut stream);
    });
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
/// The descriptor number the workspace server receives its listening socket on.
const SERVER_FD: RawFd = 3;

/// The open files the workspace server may hold, one per file the guest has looked up: what
/// virtiofsd sets for itself when it runs as root.
fn open_files_limit() -> u64 {
    const VIRTIOFSD_DEFAULT: u64 = 1_000_000;
    fs::read_to_string("/proc/sys/fs/nr_open")
        .ok()
        .and_then(|n| n.trim().parse().ok())
        .map_or(VIRTIOFSD_DEFAULT, |max: u64| max.min(VIRTIOFSD_DEFAULT))
}

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
    let supervisor = rustix::process::getpid();
    // SAFETY: the hook makes only system calls, which are async-signal-safe, and the
    // descriptor stays open until after the child has started.
    unsafe {
        command.pre_exec(move || {
            if rustix::io::write(BorrowedFd::borrow_raw(fd), b"0")? != 1 {
                return Err(io::Error::from(ErrorKind::WriteZero));
            }
            rustix::process::set_parent_process_death_signal(Some(Signal::KILL))?;
            // The supervisor died before the death signal was set.
            if rustix::process::getppid() != Some(supervisor) {
                return Err(io::Error::from(ErrorKind::Interrupted));
            }
            Ok(())
        })
    }
}

const SECCOMP_KILLED: &str = "the VMM made a system call its seccomp filter does not allow \
    and was killed; the kernel log names the call";

/// Whether the launcher reports that its VMM was killed by its seccomp filter: the launcher
/// exits with 128 plus the signal that ended the VMM, and seccomp kills with SIGSYS.
fn killed_by_seccomp(status: std::process::ExitStatus) -> bool {
    status.code() == Some(128 + libc::SIGSYS)
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
            if killed_by_seccomp(status) {
                bail!("{SECCOMP_KILLED}");
            }
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

/// The disks the run reads and writes: the workspace's and the guest image's.
fn io_disks(workspace: &Path, guest: &GuestImage) -> Vec<(u32, u32)> {
    let mut disks: Vec<(u32, u32)> = [workspace, &guest.rootfs]
        .into_iter()
        .filter_map(cgroup::disk_of)
        .collect();
    disks.sort_unstable();
    disks.dedup();
    disks
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
pub fn remove_stale_runs(cgroup_root: &Path) {
    let stale = match cgroup::stale(cgroup_root) {
        Ok(stale) if stale.is_empty() => return,
        Ok(stale) => stale,
        Err(e) => {
            crate::log::warn(
                "could not look for stale runs",
                &[("error", &format!("{e:#}"))],
            );
            return;
        }
    };
    for cgroup in &stale {
        cgroup.kill();
    }
    let paths: Vec<&Path> = stale.iter().map(RunCgroup::path).collect();
    if let Err(e) = net::remove_stale(&paths) {
        crate::log::warn(
            "could not remove stale network rules",
            &[("error", &format!("{e:#}"))],
        );
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
        crate::log::info(
            "removed a run left by a supervisor that died",
            &[("cgroup", &name.unwrap_or_default())],
        );
    }
}

impl Drop for RunDir {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.path);
    }
}

/// Kills and reaps a child process when dropped.
/// What serves the guest's workspace, stopped when dropped.
enum WorkspaceServer {
    /// `virtiofsd` for a workspace directory, as the directory's owner, with its log and that
    /// owner for reporting a failure to start.
    Virtiofsd {
        child: ChildGuard,
        log: CappedLog,
        owner: fs::Metadata,
    },
    /// A confined peerfs process serving a peerfs workspace.
    Peerfs(PeerfsServer),
}

/// Starts `virtiofsd` on `workspace`, listening at `socket`. It is root in a user namespace
/// where root is the workspace's owner, so the guest's root acts on the host as that user and
/// no other. It cannot reach the run's socket directory, so it gets a socket created for it.
fn start_virtiofsd(
    options: &VmSpec,
    workspace: &Path,
    socket: &Path,
    cgroup: &RunCgroup,
    run_dir: &RunDir,
) -> anyhow::Result<WorkspaceServer> {
    let owner = fs::metadata(workspace)?;
    let server_namespace = UserNamespace::new(owner.uid(), owner.gid())?;
    let server_listener =
        UnixListener::bind(socket).with_context(|| format!("binding {}", socket.display()))?;
    let (log, server_log_writer) =
        CappedLog::create(&run_dir.path.join("virtiofsd.log"), SERVER_LOG_LIMIT)?;
    let mut server_command = Command::new(&options.virtiofsd);
    join_cgroup(&mut server_command, cgroup);
    // Raised here, while still root: inside the namespace the server could not raise it.
    let open_files = open_files_limit();
    // SAFETY: the hook only makes a system call, which is async-signal-safe.
    unsafe {
        server_command.pre_exec(move || {
            let limit = Rlimit {
                current: Some(open_files),
                maximum: Some(open_files),
            };
            Ok(setrlimit(Resource::Nofile, limit)?)
        });
    }
    server_namespace.enter(&mut server_command);
    pass_fd(&mut server_command, server_listener.as_raw_fd(), SERVER_FD);
    let child = ChildGuard(
        server_command
            .arg(format!("--fd={SERVER_FD}"))
            .arg(format!("--shared-dir={}", workspace.display()))
            .arg(format!("--tag={WORKSPACE_TAG}"))
            .arg("--cache=auto")
            .arg("--rlimit-nofile=0")
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(server_log_writer)
            .spawn()
            .with_context(|| format!("starting {}", options.virtiofsd.display()))?,
    );
    Ok(WorkspaceServer::Virtiofsd { child, log, owner })
}

/// How long a peerfs workspace server has to give the workspace back once the VM is gone, and
/// to start serving, which may include starting the space's server.
const PEERFS_EXIT_WAIT: Duration = Duration::from_secs(5);
const PEERFS_START_WAIT: Duration = Duration::from_secs(15);

/// Starts `peerfs ws vhost-user` for `workspace` at `socket`: a process of the run's own, in
/// its cgroup, which borrows the workspace from the space's server and confines itself to it
/// before the guest is served. The space's server is started first if none runs, outside the
/// run's cgroup, since it outlives the run.
fn start_peerfs(
    workspace: &PeerfsWorkspace,
    socket: &Path,
    cgroup: &RunCgroup,
    run_dir: &RunDir,
) -> anyhow::Result<WorkspaceServer> {
    let output = peerfs_command(&workspace.peerfs, &workspace.space)
        .args(["ws", "list"])
        .output()
        .with_context(|| format!("starting {}", workspace.peerfs.display()))?;
    if !output.status.success() {
        bail!(
            "peerfs could not reach space {}: {}",
            workspace.space.display(),
            String::from_utf8_lossy(&output.stderr).trim()
        );
    }
    if let Some(from) = &workspace.fork_from {
        fork(workspace, from, &String::from_utf8_lossy(&output.stdout))?;
    }
    // So the snapshot after the command holds only what the run changed.
    snapshot(workspace, "Before a peerspan-runtime run")?;
    let (log, log_writer) = CappedLog::create(&run_dir.path.join("peerfs.log"), SERVER_LOG_LIMIT)?;
    let mut command = peerfs_command(&workspace.peerfs, &workspace.space);
    join_cgroup(&mut command, cgroup);
    let mut child = command
        .args(["ws", "vhost-user", &workspace.name])
        .arg(socket)
        .args(["--tag", WORKSPACE_TAG])
        .stdout(Stdio::null())
        .stderr(log_writer)
        .spawn()
        .with_context(|| format!("starting {}", workspace.peerfs.display()))?;
    // Ready once it has borrowed the workspace and bound the socket.
    let deadline = Instant::now() + PEERFS_START_WAIT;
    while !socket.exists() {
        if child.try_wait()?.is_some() {
            bail!(
                "peerfs could not serve workspace {}; {}",
                workspace.name,
                log.tail("its log")
            );
        }
        if Instant::now() >= deadline {
            let _ = child.kill();
            let _ = child.wait();
            bail!(
                "peerfs did not start serving workspace {} within {}s; {}",
                workspace.name,
                PEERFS_START_WAIT.as_secs(),
                log.tail("its log")
            );
        }
        thread::sleep(POLL_INTERVAL);
    }
    let server = PeerfsServer {
        child,
        log,
        name: workspace.name.clone(),
    };
    Ok(WorkspaceServer::Peerfs(server))
}

/// Creates `workspace` as a fork of the branch `from`: a branch of the workspace's name at
/// `from`'s head, then a workspace following it. `list` is `peerfs ws list`'s output, one
/// workspace per line, name first; a workspace of that name already there is refused before the
/// branch is made.
fn fork(workspace: &PeerfsWorkspace, from: &str, list: &str) -> anyhow::Result<()> {
    let name = &workspace.name;
    if list
        .lines()
        .any(|line| line.split_whitespace().next() == Some(name))
    {
        bail!("peerfs workspace {name} exists already, so it cannot be forked from {from}");
    }
    for args in [
        &["fork", name, "--from", from][..],
        &["ws", "create", name, "--branch", name],
    ] {
        let output = peerfs_command(&workspace.peerfs, &workspace.space)
            .args(args)
            .args(["--agent", &workspace.agent])
            .output()
            .with_context(|| format!("starting {}", workspace.peerfs.display()))?;
        if !output.status.success() {
            bail!(
                "peerfs could not fork workspace {name} from {from}: {}",
                String::from_utf8_lossy(&output.stderr).trim()
            );
        }
    }
    Ok(())
}

/// Commits `workspace` to its branch with `message`, returning the commit, or `None` when
/// there was nothing to commit.
fn commit(workspace: &PeerfsWorkspace, message: &str) -> anyhow::Result<Option<String>> {
    let output = peerfs_command(&workspace.peerfs, &workspace.space)
        .args(["ws", "commit", &workspace.name, "-m", message])
        .args(["--agent", &workspace.agent])
        .output()
        .with_context(|| format!("starting {}", workspace.peerfs.display()))?;
    if !output.status.success() {
        bail!(
            "peerfs could not commit workspace {}: {}",
            workspace.name,
            String::from_utf8_lossy(&output.stderr).trim()
        );
    }
    let id = String::from_utf8_lossy(&output.stdout).trim().to_owned();
    // `nothing to commit` when the workspace has no changes.
    Ok(Some(id).filter(|id| id.len() == 66 && id.bytes().all(|b| b.is_ascii_hexdigit())))
}

/// Discards `workspace`: what it held beyond its branch, such as build output, goes, and the
/// branch stays.
fn discard(workspace: &PeerfsWorkspace) -> anyhow::Result<()> {
    let output = peerfs_command(&workspace.peerfs, &workspace.space)
        .args(["ws", "discard", &workspace.name])
        .output()
        .with_context(|| format!("starting {}", workspace.peerfs.display()))?;
    if !output.status.success() {
        bail!(
            "peerfs could not discard workspace {}: {}",
            workspace.name,
            String::from_utf8_lossy(&output.stderr).trim()
        );
    }
    Ok(())
}

/// Reads each secret's file, refusing more secrets, or larger ones, than the guest accepts.
pub fn read_secrets(
    secrets: &[(String, PathBuf)],
) -> anyhow::Result<Vec<peerspan_runtime_protocol::Secret>> {
    use peerspan_runtime_protocol::{MAX_SECRET_BYTES, MAX_SECRETS, Secret};
    if secrets.len() > MAX_SECRETS {
        bail!("at most {MAX_SECRETS} secrets");
    }
    let mut names = std::collections::HashSet::new();
    secrets
        .iter()
        .map(|(name, file)| {
            if !names.insert(name) {
                bail!("secret {name} given twice");
            }
            let value = fs::read(file)
                .with_context(|| format!("reading secret {name} from {}", file.display()))?;
            if value.len() > MAX_SECRET_BYTES {
                bail!("secret {name} is over {MAX_SECRET_BYTES} bytes");
            }
            Ok(Secret {
                name: name.clone(),
                value,
            })
        })
        .collect()
}

/// Snapshots `workspace` with `message`, returning the snapshot, or `None` when nothing changed
/// since its last. Retried for a moment: a workspace server that was killed rather than exiting
/// is given back only once the space's server sees its connection close.
fn snapshot(workspace: &PeerfsWorkspace, message: &str) -> anyhow::Result<Option<String>> {
    let deadline = Instant::now() + PEERFS_EXIT_WAIT;
    loop {
        let output = peerfs_command(&workspace.peerfs, &workspace.space)
            .args(["ws", "snapshot", &workspace.name, "-m", message])
            .args(["--agent", &workspace.agent])
            .output()
            .with_context(|| format!("starting {}", workspace.peerfs.display()))?;
        if output.status.success() {
            let id = String::from_utf8_lossy(&output.stdout).trim().to_owned();
            return Ok(Some(id).filter(|id| !id.is_empty()));
        }
        if Instant::now() >= deadline {
            bail!(
                "peerfs could not snapshot workspace {}: {}",
                workspace.name,
                String::from_utf8_lossy(&output.stderr).trim()
            );
        }
        thread::sleep(POLL_INTERVAL);
    }
}

/// The message of the snapshot after a run: how the command ended.
fn after_message(outcome: &Outcome) -> String {
    let ended = match (outcome.exit_code, outcome.signal) {
        _ if outcome.timed_out => "timed out".to_owned(),
        (Some(code), _) => format!("exit {code}"),
        (None, Some(signal)) => format!("signal {signal}"),
        (None, None) => "failed".to_owned(),
    };
    format!("After a peerspan-runtime run: {ended}")
}

/// `peerfs` run for the space at `space`.
fn peerfs_command(peerfs: &Path, space: &Path) -> Command {
    let mut command = Command::new(peerfs);
    command.arg("--space").arg(space).stdin(Stdio::null());
    command
}

/// A run's peerfs workspace server. It exits by itself once the VMM hangs up, giving the
/// workspace back to the space's server; dropping waits for that, and kills it after
/// [`PEERFS_EXIT_WAIT`].
struct PeerfsServer {
    child: Child,
    log: CappedLog,
    name: String,
}

impl Drop for PeerfsServer {
    fn drop(&mut self) {
        let status = match wait_with_timeout(&mut self.child, PEERFS_EXIT_WAIT) {
            Ok(Some(status)) => Some(status),
            _ => {
                let _ = self.child.kill();
                let _ = self.child.wait();
                None
            }
        };
        if !status.is_some_and(|status| status.success()) {
            let ended = status.map_or_else(|| "killed".to_owned(), |status| status.to_string());
            crate::log::warn(
                "the peerfs workspace server did not exit cleanly",
                &[
                    ("workspace", &self.name),
                    ("status", &ended),
                    ("log", &self.log.tail("its log")),
                ],
            );
        }
    }
}

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
