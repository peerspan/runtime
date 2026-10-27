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
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use anyhow::Context;
use base64::Engine;
use base64::engine::general_purpose::STANDARD as BASE64;
use clap::Args;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use tiny_http::{Header, Method, Request, Response, Server};

use crate::vm::{self, Event, Outcome, VmSpec};

/// Largest request body accepted, as on the daemon's `admin.sock`.
const MAX_BODY: u64 = 1 << 20;
/// Largest output chunk in one event, before base64.
const CHUNK: usize = 16 * 1024;
/// Each output log is cut at the artifact size limit.
const LOG_LIMIT: u64 = 100 * 1024 * 1024;
/// Longest wait a client may ask for on the events route.
const MAX_WAIT: Duration = Duration::from_secs(25);

#[derive(Debug, Args)]
pub struct ServeArgs {
    /// Unix socket to serve on; replaced if it exists.
    #[arg(long)]
    socket: PathBuf,
    /// Guest image directory holding `vmlinux`, `initramfs.cpio.gz` and `rootfs/`.
    #[arg(long, env = "PEERSPAN_RUNTIME_GUEST")]
    guest: PathBuf,
    /// libkrun shared library to load.
    #[arg(long, env = "PEERSPAN_LIBKRUN", default_value = "libkrun.so.2")]
    libkrun: PathBuf,
    /// virtiofsd binary that serves workspaces.
    #[arg(
        long,
        env = "PEERSPAN_VIRTIOFSD",
        default_value = "/usr/libexec/virtiofsd"
    )]
    virtiofsd: PathBuf,
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
    #[arg(long, default_value_t = 3600)]
    max_seconds: u32,
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
    // SAFETY: geteuid has no preconditions.
    if unsafe { libc::geteuid() } != 0 {
        anyhow::bail!("peerspan-runtime must run as root to confine its VMs");
    }
    for dir in [args.state.clone(), args.state.join("runs")] {
        fs::DirBuilder::new()
            .recursive(true)
            .mode(0o711)
            .create(&dir)
            .with_context(|| format!("creating {}", dir.display()))?;
    }
    let _ = fs::remove_file(&args.socket);
    let server = Server::http_unix(&args.socket)
        .map_err(|e| anyhow::anyhow!("serving on {}: {e}", args.socket.display()))?;
    fs::set_permissions(&args.socket, fs::Permissions::from_mode(0o600))?;
    std::os::unix::fs::chown(&args.socket, Some(args.client_uid), None)?;
    eprintln!("peerspan-runtime: serving on {}", args.socket.display());

    let runtime = Arc::new(Runtime {
        args,
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
            Ok(body) => start(runtime, &body),
            Err(e) => error(400, "invalid", &e),
        },
        (Method::Get, ["v1", "runs", id, "events"]) => events(runtime, id, query),
        (Method::Post, ["v1", "runs", id, "cancel"]) => match find(runtime, id) {
            Some(run) => {
                run.cancel.store(true, Ordering::Relaxed);
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
        "image": args.guest.display().to_string(),
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
        created_at: rfc3339(SystemTime::now()),
        request,
        cancel: AtomicBool::new(false),
        deleted: AtomicBool::new(false),
        log: Mutex::new(Log::default()),
        changed: Condvar::new(),
    });
    runs.insert(run.request.run_id.clone(), Arc::clone(&run));
    drop(runs);

    let record = run.record();
    let runtime = Arc::clone(runtime);
    thread::spawn(move || execute(&runtime, &run));
    (201, record)
}

fn validate(request: &RunRequest) -> Result<(), String> {
    let id_ok = request.run_id.strip_prefix("run_").is_some_and(|hex| {
        hex.len() == 64
            && hex
                .bytes()
                .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
    });
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
    let (outcome, results) = match prepare(run) {
        Err(e) => (failed(run, &e), None),
        Ok((workspace, mut results)) => {
            let spec = VmSpec {
                workspace,
                archive: request.workspace_archive.clone(),
                guest: runtime.args.guest.clone(),
                libkrun: runtime.args.libkrun.clone(),
                virtiofsd: runtime.args.virtiofsd.clone(),
                cpus: request.resources.cpus,
                memory_mib: request.resources.memory_mib,
                timeout: Some(Duration::from_secs(u64::from(
                    request.resources.timeout_seconds,
                ))),
                console_log: Some(run.dir.join("console.log")),
                cgroup_root: runtime.args.cgroup_root.clone(),
                kernel_args: None,
                command: peerspan_runtime_protocol::Command {
                    argv: request.argv.clone(),
                    cwd: request.cwd.clone(),
                    env: request.env.clone().into_iter().collect(),
                    archive: false,
                    collect_changes: request.results == Results::Changes,
                },
            };
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
    let artifacts = results.map_or_else(Vec::new, |r| r.finish(runtime.args.client_uid));
    run.push(json!({"kind": "results", "artifacts": artifacts}));
    run.log.lock().expect("run log lock poisoned").finished = true;
    run.changed.notify_all();
    if run.deleted.load(Ordering::Relaxed) {
        let _ = fs::remove_dir_all(&run.dir);
    }
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

fn prepare(run: &Run) -> anyhow::Result<(PathBuf, ResultFiles)> {
    let _ = fs::remove_dir_all(&run.dir);
    fs::DirBuilder::new().mode(0o711).create(&run.dir)?;
    let workspace = run.dir.join("workspace");
    fs::DirBuilder::new().mode(0o700).create(&workspace)?;
    let results_dir = run.dir.join("results");
    fs::DirBuilder::new().mode(0o700).create(&results_dir)?;
    Ok((
        workspace,
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
        })
    }

    fn record(&mut self, run: &Run, event: &Event<'_>) -> io::Result<()> {
        match *event {
            Event::Started => {
                if self.results == Results::Changes {
                    self.changes = Some(File::create(self.dir.join("changes.tar"))?);
                    self.deleted = Some(File::create(self.dir.join("deleted.txt"))?);
                }
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
                if let Some(file) = self.changes.as_mut() {
                    file.write_all(data)?;
                }
            }
            Event::Deleted(data) => {
                if let Some(file) = self.deleted.as_mut() {
                    file.write_all(data)?;
                }
            }
        }
        Ok(())
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
    let room = usize::try_from(LOG_LIMIT.saturating_sub(written)).unwrap_or(usize::MAX);
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

/// Formats a time as RFC 3339 in UTC, to the second.
fn rfc3339(time: SystemTime) -> String {
    let secs = time.duration_since(UNIX_EPOCH).map_or(0, |d| d.as_secs());
    let (days, rem) = (secs / 86_400, secs % 86_400);
    // Civil date from days since 1970-01-01 (Howard Hinnant's algorithm).
    let z = i64::try_from(days).unwrap_or(0) + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1_460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let day = doy - (153 * mp + 2) / 5 + 1;
    let month = if mp < 10 { mp + 3 } else { mp - 9 };
    let year = yoe + era * 400 + i64::from(month <= 2);
    format!(
        "{year:04}-{month:02}-{day:02}T{:02}:{:02}:{:02}Z",
        rem / 3_600,
        rem % 3_600 / 60,
        rem % 60
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rfc3339_formats_known_instants() {
        let at = |secs| rfc3339(UNIX_EPOCH + Duration::from_secs(secs));
        assert_eq!(at(0), "1970-01-01T00:00:00Z");
        assert_eq!(at(951_782_400), "2000-02-29T00:00:00Z");
        assert_eq!(at(1_793_090_096), "2026-10-27T08:34:56Z");
    }
}
