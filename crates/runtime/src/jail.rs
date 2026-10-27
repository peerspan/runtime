// SPDX-License-Identifier: Apache-2.0
// Copyright The Peerspan Authors

//! Confines the launcher before it builds a VM.
//!
//! libkrun's devices act with the full authority of the process that runs them, so the launcher
//! moves into new mount, PID, network, IPC and UTS namespaces, pivots into a root that holds only
//! what the VM needs, and drops to the run's own user before libkrun sees any guest input.

use std::ffi::CString;
use std::fs::{self, File};
use std::io;
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::{DirBuilderExt, MetadataExt};
use std::path::{Path, PathBuf};

use anyhow::{Context, bail};

/// Where each file appears inside the jail.
pub const ROOTFS: &str = "/rootfs";
pub const KERNEL: &str = "/vmlinux";
pub const INITRD: &str = "/initramfs";
pub const SOCKETS: &str = "/sockets";
const KVM: &str = "/dev/kvm";
const PROC: &str = "/proc";

/// What the jail holds and whom the launcher becomes.
#[derive(Debug)]
pub struct Jail<'cfg> {
    /// An empty directory, owned by root, that becomes the jail's root.
    pub dir: &'cfg Path,
    pub rootfs: &'cfg Path,
    pub kernel: &'cfg Path,
    pub initrd: &'cfg Path,
    /// The directory holding the workspace and agent sockets.
    pub sockets: &'cfg Path,
    pub uid: u32,
    pub gid: u32,
}

impl Jail<'_> {
    /// Enters the jail. Afterwards the process sees only the jail's paths and its own process,
    /// has no network, and runs as `uid` and `gid` with the kvm group and no way to regain
    /// privileges.
    ///
    /// Entering a PID namespace needs a new process, so this forks: the original process waits
    /// for the jailed one and exits with its status, and only the jailed one returns. The caller
    /// must not have started threads.
    ///
    /// # Errors
    ///
    /// Fails if any step fails; the process must then exit rather than boot unconfined.
    pub fn enter(&self) -> anyhow::Result<()> {
        let kvm_gid = fs::metadata(KVM).context("/dev/kvm")?.gid();

        check(
            // SAFETY: unshare takes only flags.
            unsafe {
                libc::unshare(
                    libc::CLONE_NEWNS
                        | libc::CLONE_NEWPID
                        | libc::CLONE_NEWNET
                        | libc::CLONE_NEWIPC
                        | libc::CLONE_NEWUTS,
                )
            },
            "unshare namespaces",
        )?;
        fork_into_namespace(self.uid, self.gid)?;
        mount(None, Path::new("/"), None, libc::MS_REC | libc::MS_PRIVATE)
            .context("making mounts private")?;

        let root = self.dir;
        mount(
            Some("tmpfs"),
            root,
            Some("tmpfs"),
            libc::MS_NOSUID | libc::MS_NODEV,
        )
        .context("mounting the jail root")?;
        let read_only = libc::MS_RDONLY | libc::MS_NOSUID | libc::MS_NODEV;
        bind(self.rootfs, &inside(root, ROOTFS), read_only)?;
        bind(self.kernel, &inside(root, KERNEL), read_only)?;
        bind(self.initrd, &inside(root, INITRD), read_only)?;
        bind(
            self.sockets,
            &inside(root, SOCKETS),
            libc::MS_NOSUID | libc::MS_NODEV,
        )?;
        bind(Path::new(KVM), &inside(root, KVM), libc::MS_NOSUID)?;
        // libkrun's file sharing resolves descriptors through /proc/self/fd. In the new PID
        // namespace this /proc shows only the jailed process.
        let proc_dir = inside(root, PROC);
        fs::create_dir(&proc_dir)?;
        mount(
            Some("proc"),
            &proc_dir,
            Some("proc"),
            libc::MS_NOSUID | libc::MS_NODEV | libc::MS_NOEXEC,
        )
        .context("mounting /proc")?;

        let old = inside(root, "/.old");
        fs::DirBuilder::new().mode(0o700).create(&old)?;
        pivot_root(root, &old)?;
        std::env::set_current_dir("/")?;
        check(
            // SAFETY: the path is a valid NUL-terminated string.
            unsafe { libc::umount2(c"/.old".as_ptr(), libc::MNT_DETACH) },
            "detaching the old root",
        )?;
        fs::remove_dir("/.old")?;
        mount(
            None,
            Path::new("/"),
            None,
            libc::MS_REMOUNT | libc::MS_BIND | read_only,
        )
        .context("making the jail root read-only")?;

        drop_privileges(self.uid, self.gid, &[kvm_gid])
    }
}

