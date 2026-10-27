#!/usr/bin/env python3
# SPDX-License-Identifier: Apache-2.0
# Copyright The Peerspan Authors
"""Drives `peerspan-runtime serve` over its socket as the host daemon will.

Needs root and the environment scripts/check.sh needs; run by it.
"""
import base64
import hashlib
import http.client
import io
import json
import os
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
    server = subprocess.Popen([os.environ["PEERSPAN_RUNTIME_BIN"], "serve", "--socket", sock,
                               "--state", state, "--max-cpus", "2", "--max-memory-mib", "2048",
                               "--max-seconds", "120", "--max-concurrent-runs", "1",
                               "--max-network", "internet"],
                              stderr=subprocess.DEVNULL)
    try:
        for _ in range(100):
            if os.path.exists(sock):
                break
            time.sleep(0.05)
        status, info = call(sock, "GET", "/v1/runtime")
        check("runtime", status == 200 and info["max_cpus"] == 2 and info["running"] == 0, info)

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

        status, _ = call(sock, "DELETE", f"/v1/runs/{run_id(1)}")
        gone, _ = call(sock, "GET", f"/v1/runs/{run_id(1)}/events")
        check("delete", status == 200 and gone == 404
              and not os.path.exists(os.path.join(state, "runs", run_id(1))), (status, gone))
    finally:
        server.kill()
        server.wait()
    return failures


if __name__ == "__main__":
    sys.exit(1 if main() else 0)
