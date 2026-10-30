// SPDX-License-Identifier: Apache-2.0
// Copyright The Peerspan Authors

//! PID 1 inside a runtime microVM.
//!
//! It loads the modules the initramfs lists, mounts the root filesystem and the workspace,
//! switches into the root filesystem, connects to the supervisor over vsock, runs the one
//! command it is sent while streaming its output back, and reboots, which ends the VM.
//!
//! It has its own C `main` rather than Rust's: at startup, Rust's runtime replaces closed
//! standard descriptors with `/dev/null` and aborts if it cannot, and when init starts neither
//! `/dev/null` nor, yet, the console exists. The guest's console port appears only once the host
//! announces it, so init waits for it itself.
//!
//! Init refuses to run unless it is PID 1: it ends by rebooting, and on a host, as root, that
//! reboots the host. Tests use the harness's own `main` for the same reason.

#![cfg_attr(not(test), no_main)]

use std::ffi::CString;
use std::fs::{self, File};
use std::io::{self, PipeReader, Read, Write};
use std::os::fd::{AsRawFd, IntoRawFd};
use std::os::unix::fs as unix_fs;
use std::os::unix::process::ExitStatusExt;
use std::path::Path;
use std::process::{self, ChildStdin, Stdio};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::Duration;

use peerspan_runtime_protocol::{
    AGENT_PORT, Command, Message, ROOTFS_TAG, WORKSPACE_PATH, WORKSPACE_TAG,
};
use rustix::fs::{Mode, OFlags};
use rustix::mount::MountFlags;
use rustix::net::{AddressFamily, SocketFlags, SocketType};

/// Modules to load, one file name per line, in dependency order.
const MODULE_ORDER: &str = "/lib/modules/order";
const MODULE_DIR: &str = "/lib/modules";
const NEW_ROOT: &str = "/rootfs";
/// The image, read-only, under the root.
const IMAGE: &str = "/image";
/// A tmpfs holding the root's writable layer, so writes outside the workspace last only as
/// long as the VM and never reach the image.
const LAYER: &str = "/layer";
/// How long to keep retrying the vsock connection while the transport comes up.
const CONNECT_ATTEMPTS: u32 = 100;
const CONNECT_RETRY: Duration = Duration::from_millis(50);
const DEFAULT_PATH: &str = "/usr/local/sbin:/usr/local/bin:/usr/sbin:/usr/bin:/sbin:/bin";

mod net;
mod workspace;

/// How long init waits for the console before running without one.
const CONSOLE_WAIT: Duration = Duration::from_secs(5);

#[cfg_attr(not(test), unsafe(no_mangle))]
extern "C" fn main(_argc: libc::c_int, _argv: *const *const libc::c_char) -> libc::c_int {
    if !rustix::process::getpid().is_init() {
        eprintln!("peerspan-guest-init: not PID 1; it runs only as a VM's init");
        return 1;
    }
    let mut phases = Phases::default();
    phases.mark("init");
    let result = std::panic::catch_unwind(move || {
        attach_console();
        phases.mark("console");
        run(phases)
    });
    match result {
        Ok(Ok(())) => {}
        Ok(Err(e)) => eprintln!("peerspan-guest-init: {e}"),
        Err(_) => eprintln!("peerspan-guest-init: panicked"),
    }
    rustix::fs::sync();
    // PID 1 must not exit, or the kernel panics; rebooting ends the VM instead.
    let _ = rustix::system::reboot(rustix::system::RebootCommand::Restart);
    0
}

/// Mounts `/dev` and points descriptors 0 to 2 at the console, or at `/dev/null` if the
/// console does not appear in time.
fn attach_console() {
    let _ = mount("devtmpfs", "/dev", "devtmpfs", MountFlags::empty());
    let deadline = std::time::Instant::now() + CONSOLE_WAIT;
    let flags = OFlags::RDWR | OFlags::NOCTTY;
    let console = loop {
        match rustix::fs::open("/dev/console", flags, Mode::empty()) {
            Ok(fd) => break Some(fd),
            Err(_) if std::time::Instant::now() >= deadline => break None,
            Err(_) => thread::sleep(Duration::from_millis(5)),
        }
    };
    let Some(fd) =
        console.or_else(|| rustix::fs::open("/dev/null", OFlags::RDWR, Mode::empty()).ok())
    else {
        return;
    };
    let _ = rustix::stdio::dup2_stdin(&fd);
    let _ = rustix::stdio::dup2_stdout(&fd);
    let _ = rustix::stdio::dup2_stderr(&fd);
    // With no descriptors open at boot, the console may itself be 0, 1 or 2, which must stay.
    if fd.as_raw_fd() <= 2 {
        let _ = fd.into_raw_fd();
    }
}

