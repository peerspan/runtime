// SPDX-License-Identifier: Apache-2.0
// Copyright The Peerspan Authors

//! `peerspan-runtime serve`: the local API the host daemon drives, as the runtime task contract
//! in peerspan-internal (`docs/runtime-task-contract.md`, "The runtime's local API") specifies.
//!
//! Runs live in memory; a restarted runtime knows none of them, and the daemon fails their tasks.

use std::collections::{BTreeMap, HashMap};
use std::fs::{self, File};
use std::io::{self, Read, Write};
use std::os::unix::fs::{DirBuilderExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Condvar, Mutex};
use std::thread;
use std::time::{Duration, Instant, SystemTime};

use anyhow::Context;
use base64::Engine;
use base64::engine::general_purpose::STANDARD as BASE64;
use clap::Args;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use tiny_http::{Header, Method, Request, Response, Server};

use crate::disk::WorkspaceDisk;
use crate::net::Level;
use crate::vm::{self, Event, Outcome, VmSpec};

/// Largest request body accepted, as on the daemon's `admin.sock`.
const MAX_BODY: u64 = 1 << 20;
/// Largest output chunk in one event, before base64.
const CHUNK: usize = 16 * 1024;
/// The largest artifact the daemon accepts. Each output log is cut there, and changes larger
/// than it are left out, since the daemon would drop them.
const ARTIFACT_LIMIT: u64 = 100 * 1024 * 1024;
/// Longest wait a client may ask for on the events route.
const MAX_WAIT: Duration = Duration::from_secs(25);

#[derive(Debug, Args)]
pub struct ServeArgs {
    /// Unix socket to serve on; replaced if it exists.
    #[arg(long)]
    socket: PathBuf,
    /// Guest image directory holding `vmlinux`, `initramfs.cpio.gz` and `rootfs/` [default:
    /// `guest/` beside this binary].
    #[arg(long, env = "PEERSPAN_RUNTIME_GUEST")]
    guest: Option<PathBuf>,
    /// OCI image every run boots, such as `node:22`, pulled at startup if it is not here
    /// [default: the guest image's own root, a minimal Alpine].
    #[arg(long)]
    image: Option<String>,
    /// Where images are kept.
    #[arg(long, default_value = crate::image::DEFAULT_CACHE)]
    image_cache: PathBuf,
    /// libkrun shared library to load [default: `libkrun.so.2` beside this binary, or the
    /// dynamic loader's].
    #[arg(long, env = "PEERSPAN_LIBKRUN")]
    libkrun: Option<PathBuf>,
    /// virtiofsd binary that serves workspaces.
    #[arg(
        long,
        env = "PEERSPAN_VIRTIOFSD",
        default_value = "/usr/libexec/virtiofsd"
    )]
    virtiofsd: PathBuf,
    /// passt binary that carries guests' network traffic [default: passt beside this binary,
    /// from scripts/build-passt.sh].
    #[arg(long, env = "PEERSPAN_PASST")]
    passt: Option<PathBuf>,
    /// The most network a run may ask for; the host owner's ceiling.
    #[arg(long, value_enum, default_value_t = Level::Internet)]
    max_network: Level,
    /// Directory for runs' workspaces and results.
    #[arg(long, default_value = "/var/lib/peerspan-runtime")]
    state: PathBuf,
    /// cgroup v2 directory to create each run's cgroup in.
    #[arg(long, default_value = crate::cgroup::DEFAULT_ROOT)]
    cgroup_root: PathBuf,
    /// The user the daemon runs as: it owns the socket and the result files.
    #[arg(long, default_value_t = 0)]
    client_uid: u32,
    #[arg(long, default_value_t = 4)]
    max_cpus: u8,
    #[arg(long, default_value_t = 4096)]
    max_memory_mib: u32,
    /// Disk bandwidth for each run's processes in MiB/s, for reading and for writing each.
    #[arg(long, default_value_t = crate::cgroup::DEFAULT_IO_MIBPS,
          value_parser = clap::value_parser!(u32).range(1..))]
    io_mibps: u32,
    /// Disk operations per second for each run's processes, for reading and for writing each.
    #[arg(long, default_value_t = crate::cgroup::DEFAULT_IO_IOPS,
          value_parser = clap::value_parser!(u32).range(1..))]
    io_iops: u32,
    #[arg(long, default_value_t = 3600)]
    max_seconds: u32,
    /// Disk space each run's workspace may take, in MiB.
    #[arg(long, default_value_t = crate::disk::DEFAULT_SIZE_MIB,
          value_parser = clap::value_parser!(u32).range(64..))]
    workspace_mib: u32,
    #[arg(long, default_value_t = 1)]
    max_concurrent_runs: usize,
}

