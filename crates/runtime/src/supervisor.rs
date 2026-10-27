// SPDX-License-Identifier: Apache-2.0
// Copyright The Peerspan Authors

//! Runs one command in one microVM: serves the workspace, starts the launcher, hands the
//! command to the guest and relays its output.

use std::fs::{self, File};
use std::io::{self, ErrorKind, Write};
use std::os::unix::fs::DirBuilderExt;
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::thread;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use anyhow::{Context, bail};
use peerspan_runtime_protocol::{self as protocol, Message, WORKSPACE_TAG};

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

/// One command to run in one VM.
#[derive(Debug)]
pub struct RunOptions {
    pub workspace: PathBuf,
    pub guest: PathBuf,
    pub libkrun: PathBuf,
    pub virtiofsd: PathBuf,
    pub cpus: u8,
    pub memory_mib: u32,
    pub console_log: Option<PathBuf>,
    pub command: protocol::Command,
}

/// Runs the command and returns its exit code.
///
/// # Errors
///
/// Fails if the VM cannot be started or the guest does not report an exit code. The command
/// failing is not an error; its code is returned.
pub fn run(options: RunOptions) -> anyhow::Result<i32> {
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

    let outcome = serve_guest(&listener, &mut launcher.0, &options.command);
    let code = match outcome {
        Ok(code) => code,
        Err(e) => return Err(e.context(console_tail(&console_path))),
    };
    if wait_with_timeout(&mut launcher.0, SHUTDOWN_TIMEOUT)?.is_none() {
        eprintln!("peerspan-runtime: the VM did not shut down; stopping it");
    }
    Ok(code)
}

fn serve_guest(
    listener: &UnixListener,
    launcher: &mut Child,
    command: &protocol::Command,
) -> anyhow::Result<i32> {
    let mut stream = accept_guest(listener, launcher)?;
    Message::Run(command.clone())
        .write_to(&mut stream)
        .context("sending the command")?;
    let mut stdout = io::stdout().lock();
    let mut stderr = io::stderr().lock();
    loop {
        match Message::read_from(&mut stream).context("reading from the guest")? {
            Some(Message::Stdout(bytes)) => {
                stdout.write_all(&bytes)?;
                stdout.flush()?;
            }
            Some(Message::Stderr(bytes)) => {
                stderr.write_all(&bytes)?;
                stderr.flush()?;
            }
            Some(Message::Exit(code)) => return Ok(code),
            Some(other) => bail!("unexpected message from the guest: {other:?}"),
            None => bail!("the guest closed the connection without an exit code"),
        }
    }
}

/// Waits for the guest to connect, failing early if the launcher exits first.
fn accept_guest(listener: &UnixListener, launcher: &mut Child) -> anyhow::Result<UnixStream> {
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