/// When init reached each step, by the guest's boot clock, for the host's measurements.
#[derive(Debug, Default)]
struct Phases(Vec<(String, u64)>);

impl Phases {
    fn mark(&mut self, name: &str) {
        let now = rustix::time::clock_gettime(rustix::time::ClockId::Boottime);
        let micros = u64::try_from(now.tv_sec).unwrap_or(0) * 1_000_000
            + u64::try_from(now.tv_nsec).unwrap_or(0) / 1_000;
        self.0.push((name.to_owned(), micros));
    }
}

fn run(mut phases: Phases) -> io::Result<()> {
    mount("proc", "/proc", "proc", MountFlags::empty())?;
    mount("sysfs", "/sys", "sysfs", MountFlags::empty())?;
    load_modules()?;
    phases.mark("modules");

    mount_root()?;
    mount(
        WORKSPACE_TAG,
        &format!("{NEW_ROOT}{WORKSPACE_PATH}"),
        "virtiofs",
        MountFlags::empty(),
    )?;
    mount(
        "proc",
        &format!("{NEW_ROOT}/proc"),
        "proc",
        MountFlags::empty(),
    )?;
    mount(
        "sysfs",
        &format!("{NEW_ROOT}/sys"),
        "sysfs",
        MountFlags::empty(),
    )?;
    mount(
        "devtmpfs",
        &format!("{NEW_ROOT}/dev"),
        "devtmpfs",
        MountFlags::empty(),
    )?;
    mount(
        "tmpfs",
        &format!("{NEW_ROOT}/tmp"),
        "tmpfs",
        MountFlags::empty(),
    )?;
    mount(
        "tmpfs",
        &format!("{NEW_ROOT}/run"),
        "tmpfs",
        MountFlags::empty(),
    )?;
    unix_fs::chroot(NEW_ROOT)?;
    std::env::set_current_dir("/")?;
    net::loopback_up()?;
    phases.mark("mounted");

    let stream = connect_host(AGENT_PORT)?;
    phases.mark("connected");
    let mut reader = stream.try_clone()?;
    let command = match Message::read_from(&mut reader)? {
        Some(Message::Run(command)) => command,
        other => {
            return Err(io::Error::other(format!(
                "expected a command, got {other:?}"
            )));
        }
    };
    if command.network {
        if let Err(e) = net::configure() {
            return fail(&mut reader, &format!("setting up the network: {e}"));
        }
        phases.mark("network");
    }
    let workspace = Path::new(WORKSPACE_PATH);
    let manifest = if command.archive {
        match workspace::unpack(&mut reader, workspace) {
            Ok(manifest) => manifest,
            Err(reason) => return fail(&mut reader, &reason),
        }
    } else {
        workspace::Manifest::new()
    };
    phases.mark("unpacked");

    let writer = Arc::new(Mutex::new(stream));
    let Some((code, signal)) = execute(&command, reader, &writer, phases) else {
        return Message::Exited {
            code: None,
            signal: None,
        }
        .write_to(&mut *writer.lock().expect("stream lock poisoned"));
    };
    let mut stream = writer.lock().expect("stream lock poisoned");
    if command.collect_changes {
        workspace::send_changes(workspace, &manifest, &mut *stream)?;
    }
    Message::Exited { code, signal }.write_to(&mut *stream)
}

/// Reports a run that failed before its command started.
fn fail(stream: &mut File, reason: &str) -> io::Result<()> {
    Message::Failed(reason.to_owned()).write_to(stream)?;
    Message::Exited {
        code: None,
        signal: None,
    }
    .write_to(stream)
}

