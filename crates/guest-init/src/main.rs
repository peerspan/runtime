// SPDX-License-Identifier: Apache-2.0
// Copyright The Peerspan Authors

//! PID 1 inside a runtime microVM.
//!
//! It loads the modules the initramfs lists, mounts the root filesystem and the workspace,
//! switches into the root filesystem, connects to the supervisor over vsock, runs the one
//! command it is sent while streaming its output back, and reboots, which ends the VM.

use std::ffi::CString;
use std::fs::{self, File};
use std::io::{self, Read};
use std::os::fd::{FromRawFd, OwnedFd};
use std::os::unix::fs as unix_fs;
use std::os::unix::process::ExitStatusExt;
use std::process::{self, Stdio};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::Duration;

use peerspan_runtime_protocol::{
    AGENT_PORT, Command, Message, ROOTFS_TAG, WORKSPACE_PATH, WORKSPACE_TAG,
};

/// Modules to load, one file name per line, in dependency order.
const MODULE_ORDER: &str = "/lib/modules/order";
const MODULE_DIR: &str = "/lib/modules";
const NEW_ROOT: &str = "/rootfs";
/// How long to keep retrying the vsock connection while the transport comes up.
const CONNECT_ATTEMPTS: u32 = 100;
const CONNECT_RETRY: Duration = Duration::from_millis(50);
/// Exit code reported when the command cannot be started, as shells do.
const EXIT_NOT_RUN: i32 = 127;
const DEFAULT_PATH: &str = "/usr/local/sbin:/usr/local/bin:/usr/sbin:/usr/bin:/sbin:/bin";

fn main() {
    if let Err(e) = run() {
        eprintln!("peerspan-guest-init: {e}");
    }
    // SAFETY: sync takes no arguments and cannot fail.
    unsafe { libc::sync() };
    // PID 1 must not exit, or the kernel panics; rebooting ends the VM instead.
    // SAFETY: reboot with a valid command constant only affects the guest kernel.
    unsafe { libc::reboot(libc::RB_AUTOBOOT) };
}

fn run() -> io::Result<()> {
    mount("proc", "/proc", "proc", 0)?;
    mount("sysfs", "/sys", "sysfs", 0)?;
    mount("devtmpfs", "/dev", "devtmpfs", 0)?;
    load_modules()?;

    mount(ROOTFS_TAG, NEW_ROOT, "virtiofs", libc::MS_RDONLY)?;
    mount(
        WORKSPACE_TAG,
        &format!("{NEW_ROOT}{WORKSPACE_PATH}"),
        "virtiofs",
        0,
    )?;
    mount("proc", &format!("{NEW_ROOT}/proc"), "proc", 0)?;
    mount("sysfs", &format!("{NEW_ROOT}/sys"), "sysfs", 0)?;
    mount("devtmpfs", &format!("{NEW_ROOT}/dev"), "devtmpfs", 0)?;
    mount("tmpfs", &format!("{NEW_ROOT}/tmp"), "tmpfs", 0)?;
    mount("tmpfs", &format!("{NEW_ROOT}/run"), "tmpfs", 0)?;
    unix_fs::chroot(NEW_ROOT)?;
    std::env::set_current_dir("/")?;

    let stream = connect_host(AGENT_PORT)?;
    let mut reader = stream.try_clone()?;
    let command = match Message::read_from(&mut reader)? {
        Some(Message::Run(command)) => command,
        other => {
            return Err(io::Error::other(format!(
                "expected a command, got {other:?}"
            )));
        }
    };
    let writer = Arc::new(Mutex::new(stream));
    let code = execute(&command, &writer);
    Message::Exit(code).write_to(&mut *writer.lock().expect("stream lock poisoned"))
}

fn mount(source: &str, target: &str, fstype: &str, flags: libc::c_ulong) -> io::Result<()> {
    let c = |s: &str| CString::new(s).expect("mount arguments have no NUL bytes");
    let (source_c, target_c, fstype_c) = (c(source), c(target), c(fstype));
    // SAFETY: all pointers are valid NUL-terminated strings that outlive the call, and a null
    // data pointer is allowed.
    let rc = unsafe {
        libc::mount(
            source_c.as_ptr(),
            target_c.as_ptr(),
            fstype_c.as_ptr(),
            flags,
            std::ptr::null(),
        )
    };
    if rc != 0 {
        let e = io::Error::last_os_error();
        return Err(io::Error::new(
            e.kind(),
            format!("mount {source} on {target}: {e}"),
        ));
    }
    Ok(())
}

fn load_modules() -> io::Result<()> {
    let order = match fs::read_to_string(MODULE_ORDER) {
        Ok(order) => order,
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(()),
        Err(e) => return Err(e),
    };
    for name in order.lines().filter(|line| !line.is_empty()) {
        let file = File::open(format!("{MODULE_DIR}/{name}"))?;
        // SAFETY: finit_module reads from a valid open descriptor with an empty, NUL-terminated
        // parameter string and no flags.
        let rc = unsafe {
            libc::syscall(
                libc::SYS_finit_module,
                std::os::fd::AsRawFd::as_raw_fd(&file),
                c"".as_ptr(),
                0,
            )
        };
        if rc != 0 {
            let e = io::Error::last_os_error();
            if e.raw_os_error() != Some(libc::EEXIST) {
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
    // SAFETY: socket has no memory-safety preconditions.
    let fd = unsafe { libc::socket(libc::AF_VSOCK, libc::SOCK_STREAM | libc::SOCK_CLOEXEC, 0) };
    if fd < 0 {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: fd is a freshly created socket that nothing else owns.
    let fd = unsafe { OwnedFd::from_raw_fd(fd) };
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

/// Runs the command, streaming its output, and returns the code to report.
fn execute(command: &Command, writer: &Arc<Mutex<File>>) -> i32 {
    let Some((program, args)) = command.argv.split_first() else {
        report(writer, "empty command");
        return EXIT_NOT_RUN;
    };
    let mut child = process::Command::new(program);
    child
        .args(args)
        .current_dir(&command.cwd)
        .env_clear()
        .env("PATH", DEFAULT_PATH)
        .envs(command.env.iter().map(|(name, value)| (name, value)))
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    let mut child = match child.spawn() {
        Ok(child) => child,
        Err(e) => {
            report(writer, &format!("cannot run {program}: {e}"));
            return EXIT_NOT_RUN;
        }
    };
    let stdout = child.stdout.take().expect("stdout is piped");
    let stderr = child.stderr.take().expect("stderr is piped");
    let pumps = [
        pump(stdout, Arc::clone(writer), Message::Stdout),
        pump(stderr, Arc::clone(writer), Message::Stderr),
    ];
    let status = child.wait();
    for handle in pumps {
        handle.join().expect("output pump panicked");
    }
    match status {
        Ok(status) => status
            .code()
            .or_else(|| status.signal().map(|signal| 128 + signal))
            .unwrap_or(EXIT_NOT_RUN),
        Err(e) => {
            report(writer, &format!("waiting for {program}: {e}"));
            EXIT_NOT_RUN
        }
    }
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

/// Sends a runtime error to the command's standard error, where the caller will see it.
fn report(writer: &Arc<Mutex<File>>, message: &str) {
    let line = format!("peerspan-guest-init: {message}\n").into_bytes();
    let mut stream = writer.lock().expect("stream lock poisoned");
    // The final Exit frame carries the failure even if this write is lost.
    let _ = Message::Stderr(line).write_to(&mut *stream);
}
