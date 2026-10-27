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
use std::os::fd::AsRawFd;
use std::os::unix::process::CommandExt;
use std::process::Command;

use anyhow::{Context, bail};

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
        let ready = ready_writer.as_raw_fd();
        // SAFETY: getpid has no preconditions.
        let parent = unsafe { libc::getpid() };
        // SAFETY: the child calls only async-signal-safe functions and never returns, so forking
        // a process with other threads is sound.
        let pid = unsafe { libc::fork() };
        if pid < 0 {
            bail!("fork: {}", io::Error::last_os_error());
        }
        if pid == 0 {
            // SAFETY: as above; the child waits to be killed.
            unsafe {
                let ok = libc::prctl(libc::PR_SET_PDEATHSIG, libc::SIGKILL, 0, 0, 0) == 0
                    && libc::getppid() == parent
                    && libc::unshare(libc::CLONE_NEWUSER) == 0;
                libc::write(ready, (&raw const ok).cast(), 1);
                loop {
                    libc::pause();
                }
            }
        }
        let holder = Holder(pid);
        drop(ready_writer);
        let mut ok = [0];
        ready_reader
            .read_exact(&mut ok)
            .context("waiting for the user namespace")?;
        if ok[0] != 1 {
            bail!("could not create a user namespace for the workspace server");
        }
        let proc = format!("/proc/{}", holder.0);
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
        // SAFETY: getpid has no preconditions.
        let supervisor = unsafe { libc::getpid() };
        // SAFETY: the hook only makes async-signal-safe system calls, and the descriptor stays
        // open until after the child has started.
        unsafe {
            command.pre_exec(move || {
                if libc::setns(fd, libc::CLONE_NEWUSER) != 0
                    || libc::setgroups(0, std::ptr::null()) != 0
                    || libc::setresgid(0, 0, 0) != 0
                    || libc::setresuid(0, 0, 0) != 0
                    // Changing user cleared the death signal.
                    || libc::prctl(libc::PR_SET_PDEATHSIG, libc::SIGKILL, 0, 0, 0) != 0
                {
                    return Err(io::Error::last_os_error());
                }
                if libc::getppid() != supervisor {
                    return Err(io::Error::from(ErrorKind::Interrupted));
                }
                Ok(())
            })
        }
    }
}

/// The child that created a namespace; it is killed and reaped on drop.
struct Holder(libc::pid_t);

impl Drop for Holder {
    fn drop(&mut self) {
        // SAFETY: kill and waitpid take only integers and a null status pointer; the pid is our
        // child, not yet reaped.
        unsafe {
            libc::kill(self.0, libc::SIGKILL);
            libc::waitpid(self.0, std::ptr::null_mut(), 0);
        }
    }
}
