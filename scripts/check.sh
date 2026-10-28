#!/bin/sh
# SPDX-License-Identifier: Apache-2.0
# Copyright The Peerspan Authors
#
# End-to-end checks: boots real microVMs with `peerspan-runtime run`.
# Needs root, /dev/kvm, a guest image (scripts/build-guest.sh), PEERSPAN_RUNTIME_GUEST pointing
# at it, PEERSPAN_LIBKRUN pointing at a libkrun built with `make VHOST_USER=1 NET=1`, passt
# from scripts/build-passt.sh beside the runtime binary or named by PEERSPAN_PASST, and nft and
# ip on PATH.
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
run_cgroups() { find /sys/fs/cgroup/peerspan-runtime -maxdepth 1 -name 'run-*' 2>/dev/null | wc -l; }
cgroups_before=$(run_cgroups)

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

# The workspace belongs to a user other than root, as a project directory would, so the checks
# run with the workspace server as that user. It must be able to reach the workspace.
owner=4242:4343
chmod 711 "$work"
mkdir -p "$work/ws"
echo "from the host" > "$work/ws/from-host"
chown -R "$owner" "$work/ws"

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

# The guest's root acts on the host as the workspace's owner and no one else: the owner's files
# are root's, others' are nobody's and not writable, and files cannot be given away.
touch "$work/ws/roots"
if check workspace-owner 0 -- sh -c 'stat -c %u from-host roots; echo x > mine
    chown 1 mine 2>/dev/null && echo gave-away; echo x 2>/dev/null >> roots && echo wrote-roots; true'; then
  got="$(tr '\n' ' ' < "$work/out")$(stat -c %u:%g "$work/ws/mine")"
  [ "$got" = "0 65534 $owner" ] && pass workspace-owner || fail "workspace-owner: $got"
fi
rm -f "$work/ws/roots" "$work/ws/mine"


if "$bin" run --workspace "$work/ws" --cwd /tmp --env GREETING=hi -- sh -c 'echo "$GREETING $(pwd)"' \
    > "$work/out" 2> "$work/err"; then
  [ "$(cat "$work/out")" = "hi /tmp" ] && pass env-and-cwd || fail "env-and-cwd: $(cat "$work/out")"
else
  fail "env-and-cwd: exit $?"
fi

# Writes outside the workspace go to a layer that ends with the VM: they succeed, never reach
# the image, and are gone in the next run.
if check throwaway-root 0 -- sh -c 'echo x > /etc/peerspan-check && ls /root'; then
  if [ -e "$PEERSPAN_RUNTIME_GUEST/rootfs/etc/peerspan-check" ]; then
    fail "throwaway-root: the write reached the image"
  elif check throwaway-root 1 -- test -e /etc/peerspan-check; then
    pass throwaway-root
  fi
fi

# OCI images: a pulled image boots, by tag and by the digest pull reports, with its own
# environment under the run's. Needs Docker Hub.
images="$work/images"
digest=$("$bin" pull --image-cache "$images" alpine:3.20 2> "$work/err") || digest=""
got=$("$bin" run --workspace "$work/ws" --image-cache "$images" --image alpine:3.20 --env HOME=/x -- \
  sh -c 'cut -d. -f1,2 /etc/alpine-release; echo "$PATH $HOME"' 2>> "$work/err" | tr '\n' ' ') || true
by_digest=$("$bin" run --workspace "$work/ws" --image-cache "$images" --image "alpine@$digest" -- \
  cut -d. -f1,2 /etc/alpine-release 2>> "$work/err") || true
want_path=/usr/local/sbin:/usr/local/bin:/usr/sbin:/usr/bin:/sbin:/bin
[ "$got" = "3.20 $want_path /x " ] && [ "$by_digest" = 3.20 ] && pass image \
  || fail "image: [$got] by digest [$by_digest] $(tail -3 "$work/err")"

# Standard input reaches the command with --stdin, intact and with its end; without it the
# command's is empty, and a command that ignores endless input still finishes.
head -c 5000000 /dev/urandom > "$work/stdin.bin"
want=$(sha256sum < "$work/stdin.bin" | cut -d' ' -f1)
got=$("$bin" run --workspace "$work/ws" --stdin -- sh -c 'sha256sum | cut -d" " -f1' \
  < "$work/stdin.bin" 2> "$work/err") || true
