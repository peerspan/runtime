// SPDX-License-Identifier: Apache-2.0
// Copyright The Peerspan Authors

//! A link between two runs: the host's half of joining a `connect` guest's loopback port to a
//! `listen` guest's.
//!
//! Each guest reaches the host only through vsock ports mapped to Unix sockets in its run's
//! sockets directory. The `connect` guest's link connections arrive at a socket the supervisor
//! listens on; the `listen` guest's VMM listens on a socket of its own and carries each
//! connection made to it into the guest. The relay here accepts the first kind and connects to
//! the second, one pair per connection, and copies bytes both ways under the link's bounds:
//! connections open at once, and bytes each way over the link's life. Reaching the byte bound,
//! or either run's end, closes every connection and refuses new ones.
//!
//! The `listen` guest's init sends [`LINK_ACCEPTED`] on a stream once the command has accepted
//! the connection it opened for it. Until then nothing is carried or counted, so bytes a client
//! sends on a connection the `listen` guest refuses never count against the byte bound.

use std::collections::HashMap;
use std::io::{self, ErrorKind, Read, Write};
use std::net::Shutdown;
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::Duration;

use peerspan_runtime_protocol::LINK_ACCEPTED;

/// How often the relay looks for a stop while no connection arrives.
const ACCEPT_POLL: Duration = Duration::from_millis(20);
const COPY_CHUNK: usize = 64 * 1024;

/// One link, shared by its two runs.
#[derive(Debug)]
pub struct Link {
    max_connections: usize,
    max_bytes: u64,
    /// Bytes carried so far, from the `connect` side and to it.
    sent: [AtomicU64; 2],
    state: Mutex<State>,
}

#[derive(Debug, Default)]
struct State {
    /// The `listen` run's VMM socket, while that run's VM is up.
    listen_socket: Option<PathBuf>,
    closed: bool,
    next_id: u64,
    /// Both ends of each open connection, kept to close them all at once.
    open: HashMap<u64, (UnixStream, UnixStream)>,
}

impl Link {
    #[must_use]
    pub fn new(max_connections: usize, max_bytes: u64) -> Arc<Link> {
        Arc::new(Link {
            max_connections,
            max_bytes,
            sent: [AtomicU64::new(0), AtomicU64::new(0)],
            state: Mutex::new(State::default()),
        })
    }

    fn state(&self) -> std::sync::MutexGuard<'_, State> {
        self.state.lock().expect("link lock poisoned")
    }

    /// Called by the `listen` run once its VMM is up: connections go to `socket` from now on.
    pub fn listening(&self, socket: PathBuf) {
        let mut state = self.state();
        if !state.closed {
            state.listen_socket = Some(socket);
        }
    }

    /// Ends the link: closes every connection and refuses new ones. Called when either run
    /// ends, and when a direction reaches the byte bound.
    pub fn end(&self) {
        let mut state = self.state();
        state.closed = true;
        state.listen_socket = None;
        for (a, b) in state.open.values() {
            let _ = a.shutdown(Shutdown::Both);
            let _ = b.shutdown(Shutdown::Both);
        }
    }

    /// Accepts the `connect` guest's connections on `listener` until `stop` is set, joining each
    /// to the `listen` guest. Run on its own thread by the `connect` run's supervisor.
    pub fn relay(self: &Arc<Self>, listener: &UnixListener, stop: &AtomicBool) {
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

    /// Joins one connection from the `connect` guest to a new one to the `listen` guest, or
    /// refuses it by closing it.
    fn join(self: &Arc<Self>, from_connect: UnixStream) {
        let mut state = self.state();
        if state.closed || state.open.len() >= self.max_connections {
            return;
        }
        let Some(socket) = state.listen_socket.clone() else {
            return;
        };
        let Ok(to_listen) = UnixStream::connect(&socket) else {
            return;
        };
        let (Ok(a), Ok(b)) = (from_connect.try_clone(), to_listen.try_clone()) else {
            return;
        };
        if from_connect.set_nonblocking(false).is_err() {
            return;
        }
        let id = state.next_id;
        state.next_id += 1;
        state.open.insert(id, (a, b));
        drop(state);
        let link = Arc::clone(self);
        thread::spawn(move || link.carry(id, from_connect, to_listen));
    }

    /// Waits for the `listen` guest to accept the connection, then copies both ways until both
    /// directions have ended.
    fn carry(&self, id: u64, from_connect: UnixStream, mut to_listen: UnixStream) {
        let mut accepted = [0];
        let ok = matches!(to_listen.read(&mut accepted), Ok(1)) && accepted[0] == LINK_ACCEPTED;
        let (true, Ok(connect_in), Ok(listen_out)) =
            (ok, from_connect.try_clone(), to_listen.try_clone())
        else {
            self.state().open.remove(&id);
            return;
        };
        thread::scope(|scope| {
            scope.spawn(|| self.copy(0, connect_in, listen_out));
            self.copy(1, to_listen, from_connect);
        });
        self.state().open.remove(&id);
    }

    /// Copies one direction under its byte bound, then passes on the end of stream.
    fn copy(&self, direction: usize, mut from: UnixStream, mut to: UnixStream) {
        let mut buf = vec![0; COPY_CHUNK];
        loop {
            let n = match from.read(&mut buf) {
                Ok(0) | Err(_) => break,
                Ok(n) => n,
            };
            let before = self.sent[direction].fetch_add(n as u64, Ordering::Relaxed);
            let allowed = usize::try_from(self.max_bytes.saturating_sub(before)).unwrap_or(n);
            let write = n.min(allowed);
            let written = write == 0 || to.write_all(&buf[..write]).is_ok();
            // The bound ends the link whether or not this last write got through.
            if before + n as u64 >= self.max_bytes {
                self.end();
                return;
            }
            if !written {
                break;
            }
        }
        let _ = to.shutdown(Shutdown::Write);
    }
}

