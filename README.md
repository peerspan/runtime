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

## Contributing

We welcome bug reports, feature requests and design feedback as
[issues](https://github.com/peerspan/runtime/issues/new/choose). We do not accept code
contributions from outside Peerspan, and pull requests from outside are closed. See
[CONTRIBUTING.md](CONTRIBUTING.md).

## License

Apache-2.0. See [LICENSE](LICENSE) and [NOTICE](NOTICE).
