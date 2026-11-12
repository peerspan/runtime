#!/usr/bin/env python3
# SPDX-License-Identifier: Apache-2.0
# Copyright The Peerspan Authors
"""Drives `peerspan-runtime serve` over its socket as the host daemon will.

Needs root and the environment scripts/check.sh needs; run by it.
"""
import base64
import glob
import hashlib
import http.client
import io
import json
import os
import re
import socket
import subprocess
import sys
import tarfile
import tempfile
import time

failures = 0


class UnixConnection(http.client.HTTPConnection):
    def __init__(self, path):
        super().__init__("localhost", timeout=60)
        self.path = path

    def connect(self):
        self.sock = socket.socket(socket.AF_UNIX, socket.SOCK_STREAM)
        self.sock.connect(self.path)


def call(sock, method, path, body=None):
    conn = UnixConnection(sock)
    conn.request(method, path, json.dumps(body) if body is not None else None,
                 {"Content-Type": "application/json"})
    response = conn.getresponse()
    data = json.loads(response.read() or b"null")
    conn.close()
    return response.status, data


def check(name, ok, detail=""):
    global failures
    if ok:
        print(f"ok   serve-{name}")
    else:
        failures += 1
        print(f"FAIL serve-{name}: {detail}")


def run_id(n):
    return "run_" + hashlib.sha256(f"host/task_{n}".encode()).hexdigest()


def request(n, argv, results="output", archive=None, **resources):
    return {"run_id": run_id(n), "argv": argv, "cwd": "/workspace", "env": {"GREETING": "hi"},
            "resources": {"cpus": 1, "memory_mib": 512, "timeout_seconds": 60, **resources},
            "workspace_archive": archive, "results": results}


def follow(sock, rid):
    """Long-polls until the results event and returns every event."""
    events = []
    while not events or events[-1]["kind"] != "results":
        status, batch = call(sock, "GET", f"/v1/runs/{rid}/events?after={len(events)}&wait=25")
        assert status == 200, (status, batch)
        events += batch
    return events


