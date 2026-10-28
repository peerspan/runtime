# runtime

Peerspan's runtime: lightweight VMs for agent collaboration.

The runtime runs agent work in microVMs on machines you own, and on machines your peers let
you use. It is a separate process beside the Peerspan host daemon and peerfs, Peerspan's
filesystem for agents: the daemon accepts a task and checks access, peerfs provides the
workspace, and the runtime boots a microVM with that workspace attached and runs the work
inside it. Every runtime the host launches is a microVM, including your own work on your own
machines.

The VMM is [libkrun](https://github.com/containers/libkrun), through a thin fork at
[peerspan/libkrun](https://github.com/peerspan/libkrun), on Linux (KVM) and macOS (Apple
Silicon).

## Status

Early development. Nothing here is usable yet. The first goal is one person running a
delegated task's commands, such as a test suite, in a microVM on another of their own
machines, with the results coming back to the agent that asked.

## Trying it

Linux x86_64 with `/dev/kvm` only, for now, run as root (see Confinement). You need a kernel
build toolchain (`flex`, `bison`, `bc`, `libelf-dev`) for the guest kernel, libkrun built from
the `peerspan`
branch of [peerspan/libkrun](https://github.com/peerspan/libkrun) with
`make VHOST_USER=1 NET=1`, `virtiofsd`, `nft` (nftables), and the
`x86_64-unknown-linux-musl` Rust target.

```sh
scripts/build-guest.sh target/guest
export PEERSPAN_RUNTIME_GUEST=$PWD/target/guest
scripts/build-passt.sh target/release
export PEERSPAN_LIBKRUN=/path/to/libkrun/target/release/libkrun.so.2.0.0
cargo run --release -p peerspan-runtime -- run --workspace ~/src/project -- make test
scripts/check.sh   # end-to-end checks
```

`run` mounts the workspace at `/workspace` in a fresh microVM, with a read-only Alpine root
filesystem, runs the command there, streams its output, and exits with its code (124 on
`--timeout-seconds`, 127 if it never ran). `--archive FILE` unpacks a tar or tar.gz into an
empty workspace inside the VM, refusing absolute paths, `..` and anything but directories,
files and symbolic links; `--results-dir DIR` receives `changes.tar` and `deleted.txt`, what
the command changed. Until peerfs serves workspaces, `virtiofsd` stands in for it.
`--network` sets what the guest may reach (see Network).

`serve --socket PATH` is the local API the Peerspan host daemon drives: start a run, follow its
events with a long poll, cancel it, collect its result files and delete it. The runtime task
contract in the Peerspan host repository specifies it; `scripts/check-serve.py` exercises it.

## Network

Each run has a network level:

| Level | The guest reaches |
| --- | --- |
| `none` | Nothing: no network device, only loopback |
| `internet` (the default for `run`) | Public addresses only |
| `internet+lan` | Public addresses and private ranges |

At every level the guest is refused the host's own addresses, loopback, link-local addresses
(including the cloud metadata service at 169.254.169.254) and multicast; `internet` also
refuses private ranges (RFC 1918, shared address space, unique local IPv6). Refusals fail at
once rather than time out. `serve` caps what a run may ask for with `--max-network` (default
`internet`); a request without `network` gets `none`.

A guest with a network gets a virtio-net device whose other end is a
[passt](https://passt.top) process of its own, joined by a socket pair, so the VMM itself keeps
its empty network namespace. passt carries the guest's traffic as ordinary host sockets, so
the limits are enforced by the kernel: passt runs in the run's cgroup, and the runtime's
nftables table (`inet peerspan_runtime`) sends traffic from each run's cgroup to a chain for
its level, and rejects traffic from any other process in the runtime's cgroups, so a missing
rule fails closed. DNS goes to passt, which forwards it to the host's resolvers, the one local
address a guest may reach, on port 53 only. The guest's addresses are fixed (10.0.2.15/24,
gateway 10.0.2.2, DNS 10.0.2.3; fd50:5053::15/64, gateway fe80::1), so booting needs no DHCP
or router advertisements. The IPv6 address is unique local, so programs prefer IPv4 for names
that have both, and reach IPv6-only ones through the host when it has IPv6; on a host without,
they fail at once. The same limits apply in IPv6, including to IPv4 addresses behind the NAT64
prefixes.

The runtime ships with its own passt rather than the host's: `scripts/build-passt.sh` builds a
pinned release, checked against its commit and linked statically, and the runtime looks for it
beside its own binary (`--passt` or `PEERSPAN_PASST` to use another). passt is
GPL-2.0-or-later; its source is the pinned commit at [passt.top](https://passt.top/passt).

## Guest kernel

`scripts/build-kernel.sh` builds the guest kernel: a pinned kernel.org longterm release,
checked against its published SHA-256, configured by `guest/kernel.config` on top of
`allnoconfig`, with everything the guest needs built in (virtio-mmio, the virtio console,
virtio-fs, vsock, virtio-net, IPv4 and IPv6, and what programs expect of a kernel) and nothing else. It is cached by
version and config. `build-guest.sh` uses it by default and copies the resolved config into
the image beside `vmlinux`; `GUEST_ROOT=/` uses a distribution kernel and its modules instead.
The kernel is GPL-2.0: its source is the pinned release and its configuration is
`guest/kernel.config`.

## Measuring

`scripts/bench.py` boots a VM repeatedly (20 times by default, running `true`) and reports
p50, p90 and the maximum of:

- `connect_ms`: from starting the VM's process until the guest connects (VMM setup, kernel
  boot and init);
- `start_ms`: until the command starts;
- `total_ms`: the whole run, through teardown;
- `memory_peak_mib`: the most host memory the run's processes used, from its cgroup.

`--record` appends the result to `bench/history.jsonl`, with the commit, the kernel and the
settings, so each change can be compared with the last. `run --metrics FILE` writes one run's
measurements.

## Confinement

libkrun's devices act with the authority of the process that runs them, so each VM's process
is confined before libkrun sees any guest input:

- new mount, PID, network, IPC and UTS namespaces, so it has no network and sees only itself;
- a `pivot_root` into a root holding only the guest's read-only root filesystem, kernel and
  initramfs, the run's two sockets, `/dev/kvm`, and its own `/proc`;
- its own user and group, a fresh ID per run with only the `kvm` group, no capabilities,
  `no_new_privs`, and not dumpable;
- a seccomp allow-list, installed before libkrun starts any thread: the calls the VMM was
  seen to make across `scripts/check.sh`, those libkrun's read-only file server makes, and
  those the C and Rust runtimes make on rarer paths, about 80 in all. Sockets are Unix sockets
  only, `ioctl` takes only the 23 KVM commands that build and run the VM and a terminal check,
  `clone` cannot create namespaces, and `clone3`, whose flags seccomp cannot inspect, fails so
  the C library falls back to `clone`. Any other call kills the VMM; the run fails
  saying so, and the kernel log names the call.

Each run also gets its own cgroup v2, under `/sys/fs/cgroup/peerspan-runtime` by default
(`--cgroup-root`), which the VMM, the workspace server and passt join before they run any code
of their own. It caps their memory at the guest's plus 256 MiB with no swap, their CPU time at the
run's vCPUs, their processes and threads at 512, and their disk I/O on the disks holding the
workspace and the guest image at 256 MiB/s and 10,000 operations a second, each for reading and
for writing (`--io-mibps` and `--io-iops`, on `run` and, for every run, on `serve`). A run that
exceeds its memory is killed and reports so. Disk I/O is capped only where a block device holds
the files: not on tmpfs, overlayfs or btrfs, whose files report device numbers of their own. Stopping a run, by cancellation or timeout, kills the whole cgroup, which is
removed afterwards.

A supervisor killed outright cannot tear its run down. Every process of a run dies with it,
and a supervisor holds a lock on its run's cgroup for as long as the run lasts, so the next
run on the host finds runs whose lock is free and removes their cgroups, network filters and
run directories.

The workspace server runs in a user namespace of its own in which root is the workspace's
owner on the host, and no other user exists. There it builds `virtiofsd`'s namespace sandbox
and seccomp filter as usual, so its file access stays inside the workspace, but it holds no
privilege on the host and acts only as that user. In the guest, root owns the owner's files,
and files the guest creates belong to the owner on the host. Files of other users show as
`nobody` and are writable only as their permissions allow anyone, and the guest cannot give
files to other users. `run` serves a project directory as its owner, who must be able to reach
it; `serve` gives each workspace it creates to the run's own user, so its `--state` directory
must be searchable by other users.

Setting all this up needs root, as Firecracker's jailer does. Not yet: a limit on the disk
space a workspace may take.
`scripts/check.sh` inspects a running VM from the host to check the confinement above.

## Layout

- `crates/runtime`: the `peerspan-runtime` binary. `vm.rs` runs one VM; `run` and `serve` are
  its two front ends; `launch` is the per-VM child that confines itself and boots libkrun,
  which takes over its process.
- `crates/guest-init`: PID 1 in the guest, a static binary that mounts the filesystems,
  unpacks the workspace archive, runs the command, streams its output back over vsock, and
  reports what it changed.
- `crates/protocol`: the framing the two speak.
- `scripts/`: the guest image build and end-to-end checks.

## Contributing

We welcome bug reports, feature requests and design feedback as
[issues](https://github.com/peerspan/runtime/issues/new/choose). We do not accept code
contributions from outside Peerspan, and pull requests from outside are closed. See
[CONTRIBUTING.md](CONTRIBUTING.md).

## License

Apache-2.0. See [LICENSE](LICENSE) and [NOTICE](NOTICE).
