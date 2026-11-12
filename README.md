# runtime

Peerspan's runtime: lightweight VMs for agent collaboration.

The runtime runs agent work in microVMs, on your own machines and on machines your peers let
you use. It sits beside the Peerspan host daemon and peerfs: the daemon accepts a task and
checks access, peerfs provides the workspace, and the runtime boots a microVM with that
workspace and runs the work inside it.

The VMM is [libkrun](https://github.com/containers/libkrun), through a thin fork at
[peerspan/libkrun](https://github.com/peerspan/libkrun).

## Status

Early development; not usable yet. The first goal is running a delegated task's commands, such
as a test suite, in a microVM on another of your own machines.

Today: Linux x86_64 with KVM, run as root. macOS (Apple Silicon) is planned.

## Install

On Linux x86_64 with KVM, as root:

```sh
curl -fsSL https://raw.githubusercontent.com/peerspan/runtime/main/scripts/install.sh | sudo sh
sudo peerspan-runtime run --image node:22 --workspace ~/src/project -- npm test
```

A release bundles the runtime with libkrun, passt and the guest image. The host needs
`virtiofsd`, `nft` and `mkfs.ext4` (on Debian and Ubuntu:
`apt-get install virtiofsd nftables e2fsprogs`). `peerspan-runtime doctor` checks everything a
run needs and says what to fix.

- **Layout:** each version goes in `/usr/local/lib/peerspan-runtime/VERSION`, with `current`
  linking the installed one and `/usr/local/bin/peerspan-runtime` linking its binary.
- **Options:** `install.sh` takes `--version`, `--prefix`, `--from TARBALL` and
  `--no-service`.
- **Service:** where systemd runs, the `serve` unit is installed but not enabled. Its settings
  go in `/etc/default/peerspan-runtime`, for example
  `PEERSPAN_RUNTIME_ARGS="--client-uid 1000"`. Start it with
  `systemctl enable --now peerspan-runtime`.

## Build from source

Needs a kernel build toolchain (`flex`, `bison`, `bc`, `libelf-dev`), `clang` and
`libclang-dev` for libkrun, and the `x86_64-unknown-linux-musl` Rust target.

```sh
scripts/build-guest.sh target/release/guest   # guest kernel, init and root filesystem
scripts/build-libkrun.sh target/release       # pinned libkrun, beside the binary
scripts/build-passt.sh target/release         # pinned passt, beside the binary
cargo build --release -p peerspan-runtime
sudo target/release/peerspan-runtime doctor
sudo scripts/check.sh                         # end-to-end checks
```

The runtime looks for libkrun, passt and the guest image beside its own binary.
`--libkrun`, `--passt` and `--guest`, or `PEERSPAN_LIBKRUN`, `PEERSPAN_PASST` and
`PEERSPAN_RUNTIME_GUEST`, name others. `scripts/build-release.sh OUT_DIR` builds a release
tarball. Its binaries need the glibc of the host that built them, so build releases on the
oldest distribution to support.

## `run`

Runs one command in a fresh microVM with the workspace at `/workspace`, streams its output, and
exits with its code.

| Option | Effect |
| --- | --- |
| `--workspace DIR` | The directory to mount at `/workspace` |
| `--peerfs-workspace NAME` | Instead, a peerfs workspace, from the space `--peerfs-space` (or `PEERFS_SPACE`); see below |
| `--fork-from BRANCH` | Create `--peerfs-workspace` first, as a fork of BRANCH; see below |
| `--image REF` | OCI image to boot, such as `node:22` (default: a minimal Alpine); see Images |
| `--pull` | Pull the image again, to update its tag |
| `--archive FILE` | Unpack a tar or tar.gz into an empty workspace first |
| `--results-dir DIR` | Write `changes.tar` and `deleted.txt`: what the command changed |
| `--network LEVEL` | `none`, `internet` (default) or `internet+lan`; see Network |
| `--stdin` | Pass this program's standard input to the command (otherwise empty) |
| `--combined-output` | Send stderr to stdout, like `2>&1`, keeping the order of writes |
| `--timeout-seconds N` | Stop the command after N seconds |
| `--cpus`, `--memory-mib` | VM size (default 2 vCPUs, 1024 MiB) |
| `--io-mibps`, `--io-iops` | Disk I/O caps (default 256 MiB/s, 10,000 IOPS) |
| `--cwd`, `--env NAME=VALUE` | Working directory and environment |
| `--secret NAME=FILE` | Write FILE's contents in the guest to `/run/secrets/NAME`, in memory and readable only by root; repeatable, up to 16 of 64 KiB each. Never in the workspace, its history or the logs |
| `--harness DIR` | Mount DIR read-only at `/opt/peerspan/harness`, for a tool the image lacks, such as an agent's static binary; the VM's own user must be able to read it |

Exit codes: the command's own, 128 + N if a signal ended it, 124 on timeout, 127 if it never
ran. Archives may hold only directories, files and symbolic links, with no absolute paths or
`..`.

With `--peerfs-workspace`, a peerfs process of the run's own serves the workspace to the VM as a
vhost-user-fs device in place of `virtiofsd`, so what the command changes lands in the
workspace's history, where `peerfs ws snapshot` and `ws commit` take it. The runtime starts the
space's peerfs server if none runs, then runs `peerfs ws vhost-user` (`--peerfs` or
`PEERSPAN_PEERFS`) in the run's cgroup. That process borrows the workspace from the server,
which refuses other changes to it until the run ends, and confines itself before serving the
guest (see Confinement). The runtime snapshots the workspace before the command and after it,
so `peerfs ws snapshots` lists what each run changed. A workspace serves one run at a time. `--archive` and `--results-dir`
take a workspace directory only: the workspace's history holds what the command changed.

With `--fork-from BRANCH`, the run first makes a branch named after `--peerfs-workspace` at
BRANCH's head (`peerfs fork`), then the workspace following it (`peerfs ws create`), so each
run, such as one agent task, works on a fork of its own and BRANCH is left alone. The run fails
before the VM starts if the workspace or the branch exists already, so a fork never lands on
other work. Committing the fork and merging it are left to the caller.

## `serve`

`serve --socket PATH` is the local API the Peerspan host daemon drives: start a run, follow its
events, cancel it, collect its result files, delete it. The runtime task contract in the
Peerspan host repository specifies it; `scripts/check-serve.py` exercises it.

`--image REF` sets the image every run boots. Host-wide limits are flags: `--max-cpus`, `--max-memory-mib`, `--max-seconds`,
`--max-network`, `--max-concurrent-runs`, `--io-mibps`, `--io-iops` and `--workspace-mib`.
Runs keep their state under `--state` (default `/var/lib/peerspan-runtime`), which must be
searchable by other users, and only one `serve` may use a state directory at a time. Runs
last only as long as the `serve` that started them: a restarted `serve` removes what the last
one left (run directories, workspaces, cgroups and network rules), and the daemon fails the
tasks whose runs it no longer knows. `--stdin` and `--combined-output` are not available here yet, since
the contract has neither.

`serve --peerfs-space DIR` lets a run name a workspace of that space with `peerfs_workspace`
in place of `workspace_archive`, served as `run --peerfs-workspace` serves it, as the runtime
task contract's peerfs workspaces specify:
- `GET /v1/runtime` reports `peerfs: true`; without a space, such a run is refused with 422.
- The run takes `results` `none` or `output`, and its `exited` result gains `snapshot`: the
  snapshot after the command, or null when it changed nothing.
- Runs on one workspace take turns: while one holds it, another is refused with 429.
- `peerfs_fork_from`, a branch, creates `peerfs_workspace` first as a fork of it, as
  `run --fork-from` does. A workspace or branch of that name already there fails the run, with
  the reason in its result's `error`.
- The workspace's size is not capped.

`serve` logs to standard error, which the systemd unit sends to the journal, one line per
event with fields:

```
2026-10-28T06:42:20.225Z info run finished run=run_… exit_code=3 duration_ms=12 boot_ms=305 memory_peak_mib=36 stdout_bytes=3 stderr_bytes=0
```

It logs its settings at startup, each run accepted, refused, started, finished, cancelled or
deleted, and what it cleans up. A run's arguments and environment are never logged, only its
program's name.

## Images

`--image` takes the references Docker does: `node:22`, `python:3.12-slim`,
`ghcr.io/org/tool:1.2`, or `name@sha256:...`. `pull REF` fetches an image ahead of time, or
updates a tag to what it names now. `images` lists what is cached, with sizes and last use.
`prune` removes images no tag names, such as those a re-pulled tag moved away from, and with
`--unused-days N` also those no run has booted for N days. Images in use are never removed.

- An image is pulled once, checked against its digests, and kept under
  `/var/cache/peerspan-runtime/images` (`--image-cache`).
- A tag keeps naming what it named when it was pulled, until it is pulled again.
- The image's environment applies under `--env`. Its entrypoint, user and working directory
  are ignored: the command runs as root in `--cwd`.
- The root is writable: `apt-get`, `pip` and `npm -g` work. Writes outside the workspace go to
  a layer in the guest's memory and are gone when the run ends, and the image is never changed.
- Image files are made readable by all on the host, because the VMM serves them as an
  unprivileged user, so the guest sees looser modes on them.
- Not yet: private registries, zstd layers, and platforms other than linux/amd64.

## Network

| Level | The guest reaches |
| --- | --- |
| `none` | Nothing; loopback only |
| `internet` | Public addresses |
| `internet+lan` | Public addresses and private ranges |

- Always refused: the host's own addresses, loopback, link-local (including the cloud metadata
  service at 169.254.169.254) and multicast. Refusals fail at once rather than time out.
