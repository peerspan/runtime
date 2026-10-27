// SPDX-License-Identifier: Apache-2.0
// Copyright The Peerspan Authors

//! `peerspan-runtime`: runs agent work in microVMs.

mod jail;
mod krun;
mod launcher;
mod supervisor;

use std::path::PathBuf;
use std::process::ExitCode;

use clap::{Args, Parser, Subcommand};

#[derive(Debug, Parser)]
#[command(
    version,
    about = "Peerspan's runtime: lightweight VMs for agent collaboration"
)]
struct Cli {
    #[command(subcommand)]
    command: Commands,
}

#[derive(Debug, Subcommand)]
enum Commands {
    /// Run a command in a new microVM with a workspace attached, and exit with its code.
    Run(RunArgs),
    /// Boot one microVM; started by `run`, never by hand. Does not return.
    #[command(hide = true)]
    Launch(launcher::LaunchArgs),
}

#[derive(Debug, Args)]
struct RunArgs {
    /// Host directory to mount at /workspace in the guest, writable.
    #[arg(long)]
    workspace: PathBuf,
    /// Guest image directory holding `vmlinux`, `initramfs.cpio.gz` and `rootfs/`.
    #[arg(long, env = "PEERSPAN_RUNTIME_GUEST")]
    guest: PathBuf,
    /// libkrun shared library to load.
    #[arg(long, env = "PEERSPAN_LIBKRUN", default_value = "libkrun.so.2")]
    libkrun: PathBuf,
    /// virtiofsd binary that serves the workspace.
    #[arg(
        long,
        env = "PEERSPAN_VIRTIOFSD",
        default_value = "/usr/libexec/virtiofsd"
    )]
    virtiofsd: PathBuf,
    /// Virtual CPUs.
    #[arg(long, default_value_t = 2)]
    cpus: u8,
    /// Guest memory in MiB.
    #[arg(long, default_value_t = 1024)]
    memory_mib: u32,
    /// Where to keep the guest console; discarded with the run directory when omitted.
    #[arg(long)]
    console_log: Option<PathBuf>,
    /// Working directory inside the guest.
    #[arg(long, default_value = peerspan_runtime_protocol::WORKSPACE_PATH)]
    cwd: String,
    /// Environment variables for the command, as NAME=VALUE.
    #[arg(long = "env", value_name = "NAME=VALUE", value_parser = parse_env)]
    env: Vec<(String, String)>,
    /// The command and its arguments.
    #[arg(last = true, required = true)]
    command: Vec<String>,
}

fn parse_env(s: &str) -> Result<(String, String), String> {
    s.split_once('=')
        .map(|(name, value)| (name.to_owned(), value.to_owned()))
        .ok_or_else(|| format!("expected NAME=VALUE, got {s:?}"))
}

fn main() -> ExitCode {
    let cli = Cli::parse();
    let result = match cli.command {
        Commands::Run(args) => supervisor::run(args.into()),
        Commands::Launch(args) => launcher::launch(&args).map(|never| match never {}),
    };
    match result {
        Ok(code) => ExitCode::from(u8::try_from(code).unwrap_or(1)),
        Err(e) => {
            eprintln!("peerspan-runtime: {e:#}");
            ExitCode::from(125)
        }
    }
}

impl From<RunArgs> for supervisor::RunOptions {
    fn from(args: RunArgs) -> Self {
        supervisor::RunOptions {
            workspace: args.workspace,
            guest: args.guest,
            libkrun: args.libkrun,
            virtiofsd: args.virtiofsd,
            cpus: args.cpus,
            memory_mib: args.memory_mib,
            console_log: args.console_log,
            command: peerspan_runtime_protocol::Command {
                argv: args.command,
                cwd: args.cwd,
                env: args.env,
            },
        }
    }
}
