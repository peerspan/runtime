#!/bin/sh
# SPDX-License-Identifier: Apache-2.0
# Copyright The Peerspan Authors
#
# End-to-end checks: boots real microVMs with `peerspan-runtime run`.
# Needs root, /dev/kvm, a guest image (scripts/build-guest.sh), PEERSPAN_RUNTIME_GUEST pointing
# at it, and PEERSPAN_LIBKRUN pointing at a libkrun built with `make VHOST_USER=1`.
# PEERSPAN_RUNTIME_BIN names a built binary; without it the script builds one.
set -eu

repo=$(cd "$(dirname "$0")/.." && pwd)
bin=${PEERSPAN_RUNTIME_BIN:-}
if [ -z "$bin" ]; then
  cargo build --quiet --release --manifest-path "$repo/Cargo.toml" -p peerspan-runtime
  bin="$repo/target/release/peerspan-runtime"
fi
work=$(mktemp -d)
trap 'rm -rf "$work"' EXIT
failures=0

# check NAME EXPECTED_EXIT -- COMMAND...: runs COMMAND in a fresh VM on $work/ws.
check() {
  name=$1 want=$2
  shift 3
  rc=0
  "$bin" run --workspace "$work/ws" --console-log "$work/console-$name.log" -- "$@" \
    > "$work/out" 2> "$work/err" || rc=$?
  if [ "$rc" -ne "$want" ]; then
    fail "$name: exit $rc, want $want"
    sed 's/^/    stderr: /' "$work/err" | tail -5
    return 1
  fi
}
fail() { echo "FAIL $*"; failures=$((failures + 1)); }
pass() { echo "ok   $1"; }

mkdir -p "$work/ws"
echo "from the host" > "$work/ws/from-host"

if check io 7 -- sh -c 'cat from-host; echo oops >&2; echo made > made-in-vm; exit 7'; then
  [ "$(cat "$work/out")" = "from the host" ] || fail "io: stdout $(cat "$work/out")"
  [ "$(cat "$work/err")" = "oops" ] || fail "io: stderr $(cat "$work/err")"
  [ "$(cat "$work/ws/made-in-vm" 2>/dev/null)" = "made" ] || fail "io: host did not see made-in-vm"
  pass io
fi

if check stream 0 -- seq 1 200000; then
  [ "$(wc -l < "$work/out")" -eq 200000 ] && [ "$(tail -1 "$work/out")" = 200000 ] \
    && pass stream || fail "stream: got $(wc -l < "$work/out") lines"
fi

if check not-found 127 -- no-such-program; then
  grep -q "cannot run no-such-program" "$work/err" && pass not-found || fail "not-found: $(cat "$work/err")"
fi

check signal 137 -- sh -c 'kill -9 $$' && pass signal

if "$bin" run --workspace "$work/ws" --cwd /tmp --env GREETING=hi -- sh -c 'echo "$GREETING $(pwd)"' \
    > "$work/out" 2> "$work/err"; then
  [ "$(cat "$work/out")" = "hi /tmp" ] && pass env-and-cwd || fail "env-and-cwd: $(cat "$work/out")"
else
  fail "env-and-cwd: exit $?"
fi

if check readonly-root 1 -- sh -c 'touch /etc/x'; then pass readonly-root; fi

rc=0
"$bin" run --workspace "$work/ws" --guest "$work/no-such-guest" -- true 2> "$work/err" || rc=$?
[ "$rc" -eq 125 ] && grep -q "guest image" "$work/err" && pass bad-guest || fail "bad-guest: exit $rc"

# Workspace archives: unpacked inside the VM, with the changes reported back.
mkdir -p "$work/src/sub" "$work/arch" "$work/res"
echo alpha > "$work/src/a.txt"
echo beta > "$work/src/sub/b.txt"
ln -s sub/b.txt "$work/src/link"
tar -C "$work/src" -cf "$work/ws.tar" .
gzip -c "$work/ws.tar" > "$work/ws.tar.gz"
rc=0
"$bin" run --workspace "$work/arch" --archive "$work/ws.tar" --results-dir "$work/res" -- \
  sh -c 'cat a.txt link; echo new > new.txt; echo more >> sub/b.txt; rm a.txt' \
  > "$work/out" 2> "$work/err" || rc=$?