/// `POST /v1/runs`.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
struct RunRequest {
    run_id: String,
    argv: Vec<String>,
    cwd: String,
    env: BTreeMap<String, String>,
    resources: Resources,
    workspace_archive: Option<PathBuf>,
    results: Results,
    /// What the guest may reach; none when the request does not say.
    #[serde(default = "no_network")]
    network: Level,
}

fn no_network() -> Level {
    Level::None
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
struct Resources {
    cpus: u8,
    memory_mib: u32,
    timeout_seconds: u32,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "lowercase")]
enum Results {
    None,
    Output,
    Changes,
}

struct Runtime {
    args: ServeArgs,
    guest: PathBuf,
    libkrun: PathBuf,
    /// The image every run boots, resolved at startup.
    image: Option<crate::image::Image>,
    runs: Mutex<HashMap<String, Arc<Run>>>,
}

struct Run {
    request: RunRequest,
    created_at: String,
    dir: PathBuf,
    cancel: AtomicBool,
    deleted: AtomicBool,
    log: Mutex<Log>,
    changed: Condvar,
}

#[derive(Default)]
struct Log {
    events: Vec<Value>,
    finished: bool,
}

impl Run {
    fn push(&self, mut event: Value) {
        let mut log = self.log.lock().expect("run log lock poisoned");
        event["sequence"] = json!(log.events.len() + 1);
        log.events.push(event);
        self.changed.notify_all();
    }

    fn record(&self) -> Value {
        let finished = self.log.lock().expect("run log lock poisoned").finished;
        json!({
            "run_id": self.request.run_id,
            "state": if finished { "exited" } else { "running" },
            "created_at": self.created_at,
        })
    }
}

/// Serves until the process is stopped.
///
/// # Errors
///
/// Fails if the state directory or the socket cannot be set up.
pub fn serve(args: ServeArgs) -> anyhow::Result<()> {
    crate::log::structured();
    if !rustix::process::geteuid().is_root() {
        anyhow::bail!("peerspan-runtime must run as root to confine its VMs");
    }
    for dir in [args.state.clone(), args.state.join("runs")] {
        fs::DirBuilder::new()
            .recursive(true)
            .mode(0o711)
            .create(&dir)
            .with_context(|| format!("creating {}", dir.display()))?;
    }
    reachable_by_runs(&args.state)?;
    let _state_lock = lock_state(&args.state)?;
    remove_earlier_runs(&args.state.join("runs"));
    // Their processes, cgroups and network rules, now rather than at the next run's start.
    vm::remove_stale_runs(&args.cgroup_root);
    let _ = fs::remove_file(&args.socket);
    let server = Server::http_unix(&args.socket)
        .map_err(|e| anyhow::anyhow!("serving on {}: {e}", args.socket.display()))?;
    fs::set_permissions(&args.socket, fs::Permissions::from_mode(0o600))?;
    std::os::unix::fs::chown(&args.socket, Some(args.client_uid), None)?;

    let image = match &args.image {
        Some(name) => Some(crate::image::get(
            &args.image_cache,
            &crate::image::Reference::parse(name)?,
            false,
        )?),
        None => None,
    };
    let guest = match &args.guest {
        Some(guest) => guest.clone(),
        None => crate::bundle::guest()?,
    };
    let libkrun = args.libkrun.clone().unwrap_or_else(crate::bundle::libkrun);
    let image_name = args.image.as_deref().unwrap_or("bundled");
    crate::log::info(
        "serving",
        &[
            ("version", &env!("CARGO_PKG_VERSION")),
            ("socket", &args.socket.display()),
            ("state", &args.state.display()),
            ("guest", &guest.display()),
            ("image", &image_name),
            ("max_cpus", &args.max_cpus),
            ("max_memory_mib", &args.max_memory_mib),
            ("max_seconds", &args.max_seconds),
            ("max_network", &args.max_network),
            ("max_concurrent_runs", &args.max_concurrent_runs),
            ("workspace_mib", &args.workspace_mib),
            ("io_mibps", &args.io_mibps),
            ("io_iops", &args.io_iops),
        ],
    );
    let runtime = Arc::new(Runtime {
        args,
        guest,
        libkrun,
        image,
        runs: Mutex::new(HashMap::new()),
    });
    for request in server.incoming_requests() {
        let runtime = Arc::clone(&runtime);
        thread::spawn(move || handle(&runtime, request));
    }
    Ok(())
}

