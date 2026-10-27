#!/bin/sh
# SPDX-License-Identifier: Apache-2.0
# Copyright The Peerspan Authors
#
# Profiles one guest boot: the runtime's phase timeline, the slowest kernel initcalls, and the
# largest gaps in the kernel log. Needs root and the environment scripts/check.sh needs.
#
# usage: boot-profile.sh [EXTRA_KERNEL_ARGS]
set -eu

repo=$(cd "$(dirname "$0")/.." && pwd)
bin=${PEERSPAN_RUNTIME_BIN:-$repo/target/release/peerspan-runtime}
work=$(mktemp -d)
trap 'rm -rf "$work"' EXIT
mkdir "$work/ws"

"$bin" run --workspace "$work/ws" --metrics "$work/metrics.json" \
  --kernel-args "initcall_debug loglevel=7 ${1:-}" -- dmesg > "$work/dmesg"

python3 - "$work/metrics.json" "$work/dmesg" <<'PY'
import json, re, sys
metrics = json.load(open(sys.argv[1]))
print("runtime phases (host, ms since run start)")
for name, at in metrics["phases_ms"]:
    print(f"  {at:8.1f}  {name}")
print("guest init phases (ms of guest boot clock)")
for name, at in metrics["guest_phases_ms"]:
    print(f"  {at:8.1f}  {name}")

lines = []
for line in open(sys.argv[2], errors="replace"):
    m = re.match(r"\[\s*(\d+\.\d+)\] (.*)", line)
    if m:
        lines.append((float(m.group(1)) * 1000, m.group(2)))
calls = []
for _, text in lines:
    m = re.match(r"initcall (\S+) returned \S+ after (\d+) usecs", text)
    if m:
        calls.append((int(m.group(2)) / 1000, m.group(1)))
print(f"slowest initcalls ({len(calls)} ran, {sum(c for c, _ in calls):.1f} ms in all)")
for ms, name in sorted(calls, reverse=True)[:12]:
    print(f"  {ms:8.1f}  {name}")
gaps = sorted(((b[0] - a[0], a[0], a[1], b[1]) for a, b in zip(lines, lines[1:])), reverse=True)
print("largest gaps in the kernel log (ms, at, between)")
for gap, at, before, after in gaps[:8]:
    print(f"  {gap:8.1f}  @{at:7.1f}  {before[:60]!r} -> {after[:50]!r}")
PY
