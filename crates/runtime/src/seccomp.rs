// SPDX-License-Identifier: Apache-2.0
// Copyright The Peerspan Authors

//! The VMM's seccomp filter: a deny-list of system calls a confined VMM never needs.
//!
//! The filter is installed after the jail and before libkrun starts any thread, so every VMM
//! thread inherits it. Denied calls fail with `EPERM`. An allow-list derived from libkrun's
//! actual calls would be tighter; this list removes the calls that would let a compromised VMM
//! escape or reach the kernel's riskier interfaces.

use std::collections::BTreeMap;

use anyhow::Context;
use seccompiler::{BpfProgram, SeccompAction, SeccompFilter, TargetArch};

/// Calls a VMM process never needs once it is running.
const DENIED: &[libc::c_long] = &[
    // Running other programs.
    libc::SYS_execve,
    libc::SYS_execveat,
    // Changing what the process can see or who it is.
    libc::SYS_mount,
    libc::SYS_umount2,
    libc::SYS_mount_setattr,
    libc::SYS_move_mount,
    libc::SYS_open_tree,
    libc::SYS_fsopen,
    libc::SYS_fsconfig,
    libc::SYS_fsmount,
    libc::SYS_fspick,
    libc::SYS_pivot_root,
    libc::SYS_chroot,
    libc::SYS_unshare,
    libc::SYS_setns,
    libc::SYS_open_by_handle_at,
    libc::SYS_name_to_handle_at,
    // Reaching into other processes.
    libc::SYS_ptrace,
    libc::SYS_process_vm_readv,
    libc::SYS_process_vm_writev,
    libc::SYS_pidfd_getfd,
    // The kernel's riskier interfaces.
    libc::SYS_bpf,
    libc::SYS_perf_event_open,
    libc::SYS_userfaultfd,
    libc::SYS_keyctl,
    libc::SYS_add_key,
    libc::SYS_request_key,
    libc::SYS_init_module,
    libc::SYS_finit_module,
    libc::SYS_delete_module,
    libc::SYS_kexec_load,
    libc::SYS_kexec_file_load,
    libc::SYS_fanotify_init,
    libc::SYS_personality,
    // Host-wide state.
    libc::SYS_reboot,
    libc::SYS_swapon,
    libc::SYS_swapoff,
    libc::SYS_acct,
    libc::SYS_quotactl,
    libc::SYS_syslog,
    libc::SYS_sethostname,
    libc::SYS_setdomainname,
    libc::SYS_settimeofday,
    libc::SYS_clock_settime,
    libc::SYS_clock_adjtime,
    libc::SYS_adjtimex,
    #[cfg(target_arch = "x86_64")]
    libc::SYS_iopl,
    #[cfg(target_arch = "x86_64")]
    libc::SYS_ioperm,
];

/// Installs the filter on this thread; threads started later inherit it.
///
/// # Errors
///
/// Fails if the filter cannot be built or installed; the caller must not boot the VM then.
pub fn install() -> anyhow::Result<()> {
    let rules = DENIED
        .iter()
        .map(|&call| (call, Vec::new()))
        .collect::<BTreeMap<_, _>>();
    let arch = TargetArch::try_from(std::env::consts::ARCH)
        .map_err(|e| anyhow::anyhow!("seccomp target {}: {e}", std::env::consts::ARCH))?;
    let eperm = u32::try_from(libc::EPERM).expect("EPERM is positive");
    let filter = SeccompFilter::new(
        rules,
        SeccompAction::Allow,
        SeccompAction::Errno(eperm),
        arch,
    )
    .context("building the seccomp filter")?;
    let program: BpfProgram = filter.try_into().context("compiling the seccomp filter")?;
    seccompiler::apply_filter(&program).context("installing the seccomp filter")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn errno() -> i32 {
        std::io::Error::last_os_error().raw_os_error().unwrap_or(0)
    }

    #[test]
    fn denied_calls_fail_with_eperm_and_others_work() {
        // A filter applies to the thread that installs it, so the test harness is unaffected.
        std::thread::spawn(|| {
            install().expect("install the filter");
            // SAFETY: unshare(0) changes nothing; it only probes whether the call is allowed.
            let rc = unsafe { libc::unshare(0) };
            assert_eq!((rc, errno()), (-1, libc::EPERM), "unshare");
            // SAFETY: this personality value only queries the current personality.
            let rc = unsafe { libc::personality(0xffff_ffff) };
            assert_eq!((rc, errno()), (-1, libc::EPERM), "personality");
            let exec = std::process::Command::new("/bin/true").status();
            assert!(exec.is_err(), "execve was allowed: {exec:?}");
            // SAFETY: getpid has no preconditions.
            assert!(unsafe { libc::getpid() } > 0, "an allowed call failed");
        })
        .join()
        .expect("filtered thread panicked");
    }
}