/// The host side of a `connect` run's link: the relay's thread, stopped and the link ended when
/// dropped.
#[derive(Debug)]
pub struct Relay {
    link: Arc<Link>,
    stop: Arc<AtomicBool>,
    thread: Option<thread::JoinHandle<()>>,
}

impl Relay {
    /// Starts relaying connections that arrive on `listener`.
    ///
    /// # Errors
    ///
    /// Fails if the thread cannot be started.
    pub fn start(link: Arc<Link>, listener: UnixListener) -> io::Result<Relay> {
        let stop = Arc::new(AtomicBool::new(false));
        let thread = {
            let link = Arc::clone(&link);
            let stop = Arc::clone(&stop);
            thread::Builder::new()
                .name("link-relay".into())
                .spawn(move || link.relay(&listener, &stop))?
        };
        Ok(Relay {
            link,
            stop,
            thread: Some(thread),
        })
    }
}

impl Drop for Relay {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        self.link.end();
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

/// Ends the link when dropped; held by a `listen` run's supervisor.
#[derive(Debug)]
pub struct EndOnDrop(pub Arc<Link>);

impl Drop for EndOnDrop {
    fn drop(&mut self) {
        self.0.end();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::{Read, Write};

    /// A listen side that accepts and echoes, behind a real socket, as the listen VMM's socket
    /// would be.
    fn echo_server(dir: &std::path::Path) -> PathBuf {
        let path = dir.join("listen.sock");
        let listener = UnixListener::bind(&path).expect("bind listen socket");
        thread::spawn(move || {
            for stream in listener.incoming().flatten() {
                thread::spawn(move || {
                    let mut reader = stream.try_clone().expect("clone");
                    let mut writer = stream;
                    writer.write_all(&[LINK_ACCEPTED]).expect("accept");
                    let _ = io::copy(&mut reader, &mut writer);
                });
            }
        });
        path
    }

    fn connect_side(dir: &std::path::Path, link: &Arc<Link>) -> (PathBuf, Relay) {
        let path = dir.join("connect.sock");
        let listener = UnixListener::bind(&path).expect("bind connect socket");
        (
            path,
            Relay::start(Arc::clone(link), listener).expect("relay"),
        )
    }

    fn temp_dir() -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "link-test-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .expect("time")
                .as_nanos()
        ));
        std::fs::create_dir_all(&dir).expect("temp dir");
        dir
    }

    #[test]
    fn carries_bytes_and_refuses_before_the_listener_and_past_the_bounds() {
        let dir = temp_dir();
        let link = Link::new(1, 10);
        let (connect, _relay) = connect_side(&dir, &link);

        // No listen side yet: the connection is closed at once.
        let mut early = UnixStream::connect(&connect).expect("connect");
        let mut buf = [0; 8];
        assert_eq!(early.read(&mut buf).expect("read"), 0);

        link.listening(echo_server(&dir));
        let mut first = UnixStream::connect(&connect).expect("connect");
        first.write_all(b"hello").expect("write");
        let mut got = [0; 5];
        first.read_exact(&mut got).expect("echo");
        assert_eq!(&got, b"hello");

        // One connection at once: a second is refused while the first is open.
        let mut second = UnixStream::connect(&connect).expect("connect");
        assert_eq!(second.read(&mut buf).expect("read"), 0);

        // Ten bytes each way: the next six reach the bound, which ends the link. At most the
        // five that fit are carried there, and fewer of them may come back before it closes.
        first.write_all(b"abcdef").expect("write");
        let mut rest = Vec::new();
        let _ = first.read_to_end(&mut rest);
        assert!(b"abcde".starts_with(&rest), "got {rest:?}");
        let mut after = UnixStream::connect(&connect).expect("connect");
        assert_eq!(after.read(&mut buf).expect("read"), 0);
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// A connection the listen guest refuses, closing it without the accepted byte, carries
    /// nothing and counts nothing against the byte bound.
    #[test]
    fn a_refused_connection_counts_no_bytes() {
        let dir = temp_dir();
        let link = Link::new(1, 10);
        let (connect, _relay) = connect_side(&dir, &link);
        let refusing = dir.join("refusing.sock");
        let listener = UnixListener::bind(&refusing).expect("bind listen socket");
        link.listening(refusing);
        let mut client = UnixStream::connect(&connect).expect("connect");
        client.write_all(b"0123456789abc").expect("write");
        // The listen side reads what was sent and closes without accepting.
        let (mut refused, _) = listener.accept().expect("accept");
        let mut buf = [0; 16];
        refused
            .set_read_timeout(Some(Duration::from_millis(200)))
            .expect("timeout");
        assert!(
            refused.read(&mut buf).is_err(),
            "nothing is carried before acceptance"
        );
        drop(refused);
        // Closed, at once: with what it sent unread, the client may see a reset.
        assert!(matches!(client.read(&mut buf), Ok(0) | Err(_)));
        assert_eq!(link.sent[0].load(Ordering::Relaxed), 0);
        assert!(!link.state().closed);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn the_bound_ends_the_link_even_when_its_last_write_fails() {
        let link = Link::new(1, 1);
        let (from, mut feed) = UnixStream::pair().expect("pair");
        let (to, closed_peer) = UnixStream::pair().expect("pair");
        drop(closed_peer);
        feed.write_all(b"x").expect("write");
        drop(feed);
        link.copy(0, from, to);
        assert!(link.state().closed);
    }
}