- DNS goes through the host's resolvers.
- IPv4 and IPv6, with fixed guest addresses (10.0.2.15/24; fd50:5053::15/64), so boot needs no
  DHCP.
- `serve` defaults to `none` and caps requests at `--max-network` (default `internet`).

Each guest's traffic goes through its own [passt](https://passt.top) process and leaves as
ordinary host sockets. An nftables table (`inet peerspan_runtime`) filters it by the run's
cgroup and fails closed.

## Confinement

Setup needs root, as Firecracker's jailer does. Each VM's process is confined before libkrun
sees any guest input:

- **Namespaces:** new mount, PID, network, IPC and UTS namespaces.
- **Filesystem:** a `pivot_root` into a root holding only the guest image, the run's sockets,
  `/dev/kvm` and its own `/proc`.
- **Identity:** a fresh user per run, in the `kvm` group only, with no capabilities, under
  `no_new_privs`.
- **seccomp:** an allow-list of about 80 system calls. Sockets must be Unix sockets, `ioctl`
  takes only the KVM commands a VM needs, and `clone` cannot create namespaces. Any other call
  kills the VMM, and the run says so.

The workspace server for a directory, `virtiofsd`, runs as the workspace's owner, in a user
namespace where that user is root:
- The guest's root owns the owner's files.
- New files belong to the owner on the host.
- Other users' files show as `nobody`.

