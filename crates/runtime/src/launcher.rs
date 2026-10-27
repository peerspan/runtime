// SPDX-License-Identifier: Apache-2.0
// Copyright The Peerspan Authors

//! The per-VM child process: confines itself, installs its seccomp filter, boots one microVM
//! and never returns.

use std::convert::Infallible;
use std::path::PathBuf;

use clap::Args;
use peerspan_runtime_protocol::{AGENT_PORT, ROOTFS_TAG, WORKSPACE_TAG};

use crate::jail::{self, Jail};
use crate::krun::{Libkrun, VmConfig};
use crate::phases::PhaseWriter;
use crate::seccomp;

/// Kernel command line for runtime guests. `reboot=k` makes the guest's reboot end the VM.
pub const KERNEL_CMDLINE: &str = "console=hvc0 rdinit=/init panic=-1 reboot=k quiet";

/// Socket names inside the sockets directory.
pub const WORKSPACE_SOCKET: &str = "workspace.sock";
pub const AGENT_SOCKET: &str = "agent.sock";

#[derive(Debug, Args)]
pub struct LaunchArgs {
    #[arg(long)]
    pub libkrun: PathBuf,
    #[arg(long)]
    pub kernel: PathBuf,
    #[arg(long)]
    pub initrd: PathBuf,
    #[arg(long)]
    pub rootfs: PathBuf,
    /// Directory holding the workspace and agent sockets.
    #[arg(long)]
    pub sockets: PathBuf,
    /// Empty directory to build the jail in.
    #[arg(long)]
    pub jail: PathBuf,
    #[arg(long)]
    pub uid: u32,
    #[arg(long)]
    pub gid: u32,
    #[arg(long)]
    pub cpus: u8,
    #[arg(long)]
    pub memory_mib: u32,
    /// Extra kernel command-line arguments, for experiments.
    #[arg(long)]
    pub kernel_args: Option<String>,
    /// Descriptor to write step timestamps to, for measurement.
    #[arg(long)]
    pub phases_fd: Option<i32>,
    /// Connected stream socket to the guest's network backend (passt); no network without it.
    #[arg(long)]
    pub net_fd: Option<i32>,
}

/// Confines this process and boots the VM described by `args`.
///
/// # Errors
///
/// Fails before the VM starts, if libkrun cannot be loaded, confinement fails, or libkrun
/// rejects the configuration.
pub fn launch(args: &LaunchArgs) -> anyhow::Result<Infallible> {
    // SAFETY: the supervisor passes a descriptor it opened for this process alone.
    let mut phases = unsafe { PhaseWriter::from_fd(args.phases_fd) };
    phases.mark("launcher");
    let libkrun = Libkrun::load(&args.libkrun)?;
    phases.mark("libkrun_loaded");
    Jail {
        dir: &args.jail,
        rootfs: &args.rootfs,
        kernel: &args.kernel,
        initrd: &args.initrd,
        sockets: &args.sockets,
        uid: args.uid,
        gid: args.gid,
    }
    .enter()?;
    phases.mark("jailed");
    seccomp::install()?;
    phases.mark("seccomp");
    let cmdline = match &args.kernel_args {
        Some(extra) => format!("{KERNEL_CMDLINE} {extra}"),
        None => KERNEL_CMDLINE.to_owned(),
    };
    let workspace_socket = format!("{}/{WORKSPACE_SOCKET}", jail::SOCKETS);
    let agent_socket = format!("{}/{AGENT_SOCKET}", jail::SOCKETS);
    libkrun.boot(
        &VmConfig {
            cpus: args.cpus,
            memory_mib: args.memory_mib,
            kernel: jail::KERNEL,
            initrd: jail::INITRD,
            cmdline: &cmdline,
            rootfs_tag: ROOTFS_TAG,
            rootfs_dir: jail::ROOTFS,
            workspace_tag: WORKSPACE_TAG,
            workspace_socket: &workspace_socket,
            agent_port: AGENT_PORT,
            agent_socket: &agent_socket,
            net_fd: args.net_fd,
        },
        &mut || phases.mark("vm_built"),
    )
}