fn handle(runtime: &Arc<Runtime>, mut request: Request) {
    let url = request.url().to_owned();
    let (path, query) = url.split_once('?').unwrap_or((&url, ""));
    let segments: Vec<&str> = path.trim_matches('/').split('/').collect();
    let (status, body) = match (request.method(), segments.as_slice()) {
        (Method::Get, ["v1", "runtime"]) => (200, runtime_info(runtime)),
        (Method::Post, ["v1", "runs"]) => match read_body(&mut request) {
            Ok(body) => {
                let (status, record) = start(runtime, &body);
                if status >= 400 {
                    // The id as sent, if the request parsed that far and it has an id's form.
                    let id = serde_json::from_slice::<Value>(&body)
                        .ok()
                        .and_then(|v| v.get("run_id").and_then(Value::as_str).map(str::to_owned))
                        .filter(|id| is_run_id(id))
                        .unwrap_or_default();
                    // An invalid request's reason can quote what it was sent, such as a
                    // malformed environment, so only the other refusals', which are fixed
                    // texts, go in the log.
                    let reason = if status == 400 {
                        "the request is invalid".to_owned()
                    } else {
                        record["error"].as_str().unwrap_or_default().to_owned()
                    };
                    crate::log::info(
                        "run refused",
                        &[("run", &id), ("status", &status), ("reason", &reason)],
                    );
                }
                (status, record)
            }
            Err(e) => error(400, "invalid", &e),
        },
        (Method::Get, ["v1", "runs", id, "events"]) => events(runtime, id, query),
        (Method::Post, ["v1", "runs", id, "cancel"]) => match find(runtime, id) {
            Some(run) => {
                run.cancel.store(true, Ordering::Relaxed);
                crate::log::info("run cancelled", &[("run", &run.request.run_id)]);
                (200, run.record())
            }
            None => not_found(),
        },
        (Method::Delete, ["v1", "runs", id]) => delete(runtime, id),
        _ => not_found(),
    };
    let response = Response::from_data(body.to_string())
        .with_status_code(status)
        .with_header(Header::from_bytes("Content-Type", "application/json").expect("valid header"));
    let _ = request.respond(response);
}

fn read_body(request: &mut Request) -> Result<Vec<u8>, String> {
    let mut body = Vec::new();
    request
        .as_reader()
        .take(MAX_BODY + 1)
        .read_to_end(&mut body)
        .map_err(|e| e.to_string())?;
    if body.len() as u64 > MAX_BODY {
        return Err("body exceeds 1 MiB".into());
    }
    Ok(body)
}

