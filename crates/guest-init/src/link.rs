// SPDX-License-Identifier: Apache-2.0
// Copyright The Peerspan Authors

//! A linked run's side of its link: a TCP port on loopback, carried over vsock to the host,
//! which joins it to the other run's.
//!
//! - `connect`: init listens on `127.0.0.1:port` and, for each connection the command opens,
//!   opens one vsock stream to the host's [`LINK_PORT`] and copies bytes both ways.
//! - `listen`: init listens on vsock [`LINK_PORT`] and, for each stream the host opens, opens
//!   one TCP connection to `127.0.0.1:port`, tells the host with [`LINK_ACCEPTED`], and copies
//!   bytes both ways. A connection the command is not listening for is closed at once.
//!
//! Bounds on connections and bytes are the host's; init only carries what arrives.

use std::fs::File;
use std::io::{self, Read, Write};
use std::net::{Shutdown, TcpListener, TcpStream};
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};
use std::thread;

use peerspan_runtime_protocol::{LINK_ACCEPTED, LINK_PORT, Link, LinkRole};
use rustix::net::{AddressFamily, SocketFlags, SocketType};

/// Starts carrying the link, before the command starts, so the port is ready for it.
///
/// # Errors
///
/// Fails if the loopback port or the vsock port cannot be bound.
pub fn start(link: Link) -> io::Result<()> {
    match link.role {
        LinkRole::Connect => {
            let listener = TcpListener::bind(("127.0.0.1", link.port))?;
            thread::spawn(move || {
                for tcp in listener.incoming().flatten() {
                    thread::spawn(move || {
                        if let Ok(vsock) = crate::connect_vsock(libc::VMADDR_CID_HOST, LINK_PORT) {
                            splice(tcp, vsock);
                        }
                    });
                }
            });
        }
        LinkRole::Listen => {
            let listener = vsock_listener(LINK_PORT)?;
            let port = link.port;
            thread::spawn(move || {
                while let Ok(mut vsock) = accept(&listener) {
                    thread::spawn(move || {
                        // Not listening yet, or no longer: the host sees the stream close
                        // without the accepted byte.
                        if let Ok(tcp) = TcpStream::connect(("127.0.0.1", port))
                            && vsock.write_all(&[LINK_ACCEPTED]).is_ok()
                        {
                            splice(tcp, vsock);
                        }
                    });
                }
            });
        }
    }
    Ok(())
}

/// Copies bytes both ways until each side has closed, passing on each end of stream.
fn splice(tcp: TcpStream, vsock: File) {
    let _ = tcp.set_nodelay(true);
    let (Ok(tcp_in), Ok(vsock_out)) = (tcp.try_clone(), vsock.try_clone()) else {
        return;
    };
    let outbound = thread::spawn(move || {
        copy(tcp_in, &vsock_out);
        let _ = rustix::net::shutdown(&vsock_out, rustix::net::Shutdown::Write);
    });
    copy(&vsock, &tcp);
    let _ = tcp.shutdown(Shutdown::Write);
    let _ = outbound.join();
}

fn copy(mut from: impl Read, mut to: impl Write) {
    let mut buf = vec![0; 64 * 1024];
    loop {
        match from.read(&mut buf) {
            Ok(0) | Err(_) => return,
            Ok(n) => {
                if to.write_all(&buf[..n]).is_err() {
                    return;
                }
            }
        }
    }
}

fn vsock_listener(port: u32) -> io::Result<OwnedFd> {
    let fd = rustix::net::socket_with(
        AddressFamily::VSOCK,
        SocketType::STREAM,
        SocketFlags::CLOEXEC,
        None,
    )?;
    // SAFETY: sockaddr_vm is plain data, and all-zero is a valid starting value.
    let mut addr: libc::sockaddr_vm = unsafe { std::mem::zeroed() };
    addr.svm_family = libc::sa_family_t::try_from(libc::AF_VSOCK).expect("AF_VSOCK fits");
    addr.svm_cid = libc::VMADDR_CID_ANY;
    addr.svm_port = port;
    let len = libc::socklen_t::try_from(size_of::<libc::sockaddr_vm>()).expect("small struct");
    // SAFETY: addr is a valid sockaddr_vm and len is its exact size.
    let rc = unsafe { libc::bind(fd.as_raw_fd(), std::ptr::from_ref(&addr).cast(), len) };
    if rc != 0 {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: fd is a bound socket this function owns.
    if unsafe { libc::listen(fd.as_raw_fd(), 64) } != 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(fd)
}

fn accept(listener: &OwnedFd) -> io::Result<File> {
    loop {
        // SAFETY: a null address asks for none; the descriptor is a listening socket.
        let fd = unsafe {
            libc::accept4(
                listener.as_raw_fd(),
                std::ptr::null_mut(),
                std::ptr::null_mut(),
                libc::SOCK_CLOEXEC,
            )
        };
        if fd >= 0 {
            // SAFETY: accept4 returned a new descriptor that nothing else owns.
            return Ok(unsafe { File::from_raw_fd(fd) });
        }
        let e = io::Error::last_os_error();
        if e.kind() != io::ErrorKind::Interrupted {
            return Err(e);
        }
    }
}
