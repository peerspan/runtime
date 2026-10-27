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

Linux x86_64 with `/dev/kvm` only, for now. You need libkrun built from the `peerspan`
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
filesystem, runs the command there, streams its output, and exits with its code. Until peerfs
serves workspaces, `virtiofsd` stands in for it. The VMM process is not yet confined, and the
guest has no network.

## Layout

- `crates/runtime`: the `peerspan-runtime` binary. `run` supervises one VM; `launch` is the
  per-VM child that boots libkrun, which takes over its process.
- `crates/guest-init`: PID 1 in the guest, a static binary that mounts the filesystems, runs
  the command and streams its output back over vsock.
- `crates/protocol`: the framing the two speak.
- `scripts/`: the guest image build and end-to-end checks.

## Contributing

We welcome bug reports, feature requests and design feedback as
[issues](https://github.com/peerspan/runtime/issues/new/choose). We do not accept code
contributions from outside Peerspan, and pull requests from outside are closed. See
[CONTRIBUTING.md](CONTRIBUTING.md).

## License

Apache-2.0. See [LICENSE](LICENSE) and [NOTICE](NOTICE).