empty=$(echo hi | "$bin" run --workspace "$work/ws" -- wc -c 2> "$work/err") || true
rc=0
yes | timeout 60 "$bin" run --workspace "$work/ws" --stdin -- true > /dev/null 2>&1 || rc=$?
[ "$got" = "$want" ] && [ "$empty" = 0 ] && [ "$rc" -eq 0 ] && pass stdin \
  || fail "stdin: digest ${got:-none}, empty input gave ${empty:-none}, ignored input exit $rc"
rm -f "$work/stdin.bin"

# --combined-output keeps the order of writes across the two streams.
got=$("$bin" run --workspace "$work/ws" --combined-output -- \
  sh -c 'for i in 1 2 3; do echo out$i; echo err$i >&2; done' 2> "$work/err" | tr '\n' ' ') || true
[ "$got" = "out1 err1 out2 err2 out3 err3 " ] && [ ! -s "$work/err" ] && pass combined-output \
  || fail "combined-output: [$got] stderr [$(cat "$work/err")]"

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

# The guest console is kept to 16 MiB on the host, however much the guest writes to it.
rc=0
"$bin" run --workspace "$work/ws" --console-log "$work/flood.log" -- \
  sh -c 'head -c 24000000 /dev/zero | tr "\\0" x > /dev/console' > /dev/null 2>&1 || rc=$?
size=$(stat -c %s "$work/flood.log")
[ "$rc" -eq 0 ] && [ "$size" -le $(( (16 << 20) + 100 )) ] && grep -q "log cut at" "$work/flood.log" \
  && pass console-limit || fail "console-limit: exit $rc, $size bytes"
rm -f "$work/flood.log"

# Disk I/O is capped: 60 MiB written and synced at 20 MiB/s takes about 3 seconds.
start=$(date +%s%3N); rc=0
"$bin" run --workspace "$work/ws" --io-mibps 20 -- \
  sh -c 'dd if=/dev/zero of=io-check bs=1M count=60 conv=fsync 2>/dev/null; rm io-check' \
  > /dev/null 2>&1 || rc=$?
took=$(( $(date +%s%3N) - start ))
[ "$rc" -eq 0 ] && [ "$took" -ge 2500 ] && pass io-limit || fail "io-limit: exit $rc after ${took}ms"

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
for pid in $(pgrep -f "virtiofsd --fd=3 --shared-dir=$work/ws"); do
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
  grep -q '^Seccomp:[[:space:]]*2' "/proc/$vmm/status" || problems="$problems no-seccomp-filter"
  grep -q '^Seccomp_filters:[[:space:]]*2' "/proc/$vmm/status" || problems="$problems seccomp-filters-not-2"
  root=$(ls -A "/proc/$vmm/root" | tr '\n' ' ')
  [ "$root" = "dev initramfs proc rootfs sockets vmlinux " ] || problems="$problems root=[$root]"
  [ "$(ls "/proc/$vmm/root/proc" | grep -c '^[0-9]')" -eq 1 ] || problems="$problems sees-other-processes"
  [ -n "$vfs" ] || problems="$problems workspace-server-unsandboxed"
  if [ -n "$vfs" ]; then
    [ "$(readlink "/proc/$vfs/ns/user")" != "$(readlink /proc/self/ns/user)" ] \
      || problems="$problems workspace-server-in-host-user-namespace"
    ids="$(awk '/^Uid:/ {print $3}' "/proc/$vfs/status"):$(awk '/^Gid:/ {print $3}' "/proc/$vfs/status")"
    [ "$ids" = "$owner" ] || problems="$problems workspace-server-as-$ids"
  fi
  [ -z "${root_launchers:-}" ] || problems="$problems launcher-as-root:${root_launchers}"
  # Resource limits: the VMM and the workspace server share the run's cgroup, which caps memory
  # at the guest's 1024 MiB plus 256 MiB, CPU at 2 vCPUs, processes at 512, and disk I/O at
  # 256 MiB/s and 10000 operations a second each way.
  cg=$(sed -n 's/^0:://p' "/proc/$vmm/cgroup")
  case "$cg" in /peerspan-runtime/run-*) ;; *) problems="$problems cgroup=[$cg]" ;; esac
  [ -n "$vfs" ] && [ "$(sed -n 's/^0:://p' "/proc/$vfs/cgroup")" != "$cg" ] \
    && problems="$problems workspace-server-outside-cgroup"
  limits="$(cat "/sys/fs/cgroup$cg/memory.max") $(cat "/sys/fs/cgroup$cg/memory.swap.max")"
  limits="$limits $(cat "/sys/fs/cgroup$cg/cpu.max") $(cat "/sys/fs/cgroup$cg/pids.max")"
  [ "$limits" = "1342177280 0 200000 100000 512" ] || problems="$problems limits=[$limits]"
  grep -q ' rbps=268435456 wbps=268435456 riops=10000 wiops=10000$' "/sys/fs/cgroup$cg/io.max" \
    || problems="$problems io.max=[$(cat "/sys/fs/cgroup$cg/io.max")]"
  [ -z "$problems" ] && pass confinement || fail "confinement:$problems"
