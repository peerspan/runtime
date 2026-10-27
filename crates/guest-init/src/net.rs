// SPDX-License-Identifier: Apache-2.0
// Copyright The Peerspan Authors

//! The guest's network interfaces, set up with the kernel's socket ioctls so init needs no
//! tools from the root filesystem: loopback for every run, and `eth0` with the fixed addresses
//! from the protocol crate when the run has a network.

use std::fs;
use std::io;
use std::mem;
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};

use peerspan_runtime_protocol::{GUEST_ADDRESS, GUEST_DNS, GUEST_GATEWAY, GUEST_PREFIX};

/// The read-only root filesystem keeps an empty file here for init to bind over.
const RESOLV_CONF: &str = "/etc/resolv.conf";
/// Written on the guest's `/run` tmpfs, then bound over [`RESOLV_CONF`].
const RESOLV_CONF_SOURCE: &str = "/run/resolv.conf";

/// `struct rtentry` from `<net/route.h>`, which the libc crate defines only for some targets.
#[repr(C)]
struct RtEntry {
    pad1: libc::c_ulong,
    dst: libc::sockaddr,
    gateway: libc::sockaddr,
    genmask: libc::sockaddr,
    flags: libc::c_ushort,
    pad2: libc::c_short,
    pad3: libc::c_ulong,
    tos: libc::c_uchar,
    class: libc::c_uchar,
    pad4: [libc::c_short; 3],
    metric: libc::c_short,
    dev: *mut libc::c_char,
    mtu: libc::c_ulong,
    window: libc::c_ulong,
    irtt: libc::c_ushort,
}

/// Brings up loopback, which the kernel leaves down.
pub fn loopback_up() -> io::Result<()> {
    let socket = control_socket()?;
    set_up(&socket, "lo")
}

/// Configures `eth0`, the default route through the host, and the DNS server.
pub fn configure() -> io::Result<()> {
    let socket = control_socket()?;
    let netmask = u32::MAX
        .checked_shl(32 - u32::from(GUEST_PREFIX))
        .unwrap_or(0)
        .to_be_bytes();
    interface_ioctl(&socket, "eth0", libc::SIOCSIFADDR, |ifr| {
        ifr.ifr_ifru.ifru_addr = inet(GUEST_ADDRESS);
    })?;
    interface_ioctl(&socket, "eth0", libc::SIOCSIFNETMASK, |ifr| {
        ifr.ifr_ifru.ifru_netmask = inet(netmask);
    })?;
    set_up(&socket, "eth0")?;

    // SAFETY: all-zero bytes are a valid RtEntry: null device pointer, zero addresses.
    let mut route: RtEntry = unsafe { mem::zeroed() };
    route.dst = inet([0; 4]);
    route.genmask = inet([0; 4]);
    route.gateway = inet(GUEST_GATEWAY);
    route.flags = libc::RTF_UP | libc::RTF_GATEWAY;
    // SAFETY: SIOCADDRT reads one rtentry, which `route` is, for the call's duration.
    if unsafe { libc::ioctl(socket.as_raw_fd(), request(libc::SIOCADDRT), &raw mut route) } != 0 {
        return Err(context("adding the default route"));
    }

    let [a, b, c, d] = GUEST_DNS;
    fs::write(RESOLV_CONF_SOURCE, format!("nameserver {a}.{b}.{c}.{d}\n"))?;
    crate::mount(RESOLV_CONF_SOURCE, RESOLV_CONF, "", libc::MS_BIND)
}

fn control_socket() -> io::Result<OwnedFd> {
    // SAFETY: socket has no memory preconditions.
    let fd = unsafe { libc::socket(libc::AF_INET, libc::SOCK_DGRAM | libc::SOCK_CLOEXEC, 0) };
    if fd < 0 {
        return Err(context("opening a control socket"));
    }
    // SAFETY: fd is a new descriptor that nothing else owns.
    Ok(unsafe { OwnedFd::from_raw_fd(fd) })
}

fn set_up(socket: &OwnedFd, name: &str) -> io::Result<()> {
    let current = interface_ioctl(socket, name, libc::SIOCGIFFLAGS, |_| {})?;
    // SAFETY: SIOCGIFFLAGS filled in the flags member.
    let flags = unsafe { current.ifr_ifru.ifru_flags };
    let up = flags | libc::IFF_UP as libc::c_short | libc::IFF_RUNNING as libc::c_short;
    interface_ioctl(socket, name, libc::SIOCSIFFLAGS, |ifr| {
        ifr.ifr_ifru.ifru_flags = up;
    })
    .map(drop)
}

/// Issues one interface ioctl on `name` with an `ifreq` that `fill` prepares, and returns it.
fn interface_ioctl(
    socket: &OwnedFd,
    name: &str,
    op: libc::c_ulong,
    fill: impl FnOnce(&mut libc::ifreq),
) -> io::Result<libc::ifreq> {
    // SAFETY: all-zero bytes are a valid ifreq.
    let mut ifr: libc::ifreq = unsafe { mem::zeroed() };
    for (slot, byte) in ifr.ifr_name.iter_mut().zip(name.bytes()) {
        *slot = byte as libc::c_char;
    }
    fill(&mut ifr);
    // SAFETY: interface ioctls read and write one ifreq, which `ifr` is, for the call's duration.
    if unsafe { libc::ioctl(socket.as_raw_fd(), request(op), &raw mut ifr) } != 0 {
        return Err(context(&format!("configuring {name}")));
    }
    Ok(ifr)
}

/// An IPv4 address as the generic `sockaddr` the ioctls take.
fn inet(address: [u8; 4]) -> libc::sockaddr {
    let inet = libc::sockaddr_in {
        sin_family: libc::AF_INET as libc::sa_family_t,
        sin_port: 0,
        sin_addr: libc::in_addr {
            s_addr: u32::from_ne_bytes(address),
        },
        sin_zero: [0; 8],
    };
    // SAFETY: sockaddr_in and sockaddr are both 16 bytes, and every bit pattern is a valid
    // sockaddr.
    unsafe { mem::transmute::<libc::sockaddr_in, libc::sockaddr>(inet) }
}

/// The ioctl request type is `c_ulong` on glibc and `c_int` on musl.
#[allow(clippy::cast_possible_truncation, clippy::useless_conversion)]
fn request(op: libc::c_ulong) -> libc::Ioctl {
    op as libc::Ioctl
}

fn context(what: &str) -> io::Error {
    let e = io::Error::last_os_error();
    io::Error::new(e.kind(), format!("{what}: {e}"))
}