/// Mounts the root: the image, read-only, under a writable layer in memory. The directories
/// the guest mounts over are made in the layer, so an image need not have them.
fn mount_root() -> io::Result<()> {
    for dir in [IMAGE, LAYER, NEW_ROOT] {
        fs::create_dir_all(dir)?;
    }
    mount(ROOTFS_TAG, IMAGE, "virtiofs", MountFlags::RDONLY)?;
    mount("tmpfs", LAYER, "tmpfs", MountFlags::empty())?;
    fs::create_dir(format!("{LAYER}/upper"))?;
    fs::create_dir(format!("{LAYER}/work"))?;
    mount_with(
        "overlay",
        NEW_ROOT,
        "overlay",
        MountFlags::empty(),
        &format!("lowerdir={IMAGE},upperdir={LAYER}/upper,workdir={LAYER}/work"),
    )?;
    for dir in [
        WORKSPACE_PATH,
        "/proc",
        "/sys",
        "/dev",
        "/tmp",
        "/run",
        "/etc",
    ] {
        fs::create_dir_all(format!("{NEW_ROOT}{dir}"))?;
    }
    Ok(())
}

fn mount(source: &str, target: &str, fstype: &str, flags: MountFlags) -> io::Result<()> {
    mount_with(source, target, fstype, flags, "")
}

fn mount_with(
    source: &str,
    target: &str,
    fstype: &str,
    flags: MountFlags,
    data: &str,
) -> io::Result<()> {
    let data = CString::new(data).expect("mount data has no NUL bytes");
    let data = (!data.is_empty()).then_some(data.as_c_str());
    rustix::mount::mount(source, target, fstype, flags, data).map_err(|e| {
        let e = io::Error::from(e);
        io::Error::new(e.kind(), format!("mount {source} on {target}: {e}"))
    })
}

fn load_modules() -> io::Result<()> {
    let order = match fs::read_to_string(MODULE_ORDER) {
        Ok(order) => order,
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(()),
        Err(e) => return Err(e),
    };
    for name in order.lines().filter(|line| !line.is_empty()) {
        let file = File::open(format!("{MODULE_DIR}/{name}"))?;
        match rustix::system::finit_module(&file, c"", 0) {
            Ok(()) => {}
            // Built in, or loaded already.
            Err(e) if e == rustix::io::Errno::EXIST => {}
            Err(e) => {
                let e = io::Error::from(e);
                return Err(io::Error::new(e.kind(), format!("load module {name}: {e}")));
            }
        }
    }
    Ok(())
}

fn connect_host(port: u32) -> io::Result<File> {
    let mut last = None;
    for _ in 0..CONNECT_ATTEMPTS {
        match connect_vsock(libc::VMADDR_CID_HOST, port) {
            Ok(stream) => return Ok(stream),
            Err(e) => last = Some(e),
        }
        thread::sleep(CONNECT_RETRY);
    }
    let e = last.expect("at least one attempt was made");
    Err(io::Error::new(
        e.kind(),
        format!("connect to host vsock port {port}: {e}"),
    ))
}

fn connect_vsock(cid: u32, port: u32) -> io::Result<File> {
    let fd = rustix::net::socket_with(
        AddressFamily::VSOCK,
        SocketType::STREAM,
        SocketFlags::CLOEXEC,
        None,
    )?;
    // SAFETY: sockaddr_vm is plain data, and all-zero is a valid starting value.
    let mut addr: libc::sockaddr_vm = unsafe { std::mem::zeroed() };
    addr.svm_family = libc::sa_family_t::try_from(libc::AF_VSOCK).expect("AF_VSOCK fits");
    addr.svm_cid = cid;
    addr.svm_port = port;
    let len = libc::socklen_t::try_from(size_of::<libc::sockaddr_vm>()).expect("small struct");
    // SAFETY: addr is a valid sockaddr_vm and len is its exact size.
    let rc = unsafe {
        libc::connect(
            std::os::fd::AsRawFd::as_raw_fd(&fd),
            std::ptr::from_ref(&addr).cast(),
            len,
        )
    };
    if rc != 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(File::from(fd))
}

