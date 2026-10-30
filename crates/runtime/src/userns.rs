// SPDX-License-Identifier: Apache-2.0
// Copyright The Peerspan Authors

//! A user namespace whose root is one host user, for the workspace server.
//!
//! virtiofsd builds its sandbox as root before it serves anything. Started as root inside this
//! namespace it builds the same sandbox, but its root is the workspace's owner on the host, with
//! no privilege outside the namespace: it can act only on that user's files, and the guest's root
//! owns what that user owns. An unprivileged virtiofsd would set up such a namespace with
//! newuidmap, which needs the user in /etc/passwd and /etc/subuid; the runtime, as root, writes
//! the mapping itself.

use std::fs::{self, File};
use std::io::{self, ErrorKind, Read};
use std::os::fd::{AsRawFd, BorrowedFd};
use std::os::unix::process::CommandExt;
use std::process::Command;

use anyhow::{Context, bail};
use rustix::process::{
    Gid, Pid, Signal, Uid, WaitOptions, getpid, getppid, kill_process,
    set_parent_process_death_signal, waitpid,
};
use rustix::thread::{
    LinkNameSpaceType, UnshareFlags, move_into_link_name_space, set_thread_groups,
    set_thread_res_gid, set_thread_res_uid, unshare_unsafe,
};

/// A user namespace in which only ID 0 is mapped, to one host user and group.
#[derive(Debug)]
pub struct UserNamespace(File);

impl UserNamespace {
    /// Creates the namespace. A child process creates it, the runtime maps its IDs from
    /// outside, and the child is killed once the namespace is held open.
    ///
    /// # Errors
    ///
    /// Fails if the namespace cannot be created or its IDs mapped.
    pub fn new(uid: u32, gid: u32) -> anyhow::Result<Self> {
        let (mut ready_reader, ready_writer) = io::pipe()?;
        let parent = getpid();
        // SAFETY: the child makes only system calls, which are async-signal-safe, and never
        // returns, so forking a process with other threads is sound.
        let pid = unsafe { libc::fork() };
        if pid < 0 {
            bail!("fork: {}", io::Error::last_os_error());
        }
        let Some(child) = Pid::from_raw(pid) else {
            let ok = set_parent_process_death_signal(Some(Signal::KILL)).is_ok()
                && getppid() == Some(parent)
                // SAFETY: a new user namespace unshares nothing the child's code relies on.
                && unsafe { unshare_unsafe(UnshareFlags::NEWUSER) }.is_ok();
            let _ = rustix::io::write(&ready_writer, &[u8::from(ok)]);
            // Waits to be killed.
            loop {
                rustix::event::pause();
            }
        };
        let holder = Holder(child);
        drop(ready_writer);
        let mut ok = [0];
        ready_reader
            .read_exact(&mut ok)
            .context("waiting for the user namespace")?;
        if ok[0] != 1 {
            bail!("could not create a user namespace for the workspace server");
        }
        let proc = format!("/proc/{}", holder.0.as_raw_nonzero());
        fs::write(format!("{proc}/uid_map"), format!("0 {uid} 1\n"))
            .context("mapping the workspace server's user")?;
        fs::write(format!("{proc}/gid_map"), format!("0 {gid} 1\n"))
            .context("mapping the workspace server's group")?;
        let namespace = File::open(format!("{proc}/ns/user")).context("opening the namespace")?;
        Ok(Self(namespace))
    }

    /// Makes `command`'s process enter the namespace as its root just before it runs, and still
    /// be killed if the supervisor dies first. The namespace must outlive the spawn.
    pub fn enter<'cmd>(&self, command: &'cmd mut Command) -> &'cmd mut Command {
        let fd = self.0.as_raw_fd();
        let supervisor = getpid();
        // SAFETY: the hook makes only system calls, which are async-signal-safe, in the forked
        // child, which has one thread, so changing that thread's IDs changes the process's. The
        // descriptor stays open until after the child has started.
        unsafe {
            command.pre_exec(move || {
                let namespace = BorrowedFd::borrow_raw(fd);
                move_into_link_name_space(namespace, Some(LinkNameSpaceType::User))?;
                set_thread_groups(&[])?;
                set_thread_res_gid(Gid::ROOT, Gid::ROOT, Gid::ROOT)?;
                set_thread_res_uid(Uid::ROOT, Uid::ROOT, Uid::ROOT)?;
                // Changing user cleared the death signal.
                set_parent_process_death_signal(Some(Signal::KILL))?;
                if getppid() != Some(supervisor) {
                    return Err(io::Error::from(ErrorKind::Interrupted));
                }
                Ok(())
            })
        }
    }
}

/// The child that created a namespace; it is killed and reaped on drop.
struct Holder(Pid);

impl Drop for Holder {
    fn drop(&mut self) {
        let _ = kill_process(self.0, Signal::KILL);
        let _ = waitpid(Some(self.0), WaitOptions::empty());
    }
}