fn runtime_info(runtime: &Runtime) -> Value {
    let args = &runtime.args;
    json!({
        "version": env!("CARGO_PKG_VERSION"),
        "image": runtime.guest.display().to_string(),
        "max_cpus": args.max_cpus,
        "max_memory_mib": args.max_memory_mib,
        "max_seconds": args.max_seconds,
        "max_concurrent_runs": args.max_concurrent_runs,
        "running": running(runtime),
    })
}

fn running(runtime: &Runtime) -> usize {
    let runs = runtime.runs.lock().expect("runs lock poisoned");
    runs.values()
        .filter(|run| !run.log.lock().expect("run log lock poisoned").finished)
        .count()
}

fn start(runtime: &Arc<Runtime>, body: &[u8]) -> (u16, Value) {
    let request: RunRequest = match serde_json::from_slice(body) {
        Ok(request) => request,
        Err(e) => return error(400, "invalid", &e.to_string()),
    };
    if let Err(reason) = validate(&request) {
        return error(400, "invalid", &reason);
    }
    let args = &runtime.args;
    let resources = request.resources;
    if resources.cpus > args.max_cpus
        || resources.memory_mib > args.max_memory_mib
        || resources.timeout_seconds > args.max_seconds
    {
        return error(422, "too_large", "resources exceed this runtime's maximums");
    }
    if request.network > args.max_network {
        return error(
            422,
            "too_large",
            &format!(
                "network {} exceeds this runtime's maximum, {}",
                request.network, args.max_network
            ),
        );
    }

    let mut runs = runtime.runs.lock().expect("runs lock poisoned");
    if let Some(existing) = runs.get(&request.run_id) {
        return if existing.request == request {
            (200, existing.record())
        } else {
            error(409, "conflict", "run_id reused with different fields")
        };
    }
    let busy = runs
        .values()
        .filter(|run| !run.log.lock().expect("run log lock poisoned").finished)
        .count();
    if busy >= args.max_concurrent_runs {
        return error(429, "busy", "every run slot is in use");
    }
    let run = Arc::new(Run {
        dir: args.state.join("runs").join(&request.run_id),
        created_at: crate::log::rfc3339(SystemTime::now()),
        request,
        cancel: AtomicBool::new(false),
        deleted: AtomicBool::new(false),
        log: Mutex::new(Log::default()),
        changed: Condvar::new(),
    });
    runs.insert(run.request.run_id.clone(), Arc::clone(&run));
    drop(runs);
    let request = &run.request;
    let results = format!("{:?}", request.results).to_lowercase();
    // The program's name only: arguments and the environment may carry secrets.
    let program = Path::new(&request.argv[0])
        .file_name()
        .map_or_else(String::new, |name| name.to_string_lossy().into_owned());
    crate::log::info(
        "run accepted",
        &[
            ("run", &request.run_id),
            ("program", &program),
            ("cpus", &request.resources.cpus),
            ("memory_mib", &request.resources.memory_mib),
            ("timeout_seconds", &request.resources.timeout_seconds),
            ("network", &request.network),
            ("results", &results),
            ("archive", &request.workspace_archive.is_some()),
        ],
    );

    let record = run.record();
    let runtime = Arc::clone(runtime);
    thread::spawn(move || execute(&runtime, &run));
    (201, record)
}

/// Whether `id` has a run id's form: `run_` and 64 lowercase hex digits.
fn is_run_id(id: &str) -> bool {
    id.strip_prefix("run_").is_some_and(|hex| {
        hex.len() == 64
            && hex
                .bytes()
                .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
    })
}

