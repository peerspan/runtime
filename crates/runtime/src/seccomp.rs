// SPDX-License-Identifier: Apache-2.0
// Copyright The Peerspan Authors

//! The VMM's seccomp filter: an allow-list of the system calls libkrun makes, and nothing else.
//!
//! The filter is installed after the jail and before libkrun starts any thread, so every VMM
//! thread inherits it. Any other call kills the VMM, which the supervisor reports; the kernel
//! log names the call. The list is what the VMM was seen to call across `scripts/check.sh`,
//! the calls libkrun's read-only file server makes for the operations it serves, and calls the
//! C and Rust runtimes make on rarer paths. Sockets are Unix sockets only, and threads cannot
//! be cloned into new namespaces.

use std::collections::BTreeMap;

use anyhow::Context;
use seccompiler::{
    BpfProgram, SeccompAction, SeccompCmpArgLen, SeccompCmpOp, SeccompCondition, SeccompFilter,
    SeccompRule, TargetArch,
};

/// Calls the VMM may make with any arguments.
const ALLOWED: &[libc::c_long] = &[
    // Memory.
    libc::SYS_brk,
    libc::SYS_mmap,
    libc::SYS_mprotect,
    libc::SYS_munmap,
    libc::SYS_mremap,
    libc::SYS_madvise,
    libc::SYS_memfd_create,
    libc::SYS_ftruncate,
    // Threads, signals and time.
    libc::SYS_futex,
    libc::SYS_set_robust_list,
    libc::SYS_rseq,
    libc::SYS_sched_yield,
    libc::SYS_sched_getaffinity,
    libc::SYS_getpid,
    libc::SYS_gettid,
    libc::SYS_tgkill,
    libc::SYS_rt_sigaction,
    libc::SYS_rt_sigprocmask,
    libc::SYS_rt_sigreturn,
    libc::SYS_sigaltstack,
    libc::SYS_restart_syscall,
    libc::SYS_clock_gettime,
    libc::SYS_clock_getres,
    libc::SYS_clock_nanosleep,
    libc::SYS_nanosleep,
    libc::SYS_getrandom,
    libc::SYS_prctl,
    libc::SYS_capget,
    libc::SYS_getuid,
    libc::SYS_getgid,
    libc::SYS_exit,
    libc::SYS_exit_group,
    // Descriptors: KVM, eventfds, epoll, and the console log.
    libc::SYS_ioctl,
    libc::SYS_eventfd2,
    libc::SYS_epoll_create1,
    libc::SYS_epoll_ctl,
    libc::SYS_epoll_wait,
    libc::SYS_epoll_pwait,
    libc::SYS_poll,
    libc::SYS_ppoll,
    libc::SYS_read,
    libc::SYS_readv,
    libc::SYS_write,
    libc::SYS_writev,
    libc::SYS_close,
    libc::SYS_dup,
    libc::SYS_fcntl,
    // Unix sockets: vsock connections, the workspace server, and passt.
    libc::SYS_connect,
    libc::SYS_sendmsg,
    libc::SYS_recvmsg,
    libc::SYS_sendto,
    libc::SYS_recvfrom,
    libc::SYS_setsockopt,
    libc::SYS_getsockopt,
    libc::SYS_shutdown,
    // Serving the read-only root filesystem, inside the jail.
    libc::SYS_openat,
    libc::SYS_preadv,
    libc::SYS_pread64,
    libc::SYS_lseek,
    libc::SYS_getdents64,
    libc::SYS_fstat,
    libc::SYS_newfstatat,
    libc::SYS_statx,
    libc::SYS_fstatfs,
    libc::SYS_readlinkat,
    libc::SYS_fgetxattr,
    libc::SYS_lgetxattr,
    libc::SYS_flistxattr,
    libc::SYS_llistxattr,
    libc::SYS_fsync,
    libc::SYS_fdatasync,
    libc::SYS_umask,
];

/// clone flags that create namespaces. `CLONE_NEWTIME` shares its bit with the exit signal in
/// clone(2), so it cannot be checked there; it needs clone3 or unshare, which are refused.
const CLONE_NAMESPACES: u64 = (libc::CLONE_NEWNS
    | libc::CLONE_NEWCGROUP
    | libc::CLONE_NEWUTS
    | libc::CLONE_NEWIPC
    | libc::CLONE_NEWUSER
    | libc::CLONE_NEWPID
    | libc::CLONE_NEWNET) as u64;

