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

Linux x86_64 with `/dev/kvm` only, for now, run as root (see Confinement). You need libkrun
built from the `peerspan`
branch of [peerspan/libkrun](https://github.com/peerspan/libkrun) with `make VHOST_USER=1`,
`virtiofsd`, the `x86_64-unknown-linux-musl` Rust target, and a guest kernel with virtio-mmio
command-line devices (Ubuntu's generic kernel works).

```sh
scripts/build-guest.sh target/guest
export PEERSPAN_RUNTIME_GUEST=$PWD/target/guest
export PEERSPAN_LIBKRUN=/path/to/libkrun/target/release/libkrun.so.2.0.0
cargo run --release -p peerspan-runtime -- run --workspace ~/src/project -- make test
scripts/check.sh   # end-to-end checks
```

`run` mounts the workspace at `/workspace` in a fresh microVM, with a read-only Alpine root
filesystem, runs the command there, streams its output, and exits with its code (124 on
`--timeout-seconds`, 127 if it never ran). `--archive FILE` unpacks a tar or tar.gz into an
empty workspace inside the VM, refusing absolute paths, `..` and anything but directories,
files and symbolic links; `--results-dir DIR` receives `changes.tar` and `deleted.txt`, what
the command changed. Until peerfs serves workspaces, `virtiofsd` stands in for it. The guest
has no network.

`serve --socket PATH` is the local API the Peerspan host daemon drives: start a run, follow its
events with a long poll, cancel it, collect its result files and delete it. The runtime task
contract in the Peerspan host repository specifies it; `scripts/check-serve.py` exercises it.

## Confinement

libkrun's devices act with the authority of the process that runs them, so each VM's process
is confined before libkrun sees any guest input:

- new mount, PID, network, IPC and UTS namespaces, so it has no network and sees only itself;
- a `pivot_root` into a root holding only the guest's read-only root filesystem, kernel and
  initramfs, the run's two sockets, `/dev/kvm`, and its own `/proc`;
- its own user and group, a fresh ID per run with only the `kvm` group, no capabilities,
  `no_new_privs`, and not dumpable;
- a seccomp filter, installed before libkrun starts any thread, that refuses with `EPERM` the
  calls a VMM never needs: `execve`, mounts and namespace changes, `ptrace` and cross-process
  memory access, `bpf`, `perf_event_open`, `userfaultfd`, keyrings, kernel modules, `kexec`,
  and host-wide settings such as the clock and `reboot`. It is a deny-list for now; an
  allow-list traced from libkrun's actual calls would be tighter.

The workspace server runs in `virtiofsd`'s namespace sandbox with its seccomp filter, so its
file access stays inside the workspace. Setting this up needs root, as Firecracker's jailer
does. Not yet: resource limits through cgroups, a seccomp allow-list, and running the workspace
server as the run's user. `scripts/check.sh` inspects a running VM from the host to
check the confinement above.

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
