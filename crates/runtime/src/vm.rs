// SPDX-License-Identifier: Apache-2.0
// Copyright The Peerspan Authors

//! Runs one command in one microVM: serves the workspace, starts the launcher, hands the
//! command and the workspace archive to the guest, and reports what happens as [`Event`]s.

use std::fs::{self, File};
use std::io::{self, ErrorKind, Read};
use std::net::Shutdown;
use std::os::unix::fs::DirBuilderExt;
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::Mutex;
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use anyhow::{Context, bail};
use peerspan_runtime_protocol::{self as protocol, DATA_CHUNK, Message, WORKSPACE_TAG};

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
    pub cpus: u8,
    pub memory_mib: u32,
    /// How long the command may run once started.
    pub timeout: Option<Duration>,
    pub console_log: Option<PathBuf>,
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
    let run_dir = RunDir::create()?;
    let id = run_dir.id;
    let sockets = run_dir.path.join("sockets");
    fs::DirBuilder::new().mode(0o700).create(&sockets)?;
    chown(&sockets, id)?;
    let jail = run_dir.path.join("jail");
    fs::DirBuilder::new().mode(0o700).create(&jail)?;
    let console_path = options
        .console_log
        .clone()
        .unwrap_or_else(|| run_dir.path.join("console.log"));

    let workspace_socket = sockets.join(WORKSPACE_SOCKET);
    let _server = ChildGuard(
        Command::new(&options.virtiofsd)
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

    let agent_socket = sockets.join(AGENT_SOCKET);
    let listener = UnixListener::bind(&agent_socket)
        .with_context(|| format!("binding {}", agent_socket.display()))?;
    chown(&agent_socket, id)?;

    let console = File::create(&console_path)
        .with_context(|| format!("creating {}", console_path.display()))?;
    let mut launcher = ChildGuard(
        Command::new(std::env::current_exe().context("locating this executable")?)
            .arg("launch")
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
            .stdin(Stdio::null())
            .stdout(console.try_clone()?)
            .stderr(console)
            .spawn()
            .context("starting the launcher")?,
    );

    let stream = match accept_guest(&listener, &mut launcher.0, cancel) {
        Ok(stream) => stream,
        Err(e) => return Err(e.context(console_tail(&console_path))),
    };
    let outcome = serve_guest(spec, stream, launcher.0.id(), cancel, on_event);
    if wait_with_timeout(&mut launcher.0, SHUTDOWN_TIMEOUT)?.is_none() {
        eprintln!("peerspan-runtime: the VM did not shut down; stopping it");
    }
    Ok(outcome)
}

/// Hands the guest its command and archive, and relays its messages until it reports an exit,
/// the watchdog stops it, or the connection ends.
fn serve_guest(
    spec: &VmSpec,
    mut stream: UnixStream,
    launcher_pid: u32,
    cancel: &AtomicBool,
    on_event: &mut dyn FnMut(Event<'_>) -> io::Result<()>,
) -> Outcome {
    let started: Mutex<Option<Instant>> = Mutex::new(None);
    let stopped: Mutex<Option<Stop>> = Mutex::new(None);
    let done = AtomicBool::new(false);
    let mut outcome = Outcome::default();
    thread::scope(|scope| {
        let watched = stream.try_clone();
        scope.spawn(|| {
            let Ok(watched) = watched else { return };
            while !done.load(Ordering::Relaxed) {
                let timed_out = spec.timeout.is_some_and(|limit| {
                    started
                        .lock()
                        .expect("lock poisoned")
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
                    // The jailed VMM dies with the launcher.
                    // SAFETY: kill takes only integers; the pid is our live child.
                    unsafe { libc::kill(i32::try_from(launcher_pid).unwrap_or(0), libc::SIGKILL) };
                    let _ = watched.shutdown(Shutdown::Both);
                    return;
                }
                thread::sleep(WATCH_INTERVAL);
            }
        });
        let result = converse(spec, &mut stream, &started, on_event);
        done.store(true, Ordering::Relaxed);
        let ran_for = started
            .lock()
            .expect("lock poisoned")
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
    started: &Mutex<Option<Instant>>,
    on_event: &mut dyn FnMut(Event<'_>) -> io::Result<()>,
) -> anyhow::Result<Ended> {
    let mut command = spec.command.clone();
    command.archive = spec.archive.is_some();
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
                *started.lock().expect("lock poisoned") = Some(Instant::now());
                on_event(Event::Started)?;
            }
            Some(Message::Stdout(bytes)) => on_event(Event::Stdout(&bytes))?,
            Some(Message::Stderr(bytes)) => on_event(Event::Stderr(&bytes))?,
            Some(Message::Changes(bytes)) => on_event(Event::Changes(&bytes))?,
            Some(Message::Deleted(bytes)) => on_event(Event::Deleted(&bytes))?,
            Some(Message::Failed(reason)) => failure = Some(reason),
            Some(Message::Exited { code, signal }) => {
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
        let path = std::env::temp_dir().join(format!("peerspan-runtime-{id}"));
        fs::DirBuilder::new()
            .mode(0o700)
            .create(&path)
            .with_context(|| format!("creating {}", path.display()))?;
        Ok(RunDir { path, id })
    }
}

fn chown(path: &Path, id: u32) -> anyhow::Result<()> {
    std::os::unix::fs::chown(path, Some(id), Some(id))
        .with_context(|| format!("giving {} to the run's user", path.display()))
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