/// Runs the command, streaming its output, and returns its exit code or signal, or `None` if it
/// could not be started, after reporting why.
fn execute(
    command: &Command,
    reader: File,
    writer: &Arc<Mutex<File>>,
    mut phases: Phases,
) -> Option<(Option<i32>, Option<i32>)> {
    let lock = || writer.lock().expect("stream lock poisoned");
    let fail = |reason: String| {
        let _ = Message::Failed(reason).write_to(&mut *lock());
    };
    let Some((program, args)) = command.argv.split_first() else {
        fail("empty command".into());
        return None;
    };
    let mut spawner = process::Command::new(program);
    spawner
        .args(args)
        .current_dir(&command.cwd)
        .env_clear()
        .env("PATH", DEFAULT_PATH)
        .envs(command.env.iter().map(|(name, value)| (name, value)))
        .stdin(if command.stdin {
            Stdio::piped()
        } else {
            Stdio::null()
        });
    // Combined output shares one pipe between the two descriptors, which keeps the order of
    // the command's writes; two pipes cannot.
    let combined = if command.combined_output {
        match combined_pipe(&mut spawner) {
            Ok(reader) => Some(reader),
            Err(e) => {
                fail(format!("creating the output pipe: {e}"));
                return None;
            }
        }
    } else {
        spawner.stdout(Stdio::piped()).stderr(Stdio::piped());
        None
    };
    let spawned = spawner.spawn();
    // The command holds the shared pipe's write ends; its output ends only once they close.
    drop(spawner);
    let mut child = match spawned {
        Ok(child) => child,
        Err(e) => {
            fail(format!("cannot run {program}: {e}"));
            return None;
        }
    };
    phases.mark("spawned");
    let report = Message::Phases(phases.0).write_to(&mut *lock());
    if report
        .and_then(|()| Message::Started.write_to(&mut *lock()))
        .is_err()
    {
        let _ = child.kill();
    }
    if let Some(stdin) = child.stdin.take() {
        feed_stdin(reader, stdin);
    }
    let pumps = match combined {
        Some(output) => vec![pump(output, Arc::clone(writer), Message::Stdout)],
        None => vec![
            pump(
                child.stdout.take().expect("stdout is piped"),
                Arc::clone(writer),
                Message::Stdout,
            ),
            pump(
                child.stderr.take().expect("stderr is piped"),
                Arc::clone(writer),
                Message::Stderr,
            ),
        ],
    };
    let status = child.wait();
    for handle in pumps {
        handle.join().expect("output pump panicked");
    }
    match status {
        Ok(status) => Some((status.code(), status.signal())),
        Err(_) => Some((None, None)),
    }
}

/// Points the command's standard output and error at one new pipe, and returns its read end.
fn combined_pipe(spawner: &mut process::Command) -> io::Result<PipeReader> {
    let (reader, writer) = io::pipe()?;
    spawner.stdout(writer.try_clone()?).stderr(writer);
    Ok(reader)
}

/// Copies the host's [`Message::Stdin`] frames to the command's standard input, and closes it
/// at [`Message::StdinEnd`]. Input the command no longer reads is discarded. The thread ends
/// with the VM if the input never does.
fn feed_stdin(mut stream: File, stdin: ChildStdin) {
    thread::spawn(move || {
        let mut stdin = Some(stdin);
        loop {
            match Message::read_from(&mut stream) {
                Ok(Some(Message::Stdin(bytes))) => {
                    if let Some(pipe) = stdin.as_mut()
                        && pipe.write_all(&bytes).is_err()
                    {
                        stdin = None;
                    }
                }
                _ => return,
            }
        }
    });
}

/// Copies one output pipe to the host until it closes.
fn pump(
    mut pipe: impl Read + Send + 'static,
    writer: Arc<Mutex<File>>,
    frame: fn(Vec<u8>) -> Message,
) -> thread::JoinHandle<()> {
    thread::spawn(move || {
        let mut buf = vec![0u8; 64 * 1024];
        loop {
            let n = match pipe.read(&mut buf) {
                Ok(0) | Err(_) => return,
                Ok(n) => n,
            };
            let mut stream = writer.lock().expect("stream lock poisoned");
            if frame(buf[..n].to_vec()).write_to(&mut *stream).is_err() {
                return;
            }
        }
    })
}
