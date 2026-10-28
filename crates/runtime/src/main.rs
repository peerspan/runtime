// SPDX-License-Identifier: Apache-2.0
// Copyright The Peerspan Authors

//! `peerspan-runtime`: runs agent work in microVMs.

mod caplog;
mod cgroup;
mod jail;
mod krun;
mod launcher;
mod net;
mod phases;
mod seccomp;
mod serve;
mod userns;
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
    /// Extra kernel command-line arguments, for experiments such as `initcall_debug`.
    #[arg(long)]
    kernel_args: Option<String>,
    /// Write the run's boot time and memory measurements to this file, as JSON.
    #[arg(long)]
    metrics: Option<PathBuf>,
    /// cgroup v2 directory to create the run's cgroup in.
    #[arg(long, default_value = cgroup::DEFAULT_ROOT)]
    cgroup_root: PathBuf,
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
    /// What the guest may reach.
    #[arg(long, value_enum, default_value_t = net::Level::Internet)]
    network: net::Level,
    /// passt binary that carries the guest's network traffic [default: passt beside this
    /// binary, from scripts/build-passt.sh].
    #[arg(long, env = "PEERSPAN_PASST")]
    passt: Option<PathBuf>,
    /// Virtual CPUs.
    #[arg(long, default_value_t = 2)]
    cpus: u8,
    /// Guest memory in MiB.
    #[arg(long, default_value_t = 1024)]
    memory_mib: u32,
    /// Disk bandwidth for the run's processes in MiB/s, for reading and for writing each.
    #[arg(long, default_value_t = crate::cgroup::DEFAULT_IO_MIBPS,
          value_parser = clap::value_parser!(u32).range(1..))]
    io_mibps: u32,
    /// Disk operations per second for the run's processes, for reading and for writing each.
    #[arg(long, default_value_t = crate::cgroup::DEFAULT_IO_IOPS,
          value_parser = clap::value_parser!(u32).range(1..))]
    io_iops: u32,
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
        own_workspace: false,
        archive: args.archive,
        guest: args.guest,
        libkrun: args.libkrun,
        virtiofsd: args.virtiofsd,
        passt: args.passt.unwrap_or_else(net::bundled_passt),
        network: args.network,
        cpus: args.cpus,
        memory_mib: args.memory_mib,
        io: crate::cgroup::IoRate {
            mib_per_second: args.io_mibps,
            ops_per_second: args.io_iops,
        },
        timeout: args.timeout_seconds.map(Duration::from_secs),
        console_log: args.console_log,
        cgroup_root: args.cgroup_root,
        kernel_args: args.kernel_args,
        command: peerspan_runtime_protocol::Command {
            argv: args.command,
            cwd: args.cwd,
            env: args.env,
            archive: false,
            collect_changes,
            network: false,
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
    if let Some(path) = &args.metrics {
        let json = metrics_json(&outcome.metrics).to_string() + "\n";
        std::fs::write(path, json).with_context(|| format!("writing {}", path.display()))?;
    }
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

/// The run's measurements as JSON, with durations in milliseconds.
fn metrics_json(metrics: &vm::Metrics) -> serde_json::Value {
    let millis = |d: Duration| (d.as_secs_f64() * 10_000.0).round() / 10.0;
    let timeline = |phases: &[(String, Duration)]| {
        phases
            .iter()
            .map(|(name, at)| serde_json::json!([name, millis(*at)]))
            .collect::<Vec<_>>()
    };
    serde_json::json!({
        "connect_ms": millis(metrics.connect),
        "start_ms": metrics.start.map(millis),
        "total_ms": millis(metrics.total),
        "memory_peak_bytes": metrics.memory_peak_bytes,
        "phases_ms": timeline(&metrics.phases),
        "guest_phases_ms": timeline(&metrics.guest_phases),
    })
}
