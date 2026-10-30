// SPDX-License-Identifier: Apache-2.0
// Copyright The Peerspan Authors

//! Confines the launcher before it builds a VM.
//!
//! libkrun's devices act with the full authority of the process that runs them, so the launcher
//! moves into new mount, PID, network, IPC and UTS namespaces, pivots into a root that holds only
//! what the VM needs, and drops to the run's own user before libkrun sees any guest input.

use std::ffi::CStr;
use std::fs::{self, File};
use std::io;
use std::os::unix::fs::{DirBuilderExt, MetadataExt};
use std::path::{Path, PathBuf};

use anyhow::{Context, bail};
use rustix::mount::{
    MountFlags, MountPropagationFlags, UnmountFlags, mount, mount_bind, mount_change,
    mount_remount, unmount,
};
use rustix::process::{
    DumpableBehavior, Gid, Pid, Signal, Uid, WaitOptions, getppid, kill_process, pivot_root,
    set_dumpable_behavior, set_parent_process_death_signal, waitpid,
};
use rustix::thread::{
    UnshareFlags, set_no_new_privs, set_thread_groups, set_thread_res_gid, set_thread_res_uid,
    set_thread_uid, unshare_unsafe,
};

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

        // SAFETY: none of these flags unshares anything other code in the process relies on
        // sharing, such as descriptors or the filesystem context; the caller has no threads.
        unsafe {
            unshare_unsafe(
                UnshareFlags::NEWNS
                    | UnshareFlags::NEWPID
                    | UnshareFlags::NEWNET
                    | UnshareFlags::NEWIPC
                    | UnshareFlags::NEWUTS,
            )
        }
        .context("unshare namespaces")?;
        fork_into_namespace(self.uid, self.gid)?;
        mount_change(
            "/",
            MountPropagationFlags::REC | MountPropagationFlags::PRIVATE,
        )
        .context("making mounts private")?;

        let root = self.dir;
        mount(
            "tmpfs",
            root,
            "tmpfs",
            MountFlags::NOSUID | MountFlags::NODEV,
            None::<&CStr>,
        )
        .context("mounting the jail root")?;
        let read_only = MountFlags::RDONLY | MountFlags::NOSUID | MountFlags::NODEV;
        bind(self.rootfs, &inside(root, ROOTFS), read_only)?;
        bind(self.kernel, &inside(root, KERNEL), read_only)?;
        bind(self.initrd, &inside(root, INITRD), read_only)?;
        bind(
            self.sockets,
            &inside(root, SOCKETS),
            MountFlags::NOSUID | MountFlags::NODEV,
        )?;
        bind(Path::new(KVM), &inside(root, KVM), MountFlags::NOSUID)?;
        // libkrun's file sharing resolves descriptors through /proc/self/fd. In the new PID
        // namespace this /proc shows only the jailed process.
        let proc_dir = inside(root, PROC);
        fs::create_dir(&proc_dir)?;
        mount(
            "proc",
            &proc_dir,
            "proc",
            MountFlags::NOSUID | MountFlags::NODEV | MountFlags::NOEXEC,
            None::<&CStr>,
        )
        .context("mounting /proc")?;

        let old = inside(root, "/.old");
        fs::DirBuilder::new().mode(0o700).create(&old)?;
        pivot_root(root, &old).context("pivot_root")?;
        std::env::set_current_dir("/")?;
        unmount("/.old", UnmountFlags::DETACH).context("detaching the old root")?;
        fs::remove_dir("/.old")?;
        mount_remount("/", MountFlags::BIND | read_only, "")
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
    let Some(child) = Pid::from_raw(pid) else {
        // Die with the parent, so stopping the launcher stops the VM.
        set_parent_process_death_signal(Some(Signal::KILL)).context("parent death signal")?;
        return Ok(());
    };
    if let Err(e) = drop_privileges(uid, gid, &[]) {
        eprintln!("peerspan-runtime: {e:#}");
        let _ = kill_process(child, Signal::KILL);
        std::process::exit(125);
    }
    let status = loop {
        match waitpid(Some(child), WaitOptions::empty()) {
            Ok(Some((_, status))) => break status,
            Err(e) if e == rustix::io::Errno::INTR => {}
            _ => std::process::exit(125),
        }
    };
    let code = match (status.exit_status(), status.terminating_signal()) {
        (Some(code), _) => code,
        (None, Some(signal)) => 128 + signal,
        (None, None) => 125,
    };
    std::process::exit(code);
}

/// Becomes `uid` and `gid` with only `groups`, for good. Changing user clears the parent death
/// signal, which every process of a run has so that it dies with the supervisor, so this sets
/// it again.
///
/// The system calls change only the calling thread's credentials, unlike the C library's,
/// which change every thread's; so this refuses to run in a process with other threads.
fn drop_privileges(uid: u32, gid: u32, groups: &[u32]) -> anyhow::Result<()> {
    let threads = fs::read_to_string("/proc/self/status")
        .ok()
        .and_then(|status| {
            status
                .lines()
                .find_map(|line| line.strip_prefix("Threads:"))
                .and_then(|n| n.trim().parse::<u32>().ok())
        });
    if threads != Some(1) {
        bail!("cannot drop privileges with other threads running ({threads:?})");
    }
    let parent = getppid();
    let groups: Vec<Gid> = groups.iter().map(|&g| Gid::from_raw(g)).collect();
    set_thread_groups(&groups).context("setgroups")?;
    let (uid, gid) = (Uid::from_raw(uid), Gid::from_raw(gid));
    set_thread_res_gid(gid, gid, gid).context("setresgid")?;
    set_thread_res_uid(uid, uid, uid).context("setresuid")?;
    set_no_new_privs(true).context("no_new_privs")?;
    set_dumpable_behavior(DumpableBehavior::NotDumpable).context("not dumpable")?;
    // This only probes, and must fail.
    if set_thread_uid(Uid::ROOT).is_ok() {
        bail!("privileges were not dropped");
    }
    set_parent_process_death_signal(Some(Signal::KILL)).context("parent death signal")?;
    if getppid() != parent {
        bail!("the parent process died");
    }
    Ok(())
}

/// Bind-mounts `source` at `target`, creating the mount point, then applies `flags`.
fn bind(source: &Path, target: &Path, flags: MountFlags) -> anyhow::Result<()> {
    if let Some(parent) = target.parent() {
        fs::create_dir_all(parent)?;
    }
    if source.is_dir() {
        fs::create_dir(target)?;
    } else {
        File::create(target)?;
    }
    let what = || format!("binding {} at {}", source.display(), target.display());
    mount_bind(source, target).with_context(what)?;
    // A bind mount ignores flags until it is remounted.
    mount_remount(target, MountFlags::BIND | flags, "").with_context(what)
}

fn inside(root: &Path, path: &str) -> PathBuf {
    root.join(path.trim_start_matches('/'))
}
