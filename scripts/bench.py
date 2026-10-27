#!/usr/bin/env python3
# SPDX-License-Identifier: Apache-2.0
# Copyright The Peerspan Authors
"""Measures boot time and memory: boots RUNS VMs that run COMMAND and summarises the metrics.

usage: bench.py [--runs N] [--cpus N] [--memory-mib N] [--label TEXT] [--record] [-- COMMAND...]

Needs root and the environment scripts/check.sh needs (PEERSPAN_RUNTIME_GUEST,
PEERSPAN_LIBKRUN, optionally PEERSPAN_RUNTIME_BIN). The command defaults to `true`, so the
numbers are the cost of a VM itself. --record appends the summary to bench/history.jsonl, the
running log of results to compare changes against.
"""
import argparse
import datetime
import json
import os
import pathlib
import platform
import statistics
import subprocess
import sys
import tempfile

REPO = pathlib.Path(__file__).resolve().parent.parent
METRICS = ("connect_ms", "start_ms", "total_ms", "memory_peak_mib")


def percentile(values, p):
    ordered = sorted(values)
    return ordered[min(len(ordered) - 1, round(p / 100 * (len(ordered) - 1)))]


def main():
    parser = argparse.ArgumentParser(description=__doc__.splitlines()[0])
    parser.add_argument("--runs", type=int, default=20)
    parser.add_argument("--cpus", type=int, default=2)
    parser.add_argument("--memory-mib", type=int, default=1024)
    parser.add_argument("--label", default="")
    parser.add_argument("--record", action="store_true")
    parser.add_argument("command", nargs="*", default=["true"])
    args = parser.parse_args()

    binary = os.environ.get("PEERSPAN_RUNTIME_BIN", str(REPO / "target/release/peerspan-runtime"))
    guest = pathlib.Path(os.environ["PEERSPAN_RUNTIME_GUEST"])
    samples = []
    with tempfile.TemporaryDirectory() as work:
        metrics_file = pathlib.Path(work) / "metrics.json"
        workspace = pathlib.Path(work) / "workspace"
        workspace.mkdir()
        # One unmeasured boot first, so page-cache state does not skew the first sample.
        for i in range(args.runs + 1):
            subprocess.run([binary, "run", "--workspace", str(workspace), "--cpus", str(args.cpus),
                            "--memory-mib", str(args.memory_mib), "--metrics", str(metrics_file),
                            "--", *args.command],
                           check=True, stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL)
            if i == 0:
                continue
            m = json.loads(metrics_file.read_text())
            samples.append({"connect_ms": m["connect_ms"], "start_ms": m["start_ms"],
                            "total_ms": m["total_ms"],
                            "memory_peak_mib": m["memory_peak_bytes"] / (1 << 20)})

    summary = {key: {"p50": round(statistics.median(s[key] for s in samples), 1),
                     "p90": round(percentile([s[key] for s in samples], 90), 1),
                     "max": round(max(s[key] for s in samples), 1)} for key in METRICS}
    print(f"{args.runs} runs of {' '.join(args.command)!r}, {args.cpus} vCPUs, "
          f"{args.memory_mib} MiB guest")
    print(f"{'':18}{'p50':>9}{'p90':>9}{'max':>9}")
    for key in METRICS:
        row = summary[key]
        print(f"{key:18}{row['p50']:>9}{row['p90']:>9}{row['max']:>9}")

    if args.record:
        commit = subprocess.run(["git", "-C", str(REPO), "describe", "--always", "--dirty"],
                                capture_output=True, text=True).stdout.strip()
        kernel = (guest / "kernel.config").exists() and next(
            (line.split()[2] for line in (guest / "kernel.config").read_text().splitlines()
             if line.startswith("# Linux/")), None)
        entry = {"at": datetime.datetime.now(datetime.UTC).isoformat(timespec="seconds"),
                 "label": args.label, "commit": commit, "kernel": kernel or "distribution",
                 "host": {"cpus": os.cpu_count(), "kernel": platform.release()},
                 "runs": args.runs, "command": args.command, "cpus": args.cpus,
                 "memory_mib": args.memory_mib, **summary}
        history = REPO / "bench/history.jsonl"
        history.parent.mkdir(exist_ok=True)
        with history.open("a") as f:
            f.write(json.dumps(entry) + "\n")
        print(f"recorded in {history.relative_to(REPO)}")


if __name__ == "__main__":
    sys.exit(main())