fi
wait "$run_pid" || fail "confinement: the run failed"
[ -n "${cg:-}" ] && [ -d "/sys/fs/cgroup$cg" ] && fail "confinement: cgroup $cg left behind"

# Network levels (decision 5). Targets: a public address; the metadata address; the host's own
# addresses (public, Docker's bridge if any, and a veth); a private peer that is not the host
# (a network namespace behind that veth); the gateway address and host loopback. In IPv6, which
# neither this host nor CI may have, the peer stands in for a public address too, with one from
# the documentation prefix. TCP probes connect; UDP probes send one datagram that the host side
# counts.
pub=$(hostname -I | awk '{print $1}')
docker=$(ip -4 -o addr show docker0 2>/dev/null | awk '{print $4}' | cut -d/ -f1)
ns=pscheck$$
ip netns add "$ns"
ip link add "${ns}a" type veth peer name "${ns}b"
ip link set "${ns}b" netns "$ns"
ip addr add 10.203.0.1/24 dev "${ns}a" && ip link set "${ns}a" up
ip -n "$ns" addr add 10.203.0.2/24 dev "${ns}b" && ip -n "$ns" link set "${ns}b" up
for net in fd20:3 2001:db8:203; do
  ip addr add "$net::1/64" dev "${ns}a" nodad
  ip -n "$ns" addr add "$net::2/64" dev "${ns}b" nodad
done
listen='import socket, sys, time
kind, host, port, log = sys.argv[1], sys.argv[2], int(sys.argv[3]), sys.argv[4]
family = socket.AF_INET6 if ":" in host else socket.AF_INET
s = socket.socket(family, socket.SOCK_DGRAM if kind == "udp" else socket.SOCK_STREAM)
s.setsockopt(socket.SOL_SOCKET, socket.SO_REUSEADDR, 1)
if family == socket.AF_INET6:
    s.setsockopt(socket.IPPROTO_IPV6, socket.IPV6_V6ONLY, 1)
s.bind((host, port))
if kind == "tcp":
    s.listen(16)
    time.sleep(600)
while True:
    s.recv(64)
    open(log, "a").write("hit\n")'
python3 -c "$listen" tcp 0.0.0.0 47011 - & listeners=$!
python3 -c "$listen" tcp :: 47011 - & listeners="$listeners $!"
python3 -c "$listen" tcp 127.0.0.1 47013 - & listeners="$listeners $!"
python3 -c "$listen" tcp ::1 47013 - & listeners="$listeners $!"
python3 -c "$listen" udp 0.0.0.0 47014 "$work/udp-host" & listeners="$listeners $!"
ip netns exec "$ns" python3 -c "$listen" tcp 10.203.0.2 47012 - & listeners="$listeners $!"
ip netns exec "$ns" python3 -c "$listen" udp 10.203.0.2 47015 "$work/udp-peer" & listeners="$listeners $!"
ip netns exec "$ns" python3 -c "$listen" tcp fd20:3::2 47012 - & listeners="$listeners $!"
ip netns exec "$ns" python3 -c "$listen" tcp 2001:db8:203::2 47016 - & listeners="$listeners $!"
trap 'kill $listeners 2>/dev/null; ip netns del "$ns"; ip link del "${ns}a" 2>/dev/null; rm -rf "$work"' EXIT
sleep 0.5
targets="1.1.1.1:80 169.254.169.254:80 $pub:47011 ${docker:+$docker:47011} 10.203.0.1:47011"
targets="$targets 10.203.0.2:47012 10.0.2.2:47013 127.0.0.1:47013"
targets="$targets 2001:db8:203::2:47016 fd20:3::2:47012 2001:db8:203::1:47011 fd20:3::1:47011"
targets="$targets ::1:47013"
probe='for t in '$targets'; do
  nc -w 3 ${t%:*} ${t##*:} </dev/null 2>/dev/null && echo "$t open" || echo "$t refused"
done
for t in '$pub':47014 10.203.0.2:47015; do echo hit | nc -u -w 1 ${t%:*} ${t##*:} 2>/dev/null; done
nslookup example.com >/dev/null 2>&1 && echo "dns open" || echo "dns refused"
echo "interfaces $(ls /sys/class/net | tr "\n" " ")"'
# expect LEVEL REACHABLE...: what a guest at LEVEL reaches, everything else refused.
expect_network() {
  level=$1
  shift
  : > "$work/udp-host"
  : > "$work/udp-peer"
  if ! "$bin" run --network "$level" --workspace "$work/ws" -- sh -c "$probe" \
    > "$work/out" 2> "$work/err"; then
    fail "network-$level: run failed: $(tail -3 "$work/err")"
    return
  fi
  problems=""
  for t in $targets; do
    want=refused
    for open in "$@"; do [ "$open" = "$t" ] && want=open; done
    grep -qx "$t $want" "$work/out" || problems="$problems $t-not-$want"
  done
  sleep 0.2
  [ -s "$work/udp-host" ] && problems="$problems udp-reached-host"
  case " $* " in
    *" 10.203.0.2:47012 "*) [ -s "$work/udp-peer" ] || problems="$problems udp-peer-not-reached" ;;
    *) [ -s "$work/udp-peer" ] && problems="$problems udp-reached-peer" ;;
  esac
  dns=refused
  [ "$level" = none ] || dns=open
  grep -qx "dns $dns" "$work/out" || problems="$problems dns-not-$dns"
  interfaces="eth0 lo "
  [ "$level" = none ] && interfaces="lo "
  grep -qx "interfaces $interfaces" "$work/out" || problems="$problems $(grep interfaces "$work/out")"
  [ -z "$problems" ] && pass "network-$level" || fail "network-$level:$problems"
}
expect_network none
expect_network internet 1.1.1.1:80 2001:db8:203::2:47016
expect_network internet+lan 1.1.1.1:80 10.203.0.2:47012 2001:db8:203::2:47016 fd20:3::2:47012

