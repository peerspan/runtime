#!/bin/sh
# SPDX-License-Identifier: Apache-2.0
# Copyright The Peerspan Authors
#
# End-to-end checks: boots real microVMs with `peerspan-runtime run`.
# Needs /dev/kvm, a guest image (scripts/build-guest.sh), PEERSPAN_RUNTIME_GUEST pointing at
# it, and PEERSPAN_LIBKRUN pointing at a libkrun built with `make VHOST_USER=1`.
set -eu

repo=$(cd "$(dirname "$0")/.." && pwd)
cargo build --quiet --release --manifest-path "$repo/Cargo.toml" -p peerspan-runtime
bin="$repo/target/release/peerspan-runtime"
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

[ "$failures" -eq 0 ] || { echo "$failures check(s) failed"; exit 1; }
echo "all checks passed"