fn validate(request: &RunRequest) -> Result<(), String> {
    let id_ok = is_run_id(&request.run_id);
    if !id_ok {
        return Err("run_id must be run_ and 64 lowercase hex digits".into());
    }
    if request.argv.is_empty()
        || request
            .argv
            .iter()
            .any(|arg| arg.is_empty() || arg.contains('\0'))
    {
        return Err("argv must hold non-empty strings without NUL".into());
    }
    if !request.cwd.starts_with('/') {
        return Err("cwd must be absolute".into());
    }
    let resources = request.resources;
    if resources.cpus == 0 || resources.memory_mib < 128 || resources.timeout_seconds == 0 {
        return Err("resources must be at least 1 vCPU, 128 MiB and 1 second".into());
    }
    if let Some(archive) = &request.workspace_archive
        && !(archive.is_absolute() && archive.is_file())
    {
        return Err(format!(
            "workspace_archive {} is not a readable file",
            archive.display()
        ));
    }
    Ok(())
}

/// Runs one VM and records its events; the run's thread.
fn execute(runtime: &Runtime, run: &Run) {
    let request = &run.request;
    let (outcome, results) = match prepare(run, runtime.args.workspace_mib) {
        Err(e) => (failed(run, &e), None),
        // The workspace's file system goes at the end of this arm, before the run is finished.
        Ok((workspace, _disk, mut results)) => {
            let spec = VmSpec {
                workspace,
                own_workspace: true,
                stdin: None,
                rootfs: runtime.image.as_ref().map(|i| i.rootfs.clone()),
                archive: request.workspace_archive.clone(),
                guest: runtime.guest.clone(),
                libkrun: runtime.libkrun.clone(),
                virtiofsd: runtime.args.virtiofsd.clone(),
                passt: runtime
                    .args
                    .passt
                    .clone()
                    .unwrap_or_else(crate::bundle::passt),
                network: request.network,
                cpus: request.resources.cpus,
                memory_mib: request.resources.memory_mib,
                io: crate::cgroup::IoRate {
                    mib_per_second: runtime.args.io_mibps,
                    ops_per_second: runtime.args.io_iops,
                },
                timeout: Some(Duration::from_secs(u64::from(
                    request.resources.timeout_seconds,
                ))),
                console_log: Some(run.dir.join("console.log")),
                cgroup_root: runtime.args.cgroup_root.clone(),
                kernel_args: None,
                command: peerspan_runtime_protocol::Command {
                    argv: request.argv.clone(),
                    cwd: request.cwd.clone(),
                    env: crate::image::merge_env(
                        runtime
                            .image
                            .as_ref()
                            .map_or_else(Vec::new, |i| i.env.clone()),
                        request.env.clone(),
                    ),
                    archive: false,
                    collect_changes: request.results == Results::Changes,
                    network: false,
                    // The contract has no standard input and keeps the two streams apart.
                    stdin: false,
                    combined_output: false,
                },
            };
            if let Some(image) = &runtime.image {
                image.mark_used();
            }
            let outcome = vm::run(&spec, &run.cancel, &mut |event| results.record(run, &event))
                .unwrap_or_else(|e| failed(run, &e));
            (outcome, Some(results))
        }
    };
    let (stdout_bytes, stderr_bytes) = results
        .as_ref()
        .map_or((0, 0), |r| (r.stdout_bytes, r.stderr_bytes));
    run.push(json!({
        "kind": "exited",
        "result": {
            "exit_code": outcome.exit_code,
            "signal": outcome.signal,
            "timed_out": outcome.timed_out,
            "duration_ms": u64::try_from(outcome.duration.as_millis()).unwrap_or(u64::MAX),
            "stdout_bytes": stdout_bytes,
            "stderr_bytes": stderr_bytes,
            "error": outcome.error,
        },
    }));
    log_finished(run, &outcome, stdout_bytes, stderr_bytes);
    let artifacts = results.map_or_else(Vec::new, |r| r.finish(runtime.args.client_uid));
    run.push(json!({"kind": "results", "artifacts": artifacts}));
    run.log.lock().expect("run log lock poisoned").finished = true;
    run.changed.notify_all();
    if run.deleted.load(Ordering::Relaxed) {
        let _ = fs::remove_dir_all(&run.dir);
    }
}