def main():
    work = tempfile.mkdtemp()
    # Each run's workspace server runs as the run's own user, which must reach the state dir.
    os.chmod(work, 0o711)
    sock = os.path.join(work, "runtime.sock")
    state = os.path.join(work, "state")
    serve_argv = [os.environ["PEERSPAN_RUNTIME_BIN"], "serve", "--socket", sock,
                  "--state", state, "--max-cpus", "2", "--max-memory-mib", "2048",
                  "--max-seconds", "120", "--max-concurrent-runs", "1",
                  "--max-network", "internet", "--workspace-mib", "256"]

    log_path = os.path.join(work, "serve.log")

    def start():
        if os.path.exists(sock):
            os.remove(sock)
        process = subprocess.Popen(serve_argv, stderr=open(log_path, "a"))
        for _ in range(100):
            if os.path.exists(sock):
                break
            time.sleep(0.05)
        return process

    server = start()
    try:
        status, info = call(sock, "GET", "/v1/runtime")
        check("runtime", status == 200 and info["max_cpus"] == 2 and info["running"] == 0
              and info["peerfs"] is False, info)

        archive = os.path.join(work, "ws.tar")
        with tarfile.open(archive, "w") as t:
            data = b"from the archive\n"
            info = tarfile.TarInfo("input.txt")
            info.size = len(data)
            t.addfile(info, io.BytesIO(data))
        body = request(1, ["sh", "-c", "cat input.txt; echo $GREETING >&2; echo x > out.txt"],
                       results="changes", archive=archive)
        status, record = call(sock, "POST", "/v1/runs", body)
        check("create", status == 201 and record["state"] == "running", (status, record))
        status, _ = call(sock, "POST", "/v1/runs", body)
        check("repeat", status == 200, status)
        status, _ = call(sock, "POST", "/v1/runs", {**body, "argv": ["true"]})
        check("conflict", status == 409, status)
        status, _ = call(sock, "POST", "/v1/runs", request(2, ["true"]))
        check("busy", status == 429, status)

        events = follow(sock, run_id(1))
        kinds = [e["kind"] for e in events]
        out = b"".join(base64.b64decode(e["data"]) for e in events if e["kind"] == "stdout")
        err = b"".join(base64.b64decode(e["data"]) for e in events if e["kind"] == "stderr")
        result = next(e["result"] for e in events if e["kind"] == "exited")
        artifacts = events[-1]["artifacts"]
        check("events", kinds[0] == "started" and kinds[-2:] == ["exited", "results"]
              and [e["sequence"] for e in events] == list(range(1, len(events) + 1)), kinds)
        check("output", out == b"from the archive\n" and err == b"hi\n", (out, err))
        status, page = call(sock, "GET", f"/v1/runs/{run_id(1)}/events?after=1&limit=2")
        refused = [call(sock, "GET", f"/v1/runs/{run_id(1)}/events?limit={n}")[0] for n in (0, 1001)]
        check("events-limit", status == 200 and page == events[1:3] and refused == [400, 400],
              (status, page, refused))
        check("result", result["exit_code"] == 0 and result["signal"] is None
              and not result["timed_out"] and result["stdout_bytes"] == 17 and result["error"] is None,
              result)
        names = [a["name"] for a in artifacts]
        digests_ok = all(hashlib.sha256(open(a["path"], "rb").read()).hexdigest() == a["sha256"]
                         and os.path.getsize(a["path"]) == a["size"] for a in artifacts)
        changed = tarfile.open(next(a["path"] for a in artifacts if a["name"] == "changes.tar")).getnames()
        check("results", names == ["stdout.log", "stderr.log", "changes.tar", "deleted.txt"]
              and digests_ok and changed == ["out.txt"], (names, changed))

        status, _ = call(sock, "POST", "/v1/runs", request(3, ["true"], cpus=3))
        check("too-large", status == 422, status)

        # A run may not ask for more network than the host owner's maximum, nor an unknown level.
        status, _ = call(sock, "POST", "/v1/runs", {**request(90, ["true"]), "network": "internet+lan"})
        check("network-too-large", status == 422, status)
        status, _ = call(sock, "POST", "/v1/runs", {**request(91, ["true"]), "network": "everything"})
        check("network-invalid", status == 400, status)

        status, _ = call(sock, "POST", "/v1/runs", request(4, ["sleep", "60"], results="none"))
        time.sleep(3)
        status, _ = call(sock, "POST", f"/v1/runs/{run_id(4)}/cancel", {})
        events = follow(sock, run_id(4))
        result = next(e["result"] for e in events if e["kind"] == "exited")
        check("cancel", status == 200 and result["signal"] == 9 and result["exit_code"] is None
              and events[-1]["artifacts"] == [], result)

        status, _ = call(sock, "POST", "/v1/runs", request(5, ["sleep", "60"], results="none",
                                                           timeout_seconds=1))
        events = follow(sock, run_id(5))
        result = next(e["result"] for e in events if e["kind"] == "exited")
        check("timeout", result["timed_out"] and result["exit_code"] is None, result)

        # Changes larger than an artifact may be are left out, both files together.
        status, _ = call(sock, "POST", "/v1/runs", request(
            6, ["sh", "-c", "head -c 105000000 /dev/zero > big; echo done"], results="changes"))
        events = follow(sock, run_id(6))
        result = next(e["result"] for e in events if e["kind"] == "exited")
        names = [a["name"] for a in events[-1]["artifacts"]]
        check("changes-too-large", result["exit_code"] == 0
              and names == ["stdout.log", "stderr.log"], (result, names))

        # A workspace holds what its file system's size allows, and is gone after the run.
        status, _ = call(sock, "POST", "/v1/runs", request(
            7, ["dd", "if=/dev/zero", "of=big", "bs=1M", "count=300"], results="none"))
        events = follow(sock, run_id(7))
        result = next(e["result"] for e in events if e["kind"] == "exited")
        err = b"".join(base64.b64decode(e["data"]) for e in events if e["kind"] == "stderr")
        mounted = [line for line in open("/proc/self/mountinfo") if state in line]
        images = [run for run in os.listdir(os.path.join(state, "runs"))
                  if os.path.exists(os.path.join(state, "runs", run, "workspace.img"))]
        check("workspace-full", result["exit_code"] == 1 and b"No space left" in err
              and not mounted and not images, (result, err, mounted, images))

        status, _ = call(sock, "DELETE", f"/v1/runs/{run_id(1)}")
        gone, _ = call(sock, "GET", f"/v1/runs/{run_id(1)}/events")
        check("delete", status == 200 and gone == 404
              and not os.path.exists(os.path.join(state, "runs", run_id(1))), (status, gone))

        # A malformed request's reason, which can quote what it was sent, stays out of the log.
        status, _ = call(sock, "POST", "/v1/runs", {**request(9, ["true"]), "env": "TOKEN=hunter2"})
        check("malformed", status == 400, status)

        # The log has one timestamped line per event, the first run's whole life among them,
        # and nothing of what the runs were given beyond the program's name.
        log = open(log_path).read()
        lines = log.splitlines()
        well_formed = all(re.match(r"\d{4}-\d\d-\d\dT[\d:.]+Z (info|warn) ", line)
                          for line in lines)
        # The repeat with different fields is refused under the same id.
        first = [event for event in (re.sub(r"^\S+ \S+ ", "", line).split(" run=")[0]
                                     for line in lines if f"run={run_id(1)}" in line)
                 if event != "run refused"]
        check("log", well_formed and first[:4] == ["run accepted", "run started", "run finished",
                                                   "run deleted"]
              and "run refused" in log and "input.txt" not in log and "GREETING" not in log
              and "hunter2" not in log,
              (first, [line for line in lines if not re.match(r"\d{4}-", line)][:3]))
        # A second serve may not share the state directory. A serve killed outright leaves its
        # runs behind, and the next one removes them: their directories, workspaces and records.
        status, _ = call(sock, "POST", "/v1/runs", request(8, ["sleep", "60"], results="none"))
        time.sleep(3)
        second = subprocess.run(serve_argv[:2] + ["--socket", sock + ".2"] + serve_argv[4:],
                                capture_output=True, text=True, timeout=30)
        check("one-per-state", second.returncode != 0 and "another serve" in second.stderr,
              (second.returncode, second.stderr[-200:]))
        server.kill()
        server.wait()
        server = start()
        gone, _ = call(sock, "GET", f"/v1/runs/{run_id(8)}/events")
        left = os.listdir(os.path.join(state, "runs"))
        mounted = [line for line in open("/proc/self/mountinfo") if state in line]
        cgroups = glob.glob("/sys/fs/cgroup/peerspan-runtime/run-*")
        check("restart", status == 201 and gone == 404 and not left and not mounted
              and not cgroups, (status, gone, left, mounted, cgroups))

        # Runs side by side, each with its own archive and output.
        server.kill()
        server.wait()
        serve_argv[serve_argv.index("--max-concurrent-runs") + 1] = "3"
        server = start()
        for n in (10, 11, 12):
            path = os.path.join(work, f"ws{n}.tar")
            with tarfile.open(path, "w") as t:
                data = f"{n}\n".encode()
                info = tarfile.TarInfo("n.txt")
                info.size = len(data)
                t.addfile(info, io.BytesIO(data))
            call(sock, "POST", "/v1/runs", request(n, ["sh", "-c", "sleep 2; cat n.txt"],
                                                  archive=path))
        outputs = []
        for n in (10, 11, 12):
            events = follow(sock, run_id(n))
            outputs.append(b"".join(base64.b64decode(e["data"]) for e in events
                                    if e["kind"] == "stdout"))
        check("concurrent", outputs == [b"10\n", b"11\n", b"12\n"], outputs)

        # A peerfs workspace is refused unless serve names a space.
        status, _ = call(sock, "POST", "/v1/runs",
                         {**request(20, ["true"]), "peerfs_workspace": "agent"})
        check("peerfs-no-space", status == 422, status)

        # With PEERSPAN_PEERFS naming a peerfs binary: a run on a workspace of the space reads
        # its branch's files and its writes land in the workspace, and its result names the
        # snapshot taken after it, which holds only what it changed: a change made before the
        # run is in the snapshot before. It takes no archive and gives no changes, which the
        # workspace's history holds, and runs on one workspace take turns.
        peerfs = os.environ.get("PEERSPAN_PEERFS")
        if peerfs:
            space = os.path.join(work, "peerfs-space")
            src = os.path.join(work, "peerfs-src")
            os.mkdir(src)
            open(os.path.join(src, "a.txt"), "w").write("branch\n")

            def pf(*argv):
                return subprocess.run([peerfs, "--space", space, *argv], check=True,
                                      capture_output=True, text=True).stdout
            pf("init")
            pf("import", src, "-m", "base")
            pf("ws", "create", "agent")
            subprocess.run([peerfs, "--space", space, "ws", "write", "agent", "early.txt"],
                           input="before\n", text=True, check=True)
            server.kill()
            server.wait()
            serve_argv += ["--peerfs-space", space, "--peerfs", peerfs]
            server = start()
            try:
                body = {**request(21, ["sh", "-c", "cat a.txt; echo y > b.txt"]),
                        "peerfs_workspace": "agent"}
                status, _ = call(sock, "POST", "/v1/runs", body)
                events = follow(sock, run_id(21))
                out = b"".join(base64.b64decode(e["data"]) for e in events
                               if e["kind"] == "stdout")
                result = next(e["result"] for e in events if e["kind"] == "exited")
                snapshots = pf("ws", "snapshots", "agent").splitlines()
                check("peerfs-run", status == 201 and result["exit_code"] == 0
                      and out == b"branch\n" and pf("ws", "read", "agent", "b.txt") == "y\n"
                      and re.fullmatch(r"[0-9a-f]{66}", result["snapshot"] or "")
                      and snapshots[0].startswith(result["snapshot"] + " ")
                      and snapshots[0].endswith(": After a peerspan-runtime run: exit 0")
                      and snapshots[1].endswith(": Before a peerspan-runtime run"),
                      (status, result, out, snapshots))
                info = call(sock, "GET", "/v1/runtime")[1]
                check("peerfs-reported", info["peerfs"] is True, info)

                # A run that changes nothing has no snapshot; one on a held workspace waits.
                status, _ = call(sock, "POST", "/v1/runs",
                                 {**request(25, ["sleep", "3"]), "peerfs_workspace": "agent"})
                held, _ = call(sock, "POST", "/v1/runs",
                               {**request(26, ["true"]), "peerfs_workspace": "agent"})
                events = follow(sock, run_id(25))
                result = next(e["result"] for e in events if e["kind"] == "exited")
                check("peerfs-unchanged", status == 201 and result["exit_code"] == 0
                      and result["snapshot"] is None, (status, result))
                check("peerfs-held", held == 429, held)

                # A deleted run holds its workspace until it has given it back and taken its
                # snapshot, so the next run has nothing of it to snapshot before its command,
                # and its own snapshot follows the deleted run's.
                call(sock, "POST", "/v1/runs", {**request(27, ["sh", "-c", "echo c > c.txt; sleep 60"]),
                                                "peerfs_workspace": "agent"})
                time.sleep(3)
                call(sock, "DELETE", f"/v1/runs/{run_id(27)}")
                body = {**request(28, ["sh", "-c", "echo d > d.txt"]), "peerfs_workspace": "agent"}
                first, _ = call(sock, "POST", "/v1/runs", body)
                status = first
                for _ in range(60):
                    if status != 429:
                        break
                    time.sleep(0.5)
                    status, _ = call(sock, "POST", "/v1/runs", body)
                follow(sock, run_id(28))
                messages = [line.split(": ", 1)[1] for line in
                            pf("ws", "snapshots", "agent").splitlines()[:2]]
                check("peerfs-held-after-delete", first == 429 and status == 201
                      and messages == ["After a peerspan-runtime run: exit 0",
                                       "After a peerspan-runtime run: signal 9"],
                      (first, status, messages))
                refused = [call(sock, "POST", "/v1/runs", {**request(n, ["true"], **fields),
                                                           "peerfs_workspace": name})[0]
                           for n, name, fields in [(22, "agent", {"results": "changes"}),
                                                   (23, "agent", {"archive": archive}),
                                                   (24, "../agent", {})]]
                check("peerfs-refused", refused == [400, 400, 400], refused)

                # A fork (runtime.md decision 6c): a new branch at main's head and a workspace
                # following it, which the run changes and main does not see. Forking onto a
                # workspace that exists fails the run and leaves that workspace alone, and a
                # fork needs a workspace name and a branch name.
                body = {**request(29, ["sh", "-c", "cat a.txt; echo f > f.txt"]),
                        "peerfs_workspace": "fork.29", "peerfs_fork_from": "main"}
                status, _ = call(sock, "POST", "/v1/runs", body)
                events = follow(sock, run_id(29))
                out = b"".join(base64.b64decode(e["data"]) for e in events
                               if e["kind"] == "stdout")
                result = next(e["result"] for e in events if e["kind"] == "exited")
                branches = pf("branches")
                check("peerfs-fork", status == 201 and result["exit_code"] == 0
                      and out == b"branch\n" and pf("ws", "read", "fork.29", "f.txt") == "f\n"
                      and result["snapshot"] and " fork.29\n" in branches
                      and "fork.29 fork.29 " in pf("ws", "list"),
                      (status, result, out, branches))
                before = pf("ws", "snapshots", "agent")
                body = {**request(30, ["sh", "-c", "echo z > z.txt"]),
                        "peerfs_workspace": "agent", "peerfs_fork_from": "main"}
                status, _ = call(sock, "POST", "/v1/runs", body)
                events = follow(sock, run_id(30))
                result = next(e["result"] for e in events if e["kind"] == "exited")
                check("peerfs-fork-exists", status == 201 and result["exit_code"] is None
                      and "exists already" in (result.get("error") or "")
                      and pf("ws", "snapshots", "agent") == before
                      and " agent\n" not in pf("branches"),
                      (status, result))
                refused = [call(sock, "POST", "/v1/runs", {**request(n, ["true"]), **fields})[0]
                           for n, fields in [(31, {"peerfs_fork_from": "main"}),
                                             (32, {"peerfs_workspace": "fork.32",
                                                   "peerfs_fork_from": "a/b"})]]
                check("peerfs-fork-refused", refused == [400, 400], refused)
            finally:
                pf("stop")
    finally:
        server.kill()
        server.wait()
    return failures


if __name__ == "__main__":
    sys.exit(1 if main() else 0)