listed=$(tar -tf "$work/res/changes.tar" 2>/dev/null | sort | tr '\n' ' ')
if [ "$rc" -eq 0 ] && [ "$(cat "$work/out")" = "$(printf 'alpha\nbeta')" ] \
    && [ "$listed" = "new.txt sub/b.txt " ] && [ "$(cat "$work/res/deleted.txt")" = "a.txt" ]; then
  pass archive-and-changes
else
  fail "archive-and-changes: exit $rc, out [$(cat "$work/out")], changes [$listed], deleted [$(cat "$work/res/deleted.txt" 2>/dev/null)]"
fi

mkdir -p "$work/gz"
rc=0
"$bin" run --workspace "$work/gz" --archive "$work/ws.tar.gz" -- cat sub/b.txt > "$work/out" 2>&1 || rc=$?
[ "$rc" -eq 0 ] && [ "$(cat "$work/out")" = beta ] && pass gzip-archive || fail "gzip-archive: exit $rc"

python3 -c '
import io, sys, tarfile
with tarfile.open(sys.argv[1], "w") as t:
    data = b"escaped\n"
    info = tarfile.TarInfo("../escaped.txt"); info.size = len(data)
    t.addfile(info, io.BytesIO(data))
' "$work/evil.tar"
mkdir -p "$work/evil"
rc=0
"$bin" run --workspace "$work/evil" --archive "$work/evil.tar" -- true > "$work/out" 2> "$work/err" || rc=$?
if [ "$rc" -eq 127 ] && grep -q "refused archive entry ../escaped.txt" "$work/err" \
    && [ ! -e "$work/escaped.txt" ]; then
  pass hostile-archive
else
  fail "hostile-archive: exit $rc, $(cat "$work/err")"
fi

start=$(date +%s); rc=0
"$bin" run --workspace "$work/ws" --timeout-seconds 1 -- sleep 60 > /dev/null 2>&1 || rc=$?
took=$(( $(date +%s) - start ))
[ "$rc" -eq 124 ] && [ "$took" -lt 15 ] && pass timeout || fail "timeout: exit $rc after ${took}s"

PEERSPAN_RUNTIME_BIN="$bin" python3 "$repo/scripts/check-serve.py" || failures=$((failures + 1))

# Inspect a running VM from the host: the jailed VMM and the workspace server.
"$bin" run --workspace "$work/ws" -- sleep 4 > /dev/null 2>&1 &
run_pid=$!
sleep 2
vmm="" vfs=""
for pid in $(pgrep -f "peerspan-runtime launch"); do
  uid=$(awk '/^Uid:/ {print $2}' "/proc/$pid/status" 2>/dev/null || echo 0)
  if [ "$uid" -ge 1879048192 ]; then vmm=$pid; else root_launchers="${root_launchers:-} $pid"; fi
done
for pid in $(pgrep -f "virtiofsd --socket-path=.*peerspan-runtime"); do
  [ "$(readlink "/proc/$pid/ns/mnt")" != "$(readlink /proc/self/ns/mnt)" ] && vfs=$pid
done
if [ -z "$vmm" ]; then
  fail "confinement: no VMM running as a run user"
else
  ns_differs() { [ "$(readlink "/proc/$vmm/ns/$1")" != "$(readlink "/proc/self/ns/$1")" ]; }
  problems=""
  for ns in mnt pid net ipc uts; do ns_differs "$ns" || problems="$problems same-$ns-namespace"; done
  grep -q '^NoNewPrivs:[[:space:]]*1' "/proc/$vmm/status" || problems="$problems no_new_privs-unset"
  grep -q '^CapEff:[[:space:]]*0000000000000000' "/proc/$vmm/status" || problems="$problems capabilities-left"
  root=$(ls -A "/proc/$vmm/root" | tr '\n' ' ')
  [ "$root" = "dev initramfs proc rootfs sockets vmlinux " ] || problems="$problems root=[$root]"
  [ "$(ls "/proc/$vmm/root/proc" | grep -c '^[0-9]')" -eq 1 ] || problems="$problems sees-other-processes"
  [ -n "$vfs" ] || problems="$problems workspace-server-unsandboxed"
  [ -z "${root_launchers:-}" ] || problems="$problems launcher-as-root:${root_launchers}"
  [ -z "$problems" ] && pass confinement || fail "confinement:$problems"
fi
wait "$run_pid" || fail "confinement: the run failed"

[ "$failures" -eq 0 ] || { echo "$failures check(s) failed"; exit 1; }
echo "all checks passed"