/// Logs how a run ended, leaving out what it does not have, such as an exit code after a
/// signal.
fn log_finished(run: &Run, outcome: &Outcome, stdout_bytes: u64, stderr_bytes: u64) {
    let mut fields: Vec<(&str, String)> = vec![("run", run.request.run_id.clone())];
    if let Some(code) = outcome.exit_code {
        fields.push(("exit_code", code.to_string()));
    }
    if let Some(signal) = outcome.signal {
        fields.push(("signal", signal.to_string()));
    }
    if outcome.timed_out {
        fields.push(("timed_out", "true".into()));
    }
    if let Some(error) = &outcome.error {
        fields.push(("error", error.clone()));
    }
    fields.push(("duration_ms", outcome.duration.as_millis().to_string()));
    if !outcome.metrics.connect.is_zero() {
        fields.push(("boot_ms", outcome.metrics.connect.as_millis().to_string()));
    }
    if let Some(peak) = outcome.metrics.memory_peak_bytes {
        fields.push(("memory_peak_mib", (peak >> 20).to_string()));
    }
    fields.push(("stdout_bytes", stdout_bytes.to_string()));
    fields.push(("stderr_bytes", stderr_bytes.to_string()));
    let fields: Vec<crate::log::Field<'_>> = fields
        .iter()
        .map(|(name, value)| (*name, value as &dyn std::fmt::Display))
        .collect();
    crate::log::info("run finished", &fields);
}

/// Locks the state directory for as long as this serve lasts, so that a second serve cannot
/// share it and remove this one's runs at its startup.
fn lock_state(state: &Path) -> anyhow::Result<File> {
    let dir = File::open(state).with_context(|| format!("opening {}", state.display()))?;
    match rustix::fs::flock(&dir, rustix::fs::FlockOperation::NonBlockingLockExclusive) {
        Ok(()) => Ok(dir),
        Err(e) if e == rustix::io::Errno::WOULDBLOCK => {
            anyhow::bail!("another serve is using {}", state.display())
        }
        Err(e) => Err(e).with_context(|| format!("locking {}", state.display())),
    }
}

/// Removes the runs an earlier serve left: runs live only as long as the serve that started
/// them, and the daemon fails a task whose run this serve does not know, so nothing of theirs
/// is still wanted. Their workspaces are unmounted first.
fn remove_earlier_runs(runs: &Path) {
    crate::disk::remove_stale(runs);
    let Ok(entries) = fs::read_dir(runs) else {
        return;
    };
    let mut removed = 0;
    for entry in entries.flatten() {
        match fs::remove_dir_all(entry.path()) {
            Ok(()) => removed += 1,
            Err(e) => crate::log::warn(
                "could not remove a run left by an earlier serve",
                &[("dir", &entry.path().display()), ("error", &e)],
            ),
        }
    }
    if removed > 0 {
        crate::log::info(
            "removed runs left by an earlier serve",
            &[("runs", &removed)],
        );
    }
}

/// Checks that a run's own user can reach workspaces under `dir`: each run's workspace server
/// runs as that user, which belongs to no group, so every directory on the way must be
/// searchable by other users.
fn reachable_by_runs(dir: &Path) -> anyhow::Result<()> {
    let dir = fs::canonicalize(dir).with_context(|| format!("{}", dir.display()))?;
    for ancestor in dir.ancestors() {
        if fs::metadata(ancestor)?.permissions().mode() & 0o001 == 0 {
            anyhow::bail!(
                "{} must be searchable by other users (o+x): each run's workspace server runs as \
                 the run's own user and must reach its workspace under {}",
                ancestor.display(),
                dir.display()
            );
        }
    }
    Ok(())
}

/// A run that ended outside the command: cancelled before the guest connected, or failed.
fn failed(run: &Run, e: &anyhow::Error) -> Outcome {
    if run.cancel.load(Ordering::Relaxed) {
        Outcome {
            signal: Some(libc::SIGKILL),
            ..Outcome::default()
        }
    } else {
        Outcome {
            error: Some(format!("{e:#}")),
            ..Outcome::default()
        }
    }
}

