// SPDX-License-Identifier: Apache-2.0
// Copyright The Peerspan Authors

//! `peerspan-runtime`: runs agent work in microVMs.

mod bundle;
mod caplog;
mod cgroup;
mod disk;
mod doctor;
mod image;
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
use std::os::fd::AsFd;
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
    Run(Box<RunArgs>),
    /// Serve the local API the host daemon uses to run tasks.
    Serve(serve::ServeArgs),
    /// Pull an OCI image, or update a tag to what it names now.
    Pull(PullArgs),
    /// Check that this host can run VMs, and say what to fix if it cannot.
    Doctor(doctor::DoctorArgs),
    /// List the images pulled here.
    Images(ImagesArgs),
    /// Remove images no tag names, or, with --unused-days, those no run has booted lately.
    Prune(PruneArgs),
    /// Boot one microVM; started by `run`, never by hand. Does not return.
    #[command(hide = true)]
    Launch(launcher::LaunchArgs),
    /// Unpack an image's layers into a new root; started by `pull`, never by hand.
    #[command(hide = true)]
    UnpackLayers(UnpackLayersArgs),
}

#[derive(Debug, Args)]
struct PullArgs {
    /// The image, such as `node:22` or `ghcr.io/org/image@sha256:...`.
    image: String,
    /// Where images are kept.
    #[arg(long, default_value = image::DEFAULT_CACHE)]
    image_cache: PathBuf,
}

#[derive(Debug, Args)]
struct ImagesArgs {
    /// Where images are kept.
    #[arg(long, default_value = image::DEFAULT_CACHE)]
    image_cache: PathBuf,
}

#[derive(Debug, Args)]
struct PruneArgs {
    /// Also remove images no run has booted for this many days, and their tags.
    #[arg(long)]
    unused_days: Option<u64>,
    /// Where images are kept.
    #[arg(long, default_value = image::DEFAULT_CACHE)]
    image_cache: PathBuf,
}

#[derive(Debug, Args)]
struct UnpackLayersArgs {
    #[arg(long)]
    root: PathBuf,
    /// A layer as `gzip:PATH` or `tar:PATH`, applied in the order given.
    #[arg(long)]
    layer: Vec<String>,
}

#[derive(Debug, Args)]
struct RunArgs {
    /// Host directory to mount at /workspace in the guest, writable.
    #[arg(long)]
    workspace: PathBuf,
    /// OCI image to boot, such as `node:22`; pulled the first time [default: the guest image's
    /// own root, a minimal Alpine].
    #[arg(long)]
    image: Option<String>,
    /// Pull the image again even if it is already here, to update its tag.
    #[arg(long)]
    pull: bool,
    /// Where images are kept.
    #[arg(long, default_value = image::DEFAULT_CACHE)]
    image_cache: PathBuf,
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
    /// Guest image directory holding `vmlinux`, `initramfs.cpio.gz` and `rootfs/` [default:
    /// `guest/` beside this binary].
    #[arg(long, env = "PEERSPAN_RUNTIME_GUEST")]
    guest: Option<PathBuf>,
    /// libkrun shared library to load [default: `libkrun.so.2` beside this binary, or the
    /// dynamic loader's].
    #[arg(long, env = "PEERSPAN_LIBKRUN")]
    libkrun: Option<PathBuf>,
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
    /// Pass this program's standard input to the command; without it the command's is empty.
    #[arg(long)]
    stdin: bool,
    /// Send the command's standard error to its standard output, as `2>&1` does, which keeps
    /// the order in which it wrote to the two.
    #[arg(long)]
    combined_output: bool,
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
        Commands::Run(args) => run(*args),
        Commands::Serve(args) => serve::serve(args).map(|()| 0),
        Commands::Launch(args) => launcher::launch(&args).map(|never| match never {}),
        Commands::Pull(args) => image::Reference::parse(&args.image)
            .and_then(|reference| image::pull(&args.image_cache, &reference))
            .map(|digest| {
                println!("{digest}");
                0
            }),
        Commands::UnpackLayers(args) => unpack_layers(&args).map(|()| 0),
        Commands::Doctor(args) => Ok(if doctor::doctor(&args) { 0 } else { 1 }),
        Commands::Images(args) => images(&args).map(|()| 0),
        Commands::Prune(args) => prune(&args).map(|()| 0),
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

fn images(args: &ImagesArgs) -> anyhow::Result<()> {
    let now = std::time::SystemTime::now();
    for cached in image::list(&args.image_cache)? {
        let days = now
            .duration_since(cached.last_used)
            .map_or(0, |d| d.as_secs() / 86_400);
        let tags = if cached.tags.is_empty() {
            "(untagged)".to_owned()
        } else {
            cached.tags.join(", ")
        };
        println!(
            "{}  {:>6} MiB  used {days}d ago  {tags}",
            cached.digest,
            cached.bytes >> 20
        );
    }
    Ok(())
}

fn prune(args: &PruneArgs) -> anyhow::Result<()> {
    let unused_for = args
        .unused_days
        .map(|days| Duration::from_secs(days.saturating_mul(86_400)));
    let removed = image::prune(&args.image_cache, unused_for)?;
    let bytes: u64 = removed.iter().map(|i| i.bytes).sum();
    for image in &removed {
        println!("removed {}", image.digest);
    }
    println!("freed {} MiB", bytes >> 20);
    Ok(())
}

fn unpack_layers(args: &UnpackLayersArgs) -> anyhow::Result<()> {
    let layers = args
        .layer
        .iter()
        .map(|layer| match layer.split_once(':') {
            Some(("gzip", path)) => Ok((PathBuf::from(path), true)),
            Some(("tar", path)) => Ok((PathBuf::from(path), false)),
            _ => anyhow::bail!("--layer takes gzip:PATH or tar:PATH, not {layer}"),
        })
        .collect::<anyhow::Result<Vec<_>>>()?;
    image::unpack_layers(&args.root, &layers)
}

/// Runs one command, relaying its output, and returns the exit code to leave with.
fn run(args: RunArgs) -> anyhow::Result<i32> {
    let image = match &args.image {
        Some(name) => Some(image::get(
            &args.image_cache,
            &image::Reference::parse(name)?,
            args.pull,
        )?),
        None => None,
    };
    // The image stays held, and so kept from `prune`, until the run ends.
    let (rootfs, image_env) = image.as_ref().map_or((None, Vec::new()), |i| {
        (Some(i.rootfs.clone()), i.env.clone())
    });
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
        rootfs,
        stdin: if args.stdin {
            Some(File::from(io::stdin().as_fd().try_clone_to_owned()?))
        } else {
            None
        },
        archive: args.archive,
        guest: match args.guest {
            Some(guest) => guest,
            None => crate::bundle::guest()?,
        },
        libkrun: args.libkrun.unwrap_or_else(crate::bundle::libkrun),
        virtiofsd: args.virtiofsd,
        passt: args.passt.unwrap_or_else(crate::bundle::passt),
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
            env: image::merge_env(image_env, args.env),
            archive: false,
            collect_changes,
            network: false,
            stdin: false,
            combined_output: args.combined_output,
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
