// SPDX-License-Identifier: Apache-2.0
// Copyright The Peerspan Authors

//! `peerspan-runtime`: runs agent work in microVMs.

mod jail;
mod krun;
mod launcher;
mod serve;
mod vm;

use std::fs::File;
use std::io::{self, Write};
use std::path::PathBuf;
use std::process::ExitCode;
use std::sync::atomic::AtomicBool;
use std::time::Duration;

use anyhow::Context;

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
    /// Serve the local API the host daemon uses to run tasks.
    Serve(serve::ServeArgs),
    /// Boot one microVM; started by `run`, never by hand. Does not return.
    #[command(hide = true)]
    Launch(launcher::LaunchArgs),
}

#[derive(Debug, Args)]
struct RunArgs {
    /// Host directory to mount at /workspace in the guest, writable.
    #[arg(long)]
    workspace: PathBuf,
    /// Tar archive to unpack into the workspace inside the VM; the workspace must be empty.
    #[arg(long)]
    archive: Option<PathBuf>,
    /// Directory to write `changes.tar` and `deleted.txt` into: what the command changed.
    #[arg(long)]
    results_dir: Option<PathBuf>,
    /// Stop the command after this many seconds.
    #[arg(long)]
    timeout_seconds: Option<u64>,
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
        Commands::Run(args) => run(args),
        Commands::Serve(args) => serve::serve(args).map(|()| 0),
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

/// Exit code for a run that stopped at its timeout, as `timeout(1)` uses.
const EXIT_TIMED_OUT: i32 = 124;
/// Exit code for a run whose command never ran, as shells use for a missing command.
const EXIT_NOT_RUN: i32 = 127;

/// Runs one command, relaying its output, and returns the exit code to leave with.
fn run(args: RunArgs) -> anyhow::Result<i32> {
    let collect_changes = args.results_dir.is_some();
    let mut results = match &args.results_dir {
        Some(dir) => {
            let create = |name: &str| {
                File::create(dir.join(name)).with_context(|| format!("creating {name}"))
            };
            Some((create("changes.tar")?, create("deleted.txt")?))
        }
        None => None,
    };
    let spec = vm::VmSpec {
        workspace: args.workspace,
        archive: args.archive,
        guest: args.guest,
        libkrun: args.libkrun,
        virtiofsd: args.virtiofsd,
        cpus: args.cpus,
        memory_mib: args.memory_mib,
        timeout: args.timeout_seconds.map(Duration::from_secs),
        console_log: args.console_log,
        command: peerspan_runtime_protocol::Command {
            argv: args.command,
            cwd: args.cwd,
            env: args.env,
            archive: false,
            collect_changes,
        },
    };
    let mut stdout = io::stdout().lock();
    let mut stderr = io::stderr().lock();
    let outcome = vm::run(&spec, &AtomicBool::new(false), &mut |event| match event {
        vm::Event::Started => Ok(()),
        vm::Event::Stdout(data) => stdout.write_all(data).and_then(|()| stdout.flush()),
        vm::Event::Stderr(data) => stderr.write_all(data).and_then(|()| stderr.flush()),
        vm::Event::Changes(data) => results
            .as_mut()
            .map_or(Ok(()), |(tar, _)| tar.write_all(data)),
        vm::Event::Deleted(data) => results
            .as_mut()
            .map_or(Ok(()), |(_, txt)| txt.write_all(data)),
    })?;
    if let Some(error) = &outcome.error {
        eprintln!("peerspan-runtime: {error}");
        return Ok(EXIT_NOT_RUN);
    }
    Ok(match outcome {
        vm::Outcome {
            timed_out: true, ..
        } => EXIT_TIMED_OUT,
        vm::Outcome {
            exit_code: Some(code),
            ..
        } => code,
        vm::Outcome {
            signal: Some(signal),
            ..
        } => 128 + signal,
        _ => EXIT_NOT_RUN,
    })
}