fn prepare(run: &Run, workspace_mib: u32) -> anyhow::Result<(PathBuf, WorkspaceDisk, ResultFiles)> {
    let _ = fs::remove_dir_all(&run.dir);
    fs::DirBuilder::new().mode(0o711).create(&run.dir)?;
    let workspace = run.dir.join("workspace");
    fs::DirBuilder::new().mode(0o700).create(&workspace)?;
    let disk = WorkspaceDisk::create(&run.dir.join(crate::disk::IMAGE), &workspace, workspace_mib)?;
    let results_dir = run.dir.join("results");
    fs::DirBuilder::new().mode(0o700).create(&results_dir)?;
    Ok((
        workspace,
        disk,
        ResultFiles::new(results_dir, run.request.results)?,
    ))
}

/// The result files of one run, written as its events arrive.
struct ResultFiles {
    dir: PathBuf,
    results: Results,
    stdout: Option<File>,
    stderr: Option<File>,
    changes: Option<File>,
    deleted: Option<File>,
    stdout_bytes: u64,
    stderr_bytes: u64,
    /// Bytes of `changes.tar` and `deleted.txt` together.
    changes_bytes: u64,
}

impl ResultFiles {
    fn new(dir: PathBuf, results: Results) -> io::Result<ResultFiles> {
        let open = |name: &str| -> io::Result<Option<File>> {
            match results {
                Results::None => Ok(None),
                Results::Output | Results::Changes => File::create(dir.join(name)).map(Some),
            }
        };
        Ok(ResultFiles {
            stdout: open("stdout.log")?,
            stderr: open("stderr.log")?,
            changes: None,
            deleted: None,
            dir,
            results,
            stdout_bytes: 0,
            stderr_bytes: 0,
            changes_bytes: 0,
        })
    }

    fn record(&mut self, run: &Run, event: &Event<'_>) -> io::Result<()> {
        match *event {
            Event::Started => {
                if self.results == Results::Changes {
                    self.changes = Some(File::create(self.dir.join("changes.tar"))?);
                    self.deleted = Some(File::create(self.dir.join("deleted.txt"))?);
                }
                crate::log::info("run started", &[("run", &run.request.run_id)]);
                run.push(json!({"kind": "started"}));
            }
            Event::Stdout(data) => {
                append_log(self.stdout.as_mut(), self.stdout_bytes, data)?;
                self.stdout_bytes += data.len() as u64;
                push_output(run, "stdout", data);
            }
            Event::Stderr(data) => {
                append_log(self.stderr.as_mut(), self.stderr_bytes, data)?;
                self.stderr_bytes += data.len() as u64;
                push_output(run, "stderr", data);
            }
            Event::Changes(data) => {
                if self.count_changes(data)
                    && let Some(file) = self.changes.as_mut()
                {
                    file.write_all(data)?;
                }
            }
            Event::Deleted(data) => {
                if self.count_changes(data)
                    && let Some(file) = self.deleted.as_mut()
                {
                    file.write_all(data)?;
                }
            }
        }
        Ok(())
    }

    /// Counts `data` toward the changes, and whether to keep it. Changes that outgrow the
    /// artifact limit are removed, both files together as the contract attaches them, so a guest
    /// cannot fill the host's disk with them.
    fn count_changes(&mut self, data: &[u8]) -> bool {
        self.changes_bytes += data.len() as u64;
        if self.changes_bytes <= ARTIFACT_LIMIT {
            return true;
        }
        if self.changes.take().is_some() | self.deleted.take().is_some() {
            let _ = fs::remove_file(self.dir.join("changes.tar"));
            let _ = fs::remove_file(self.dir.join("deleted.txt"));
        }
        false
    }

