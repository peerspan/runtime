// SPDX-License-Identifier: Apache-2.0
// Copyright The Peerspan Authors

//! A run's host ports: the host's half of carrying a TCP port on the guest's loopback to a Unix
//! socket on the host.
//!
//! The guest's init carries each connection to the port over a vsock port of the entry's own,
//! which the VMM maps to a socket in the run's sockets directory, as for a linked run's
//! `connect` side. The forwarder here accepts on that socket and, for each connection, connects
//! to the entry's socket on the host and copies bytes both ways, reading none of them. It
//! connects as root, so it carries a connection only to a listener running as the runtime's
//! client user. It bounds the connections open at once and nothing else; dropping it closes
//! every connection.

use std::collections::HashMap;
use std::io::{self, ErrorKind, Read, Write};
use std::net::Shutdown;
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::Duration;

use rustix::net::{AddressFamily, SocketAddrUnix, SocketFlags, SocketType};

/// Most connections one host port carries at once.
pub const MAX_CONNECTIONS: usize = 64;

/// How often the forwarder looks for a stop while no connection arrives.
const ACCEPT_POLL: Duration = Duration::from_millis(20);
const COPY_CHUNK: usize = 64 * 1024;

/// Where one host port's connections go.
#[derive(Debug, Clone)]
pub struct Target {
    /// A Unix stream socket on the host.
    pub socket: PathBuf,
    /// The user the socket's listener must run as.
    pub uid: u32,
}

/// One host port's connections, shared by the forwarder's threads.
#[derive(Debug)]
struct Port {
    target: Target,
    state: Mutex<State>,
}

#[derive(Debug, Default)]
struct State {
    closed: bool,
    next_id: u64,
    /// Both ends of each open connection, kept to close them all at once.
    open: HashMap<u64, (UnixStream, UnixStream)>,
}

impl Port {
    fn state(&self) -> std::sync::MutexGuard<'_, State> {
        self.state.lock().expect("host port lock poisoned")
    }

    /// Closes every connection and refuses new ones.
    fn end(&self) {
        let mut state = self.state();
        state.closed = true;
        for (a, b) in state.open.values() {
            let _ = a.shutdown(Shutdown::Both);
            let _ = b.shutdown(Shutdown::Both);
        }
    }

    /// Accepts the guest's connections on `listener` until `stop` is set.
    fn accept(self: &Arc<Self>, listener: &UnixListener, stop: &AtomicBool) {
        if listener.set_nonblocking(true).is_err() {
            return;
        }
        while !stop.load(Ordering::Relaxed) {
            match listener.accept() {
                Ok((stream, _)) => self.join(stream),
                Err(e) if e.kind() == ErrorKind::WouldBlock => thread::sleep(ACCEPT_POLL),
                Err(e) if e.kind() == ErrorKind::Interrupted => {}
                Err(_) => return,
            }
        }
    }

    /// Joins one guest connection to a new one to the target, or refuses it by closing it: past
    /// the bound, once the port has ended, or when the target cannot be reached at once or is
    /// not the client user's.
    fn join(self: &Arc<Self>, from_guest: UnixStream) {
        let mut state = self.state();
        if state.closed || state.open.len() >= MAX_CONNECTIONS {
            return;
        }
        let Ok(to_host) = connect(&self.target) else {
            return;
        };
        let (Ok(a), Ok(b)) = (from_guest.try_clone(), to_host.try_clone()) else {
            return;
        };
        if from_guest.set_nonblocking(false).is_err() {
            return;
        }
        let id = state.next_id;
        state.next_id += 1;
        state.open.insert(id, (a, b));
        drop(state);
        let port = Arc::clone(self);
        thread::spawn(move || port.carry(id, from_guest, to_host));
    }

    /// Copies both ways until both directions have ended.
    fn carry(&self, id: u64, from_guest: UnixStream, to_host: UnixStream) {
        if let (Ok(guest_in), Ok(host_out)) = (from_guest.try_clone(), to_host.try_clone()) {
            thread::scope(|scope| {
                scope.spawn(|| copy(guest_in, host_out));
                copy(to_host, from_guest);
            });
        }
        self.state().open.remove(&id);
    }
}

/// Connects to the target without waiting, so a listener whose backlog is full refuses the
/// connection rather than holding up the others, and checks who listens.
fn connect(target: &Target) -> io::Result<UnixStream> {
    let socket = rustix::net::socket_with(
        AddressFamily::UNIX,
        SocketType::STREAM,
        SocketFlags::CLOEXEC | SocketFlags::NONBLOCK,
        None,
    )?;
    rustix::net::connect(&socket, &SocketAddrUnix::new(&target.socket)?)?;
    let peer = rustix::net::sockopt::socket_peercred(&socket)?;
    if peer.uid.as_raw() != target.uid {
        return Err(io::Error::new(
            ErrorKind::PermissionDenied,
            "the listener is not the client user's",
        ));
    }
    let stream = UnixStream::from(socket);
    stream.set_nonblocking(false)?;
    Ok(stream)
}

/// Copies one direction, then passes on the end of stream.
fn copy(mut from: UnixStream, mut to: UnixStream) {
    let mut buf = vec![0; COPY_CHUNK];
    loop {
        match from.read(&mut buf) {
            Ok(0) | Err(_) => break,
            Ok(n) => {
                if to.write_all(&buf[..n]).is_err() {
                    break;
                }
            }
        }
    }
    let _ = to.shutdown(Shutdown::Write);
}

/// The host side of one host port: its accepting thread, stopped and every connection closed
/// when dropped.
#[derive(Debug)]
pub struct Forwarder {
    port: Arc<Port>,
    stop: Arc<AtomicBool>,
    thread: Option<thread::JoinHandle<()>>,
}

impl Forwarder {
    /// Starts carrying connections that arrive on `listener` to `target`.
    ///
    /// # Errors
    ///
    /// Fails if the thread cannot be started.
    pub fn start(target: Target, listener: UnixListener) -> io::Result<Forwarder> {
        let port = Arc::new(Port {
            target,
            state: Mutex::new(State::default()),
        });
        let stop = Arc::new(AtomicBool::new(false));
        let thread = {
            let port = Arc::clone(&port);
            let stop = Arc::clone(&stop);
            thread::Builder::new()
                .name("host-port".into())
                .spawn(move || port.accept(&listener, &stop))?
        };
        Ok(Forwarder {
            port,
            stop,
            thread: Some(thread),
        })
    }
}

impl Drop for Forwarder {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        self.port.end();
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}