/// Builds the filters in the order they are applied: the refusal of clone3, then the
/// allow-list, which does not allow installing more filters.
fn programs() -> anyhow::Result<[BpfProgram; 2]> {
    let arch = TargetArch::try_from(std::env::consts::ARCH)
        .map_err(|e| anyhow::anyhow!("seccomp target {}: {e}", std::env::consts::ARCH))?;
    let arg = |index, op, value| SeccompCondition::new(index, SeccompCmpArgLen::Qword, op, value);
    let unix_only = || -> anyhow::Result<Vec<SeccompRule>> {
        Ok(vec![SeccompRule::new(vec![arg(
            0,
            SeccompCmpOp::Eq,
            libc::AF_UNIX as u64,
        )?])?])
    };
    let mut rules: BTreeMap<i64, Vec<SeccompRule>> =
        ALLOWED.iter().map(|&call| (call, Vec::new())).collect();
    rules.insert(libc::SYS_socket, unix_only()?);
    rules.insert(libc::SYS_socketpair, unix_only()?);
    rules.insert(
        libc::SYS_clone,
        vec![SeccompRule::new(vec![arg(
            0,
            SeccompCmpOp::MaskedEq(CLONE_NAMESPACES),
            0,
        )?])?],
    );
    // Allowed here so that the second filter's ENOSYS, not this one's kill, decides it.
    rules.insert(libc::SYS_clone3, Vec::new());
    let allow_list = SeccompFilter::new(
        rules,
        SeccompAction::KillProcess,
        SeccompAction::Allow,
        arch,
    )
    .context("building the seccomp allow-list")?;

    // clone3 passes its flags in memory, where seccomp cannot check them. With ENOSYS, the C
    // library falls back to clone, whose flags the allow-list checks.
    let enosys = u32::try_from(libc::ENOSYS).expect("ENOSYS is positive");
    let no_clone3 = SeccompFilter::new(
        BTreeMap::from([(libc::SYS_clone3, Vec::new())]),
        SeccompAction::Allow,
        SeccompAction::Errno(enosys),
        arch,
    )
    .context("building the clone3 filter")?;

    Ok([
        no_clone3
            .try_into()
            .context("compiling the clone3 filter")?,
        allow_list
            .try_into()
            .context("compiling the seccomp allow-list")?,
    ])
}

/// Installs the filters on this thread; threads started later inherit them.
///
/// # Errors
///
/// Fails if the filters cannot be built or installed; the caller must not boot the VM then.
pub fn install() -> anyhow::Result<()> {
    apply(&programs()?)
}

fn apply(programs: &[BpfProgram]) -> anyhow::Result<()> {
    for program in programs {
        seccompiler::apply_filter(program).context("installing the seccomp filter")?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Runs `body` in a forked child under the filters and returns how the child ended: its
    /// exit code, or the signal that killed it as a negative number. The filters are built
    /// before forking, so the child only applies them and makes raw system calls.
    fn filtered(body: fn() -> i32) -> i32 {
        let programs = programs().expect("build the filters");
        // SAFETY: the child makes only system calls and exits without returning.
        let pid = unsafe { libc::fork() };
        assert!(pid >= 0, "fork failed");
        if pid == 0 {
            let code = if apply(&programs).is_ok() { body() } else { 99 };
            // SAFETY: _exit ends the child without running the parent's destructors.
            unsafe { libc::_exit(code) };
        }
        let mut status = 0;
        // SAFETY: status is a valid out-pointer and pid is our child.
        assert_eq!(unsafe { libc::waitpid(pid, &raw mut status, 0) }, pid);
        if libc::WIFSIGNALED(status) {
            -libc::WTERMSIG(status)
        } else {
            libc::WEXITSTATUS(status)
        }
    }

    fn errno() -> i32 {
        std::io::Error::last_os_error().raw_os_error().unwrap_or(0)
    }

    #[test]
    fn allowed_calls_work() {
        // SAFETY: getpid has no preconditions.
        assert_eq!(filtered(|| i32::from(unsafe { libc::getpid() } <= 0)), 0);
    }

    #[test]
    fn calls_outside_the_list_kill_the_process() {
        // SAFETY: unshare(0) would change nothing; it only probes whether the call is allowed.
        assert_eq!(filtered(|| unsafe { libc::unshare(0) }), -libc::SIGSYS);
        // SAFETY: execve with null arguments would fail; it only probes the filter.
        let exec = || unsafe { libc::execve(c"/bin/true".as_ptr(), [].as_ptr(), [].as_ptr()) };
        assert_eq!(filtered(exec), -libc::SIGSYS);
    }

    #[test]
    fn sockets_are_unix_only() {
        // SAFETY: socket has no memory preconditions.
        let unix = || i32::from(unsafe { libc::socket(libc::AF_UNIX, libc::SOCK_STREAM, 0) } < 0);
        assert_eq!(filtered(unix), 0);
        // SAFETY: as above.
        let inet = || unsafe { libc::socket(libc::AF_INET, libc::SOCK_STREAM, 0) };
        assert_eq!(filtered(inet), -libc::SIGSYS);
    }

    #[test]
    fn clone_cannot_create_namespaces_and_clone3_is_refused() {
        let clone_user = || {
            // SAFETY: with CLONE_NEWUSER the filter kills the caller before the clone happens.
            unsafe { libc::syscall(libc::SYS_clone, libc::CLONE_NEWUSER | libc::SIGCHLD, 0) as i32 }
        };
        assert_eq!(filtered(clone_user), -libc::SIGSYS);
        let clone3 = || {
            // SAFETY: the filter refuses clone3 before the kernel reads its arguments.
            let rc = unsafe { libc::syscall(libc::SYS_clone3, std::ptr::null::<u8>(), 0) };
            if rc == -1 && errno() == libc::ENOSYS {
                0
            } else {
                1
            }
        };
        assert_eq!(filtered(clone3), 0);
    }
}