/// Forks; the child, the first process of the new PID namespace, returns. The parent drops to
/// `uid` and `gid` with no groups, waits for the child, and exits with its status.
fn fork_into_namespace(uid: u32, gid: u32) -> anyhow::Result<()> {
    // SAFETY: the caller has started no threads, so the child may run arbitrary code.
    let pid = unsafe { libc::fork() };
    if pid < 0 {
        bail!("fork: {}", io::Error::last_os_error());
    }
    if pid == 0 {
        // Die with the parent, so stopping the launcher stops the VM.
        // SAFETY: prctl with this option takes only integers.
        let rc = unsafe { libc::prctl(libc::PR_SET_PDEATHSIG, libc::SIGKILL, 0, 0, 0) };
        return check(rc, "parent death signal");
    }
    if let Err(e) = drop_privileges(uid, gid, &[]) {
        eprintln!("peerspan-runtime: {e:#}");
        // SAFETY: kill takes only integers; pid is our child.
        unsafe { libc::kill(pid, libc::SIGKILL) };
        std::process::exit(125);
    }
    let mut status = 0;
    // SAFETY: status is a valid out-pointer and pid is our child.
    while unsafe { libc::waitpid(pid, &raw mut status, 0) } < 0 {
        if io::Error::last_os_error().kind() != io::ErrorKind::Interrupted {
            std::process::exit(125);
        }
    }
    let code = if libc::WIFEXITED(status) {
        libc::WEXITSTATUS(status)
    } else {
        128 + libc::WTERMSIG(status)
    };
    std::process::exit(code);
}

/// Becomes `uid` and `gid` with only `groups`, for good. Changing user clears the parent death
/// signal, which every process of a run has so that it dies with the supervisor, so this sets
/// it again.
fn drop_privileges(uid: u32, gid: u32, groups: &[libc::gid_t]) -> anyhow::Result<()> {
    // SAFETY: getppid has no preconditions.
    let parent = unsafe { libc::getppid() };
    // SAFETY: groups is a valid slice of the given length.
    let rc = unsafe { libc::setgroups(groups.len(), groups.as_ptr()) };
    check(rc, "setgroups")?;
    // SAFETY: setresgid takes only IDs.
    let rc = unsafe { libc::setresgid(gid, gid, gid) };
    check(rc, "setresgid")?;
    // SAFETY: setresuid takes only IDs.
    let rc = unsafe { libc::setresuid(uid, uid, uid) };
    check(rc, "setresuid")?;
    // SAFETY: prctl with this option takes only integers.
    let rc = unsafe { libc::prctl(libc::PR_SET_NO_NEW_PRIVS, 1, 0, 0, 0) };
    check(rc, "no_new_privs")?;
    // SAFETY: prctl with this option takes only integers.
    let rc = unsafe { libc::prctl(libc::PR_SET_DUMPABLE, 0, 0, 0, 0) };
    check(rc, "not dumpable")?;
    // SAFETY: setuid takes only an ID; this call only probes and must fail.
    if unsafe { libc::setuid(0) } == 0 {
        bail!("privileges were not dropped");
    }
    // SAFETY: prctl with this option takes only integers.
    let rc = unsafe { libc::prctl(libc::PR_SET_PDEATHSIG, libc::SIGKILL, 0, 0, 0) };
    check(rc, "parent death signal")?;
    // SAFETY: getppid has no preconditions.
    if unsafe { libc::getppid() } != parent {
        bail!("the parent process died");
    }
    Ok(())
}

/// Bind-mounts `source` at `target`, creating the mount point, then applies `flags`.
fn bind(source: &Path, target: &Path, flags: libc::c_ulong) -> anyhow::Result<()> {
    if let Some(parent) = target.parent() {
        fs::create_dir_all(parent)?;
    }
    if source.is_dir() {
        fs::create_dir(target)?;
    } else {
        File::create(target)?;
    }
    let what = || format!("binding {} at {}", source.display(), target.display());
    mount(source.to_str(), target, None, libc::MS_BIND).with_context(what)?;
    // A bind mount ignores flags until it is remounted.
    mount(None, target, None, libc::MS_REMOUNT | libc::MS_BIND | flags).with_context(what)
}

fn inside(root: &Path, path: &str) -> PathBuf {
    root.join(path.trim_start_matches('/'))
}

fn mount(
    source: Option<&str>,
    target: &Path,
    fstype: Option<&str>,
    flags: libc::c_ulong,
) -> io::Result<()> {
    let c = |s: &str| CString::new(s).map_err(io::Error::other);
    let source = source.map(c).transpose()?;
    let fstype = fstype.map(c).transpose()?;
    let target = CString::new(target.as_os_str().as_bytes()).map_err(io::Error::other)?;
    let ptr = |s: &Option<CString>| s.as_ref().map_or(std::ptr::null(), |s| s.as_ptr());
    // SAFETY: every pointer is null or a NUL-terminated string that outlives the call.
    let rc = unsafe {
        libc::mount(
            ptr(&source),
            target.as_ptr(),
            ptr(&fstype),
            flags,
            std::ptr::null(),
        )
    };
    if rc != 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(())
}

fn pivot_root(new_root: &Path, put_old: &Path) -> anyhow::Result<()> {
    let new_root_c = CString::new(new_root.as_os_str().as_bytes())?;
    let put_old_c = CString::new(put_old.as_os_str().as_bytes())?;
    // SAFETY: both paths are valid NUL-terminated strings that outlive the call.
    let rc = unsafe {
        libc::syscall(
            libc::SYS_pivot_root,
            new_root_c.as_ptr(),
            put_old_c.as_ptr(),
        )
    };
    if rc != 0 {
        bail!("pivot_root: {}", io::Error::last_os_error());
    }
    Ok(())
}

fn check(rc: i32, what: &str) -> anyhow::Result<()> {
    if rc != 0 {
        let e = io::Error::last_os_error();
        bail!("{what}: {e}");
    }
    Ok(())
}