    /// Closes the files, gives them to the daemon's user, and describes them in contract order.
    fn finish(self, client_uid: u32) -> Vec<Value> {
        let _ = std::os::unix::fs::chown(&self.dir, Some(client_uid), None);
        let names: &[(&str, &str)] = match self.results {
            Results::None => &[],
            Results::Output => &[("stdout.log", "text/plain"), ("stderr.log", "text/plain")],
            Results::Changes => &[
                ("stdout.log", "text/plain"),
                ("stderr.log", "text/plain"),
                ("changes.tar", "application/x-tar"),
                ("deleted.txt", "text/plain"),
            ],
        };
        drop((self.stdout, self.stderr, self.changes, self.deleted));
        names
            .iter()
            .filter_map(|(name, media_type)| {
                let path = self.dir.join(name);
                let (size, sha256) = digest(&path).ok()?;
                let _ = std::os::unix::fs::chown(&path, Some(client_uid), None);
                Some(json!({
                    "name": name,
                    "path": path.display().to_string(),
                    "size": size,
                    "sha256": sha256,
                    "media_type": media_type,
                }))
            })
            .collect()
    }
}

fn append_log(file: Option<&mut File>, written: u64, data: &[u8]) -> io::Result<()> {
    let Some(file) = file else { return Ok(()) };
    let room = usize::try_from(ARTIFACT_LIMIT.saturating_sub(written)).unwrap_or(usize::MAX);
    file.write_all(&data[..data.len().min(room)])
}

fn push_output(run: &Run, stream: &str, data: &[u8]) {
    for chunk in data.chunks(CHUNK) {
        run.push(json!({"kind": stream, "data": BASE64.encode(chunk)}));
    }
}

fn digest(path: &Path) -> io::Result<(u64, String)> {
    let mut hasher = Sha256::new();
    let size = io::copy(&mut File::open(path)?, &mut hasher)?;
    let hex = hasher
        .finalize()
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect();
    Ok((size, hex))
}

fn events(runtime: &Runtime, id: &str, query: &str) -> (u16, Value) {
    let Some(run) = find(runtime, id) else {
        return not_found();
    };
    let mut after = 0usize;
    let mut wait = Duration::ZERO;
    for pair in query.split('&').filter(|pair| !pair.is_empty()) {
        let parsed = match pair.split_once('=') {
            Some(("after", value)) => value.parse().map(|v| after = v).is_ok(),
            Some(("wait", value)) => value
                .parse::<u64>()
                .ok()
                .filter(|v| Duration::from_secs(*v) <= MAX_WAIT)
                .map(|v| wait = Duration::from_secs(v))
                .is_some(),
            _ => false,
        };
        if !parsed {
            return error(400, "invalid", &format!("bad query parameter {pair}"));
        }
    }
    let deadline = Instant::now() + wait;
    let mut log = run.log.lock().expect("run log lock poisoned");
    while log.events.len() <= after && !log.finished {
        let now = Instant::now();
        if now >= deadline {
            break;
        }
        log = run
            .changed
            .wait_timeout(log, deadline - now)
            .expect("run log lock poisoned")
            .0;
    }
    (
        200,
        Value::Array(log.events.get(after..).unwrap_or_default().to_vec()),
    )
}

fn delete(runtime: &Runtime, id: &str) -> (u16, Value) {
    let Some(run) = runtime.runs.lock().expect("runs lock poisoned").remove(id) else {
        return not_found();
    };
    run.deleted.store(true, Ordering::Relaxed);
    run.cancel.store(true, Ordering::Relaxed);
    crate::log::info("run deleted", &[("run", &id)]);
    if run.log.lock().expect("run log lock poisoned").finished {
        let _ = fs::remove_dir_all(&run.dir);
    }
    (200, json!({}))
}

fn find(runtime: &Runtime, id: &str) -> Option<Arc<Run>> {
    runtime
        .runs
        .lock()
        .expect("runs lock poisoned")
        .get(id)
        .cloned()
}

fn not_found() -> (u16, Value) {
    error(404, "not_found", "no such run")
}

fn error(status: u16, code: &str, message: &str) -> (u16, Value) {
    (status, json!({"code": code, "error": message}))
}