`serve` gives each workspace it creates to the run's own user.

A peerfs workspace's server confines itself the same way before it reads anything the guest
sends: new user, mount, network, IPC and UTS namespaces; a root holding only the space's objects,
read-only, and the run's workspace; one capability, to override permissions on its own user's
files; and a seccomp allow-list. A fault in it exposes that workspace and the space's stored
history, which never holds peerfs's secret-class files, but not the space's other workspaces or
its signing key.

### Limits

Each run gets a cgroup (under `/sys/fs/cgroup/peerspan-runtime` by default) holding the VMM, the
workspace server and passt.

| Resource | Limit |
| --- | --- |
| Memory | The guest's plus 256 MiB, no swap; exceeding it kills the run |
| CPU | The run's vCPUs |
| Processes and threads | 512 |
| Disk I/O | 256 MiB/s and 10,000 IOPS each way, on the disks behind the workspace and guest image (not tmpfs, overlayfs or btrfs) |
| Workspace size (`serve`) | 10 GiB, an ext4 image per run |
| Guest console log | 16 MiB |
| Server logs | 1 MiB each |
| Output and changes (`serve`) | 100 MiB each; larger changes are left out |

Stopping a run kills its whole cgroup. If a supervisor dies, the next run on the host removes
its leftovers: processes, cgroup, network rules and run directory. `serve` also unmounts
leftover workspace images when it starts.

## Guest

- **Kernel:** `scripts/build-kernel.sh` builds a pinned kernel.org longterm release, verified
  against its SHA-256, from `allnoconfig` plus `guest/kernel.config`. It is GPL-2.0.
- **passt:** `scripts/build-passt.sh` builds a pinned release, verified against its commit.
  It is GPL-2.0-or-later, with source at [passt.top](https://passt.top/passt).
- **Root filesystem:** the run's image, or the bundled minimal Alpine, under a writable layer
  in memory.
- **init:** `guest-init`, a static binary.

## Measuring

`scripts/bench.py` boots VMs repeatedly and reports p50, p90 and maximum boot time (`connect_ms`,
`start_ms`, `total_ms`) and peak host memory. `--record` appends the result to
`bench/history.jsonl`. `run --metrics FILE` writes one run's measurements.

## Layout

| Path | What it holds |
| --- | --- |
| `crates/runtime` | The `peerspan-runtime` binary. `vm.rs` runs one VM; `run` and `serve` are its front ends; `launch` confines itself and boots libkrun |
| `crates/guest-init` | The guest's PID 1: mounts, unpacks the workspace, runs the command, streams output over vsock |
| `crates/protocol` | The framing host and guest speak |
| `scripts/` | Guest image builds, benchmarks and end-to-end checks |

## Contributing

Bug reports, feature requests and design feedback are welcome as
[issues](https://github.com/peerspan/runtime/issues/new/choose). We do not accept outside code
contributions; see [CONTRIBUTING.md](CONTRIBUTING.md).

## License

Apache-2.0. See [LICENSE](LICENSE) and [NOTICE](NOTICE).
