// SPDX-License-Identifier: Apache-2.0
// Copyright The Peerspan Authors

//! The per-VM child process: boots one microVM and never returns.

use std::convert::Infallible;
use std::path::{Path, PathBuf};

use anyhow::Context;
use clap::Args;
use peerspan_runtime_protocol::{AGENT_PORT, ROOTFS_TAG, WORKSPACE_TAG};

use crate::krun::{self, VmConfig};

/// Kernel command line for runtime guests. `reboot=k` makes the guest's reboot end the VM.
pub const KERNEL_CMDLINE: &str = "console=hvc0 rdinit=/init panic=-1 reboot=k quiet";

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
    #[arg(long)]
    pub workspace_socket: PathBuf,
    #[arg(long)]
    pub agent_socket: PathBuf,
    #[arg(long)]
    pub cpus: u8,
    #[arg(long)]
    pub memory_mib: u32,
}

/// Boots the VM described by `args`.
///
/// # Errors
///
/// Fails before the VM starts, if a path is not UTF-8 or libkrun rejects the configuration.
pub fn launch(args: &LaunchArgs) -> anyhow::Result<Infallible> {
    let config = VmConfig {
        cpus: args.cpus,
        memory_mib: args.memory_mib,
        kernel: utf8(&args.kernel)?,
        initrd: utf8(&args.initrd)?,
        cmdline: KERNEL_CMDLINE,
        rootfs_tag: ROOTFS_TAG,
        rootfs_dir: utf8(&args.rootfs)?,
        workspace_tag: WORKSPACE_TAG,
        workspace_socket: utf8(&args.workspace_socket)?,
        agent_port: AGENT_PORT,
        agent_socket: utf8(&args.agent_socket)?,
    };
    krun::boot(&args.libkrun, &config)
}

fn utf8(path: &Path) -> anyhow::Result<&str> {
    path.to_str()
        .with_context(|| format!("path is not UTF-8: {}", path.display()))
}