# Fail closed: a process in the runtime's cgroups with no run chain reaches nothing.
probe_cg=/sys/fs/cgroup/peerspan-runtime/fail-closed-$$
mkdir "$probe_cg"
closed=$(sh -c "echo \$\$ > $probe_cg/cgroup.procs; exec python3 -c '
import socket
s = socket.socket(); s.settimeout(3)
try:
    s.connect((\"1.1.1.1\", 80)); print(\"open\")
except OSError:
    print(\"refused\")'")
rmdir "$probe_cg"
[ "$closed" = refused ] && pass network-fail-closed || fail "network-fail-closed: $closed"

left=$(nft list table inet peerspan_runtime | grep -c -e 'chain run-' -e '"peerspan-runtime/run-' || true)
[ "$left" -eq 0 ] && pass network-cleanup || fail "network-cleanup: $left run entries left in nftables"

# A supervisor killed outright: its run's processes die with it, and the next run removes the
# cgroup, network filter and run directory it left.
ls -d /sys/fs/cgroup/peerspan-runtime/run-* > "$work/runs-before" 2>/dev/null || true
"$bin" run --network internet --workspace "$work/ws" -- sleep 60 > /dev/null 2>&1 &
supervisor=$!
# Wait until the run is up: its cgroup and network filter exist.
dead=""
for _ in $(seq 100); do
  dead=$(ls -d /sys/fs/cgroup/peerspan-runtime/run-* 2>/dev/null | grep -vxF -f "$work/runs-before" | head -1)
  [ -n "$dead" ] && nft list chain inet peerspan_runtime "${dead##*/}" > /dev/null 2>&1 && break
  sleep 0.1
done
kill -9 "$supervisor"
wait "$supervisor" 2>/dev/null || true
problems=""
if [ -z "$dead" ]; then
  problems=" no-run-started"
else
  sleep 1
  [ -s "$dead/cgroup.procs" ] && problems="$problems processes-outlived-supervisor"
  check stale-next 0 -- true || problems="$problems next-run-failed"
  [ -d "$dead" ] && problems="$problems cgroup-left"
  nft list chain inet peerspan_runtime "${dead##*/}" > /dev/null 2>&1 && problems="$problems filter-left"
  [ -d "${TMPDIR:-/tmp}/peerspan-runtime-${dead##*/run-}" ] && problems="$problems run-dir-left"
fi
[ -z "$problems" ] && pass stale-run || fail "stale-run:$problems"

leaked=$(( $(run_cgroups) - cgroups_before ))
[ "$leaked" -le 0 ] && pass cgroup-cleanup || fail "cgroup-cleanup: $leaked run cgroups left behind"

[ "$failures" -eq 0 ] || { echo "$failures check(s) failed"; exit 1; }
echo "all checks passed"
