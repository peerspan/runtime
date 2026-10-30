// SPDX-License-Identifier: Apache-2.0
// Copyright The Peerspan Authors

//! The guest's network interfaces, set up with the kernel's socket ioctls so init needs no
//! tools from the root filesystem: loopback for every run, and `eth0` with the fixed addresses
//! from the protocol crate when the run has a network.

use std::fs;
use std::io;
use std::mem;
use std::net::Ipv6Addr;
use std::os::fd::{AsRawFd, OwnedFd};

use peerspan_runtime_protocol::{
    GUEST_ADDRESS, GUEST_ADDRESS6, GUEST_DNS, GUEST_GATEWAY, GUEST_GATEWAY6, GUEST_PREFIX,
    GUEST_PREFIX6,
};

/// Written in the root's writable layer, replacing the image's, which may be a link elsewhere.
const RESOLV_CONF: &str = "/etc/resolv.conf";
/// Off before the address is added, so it is usable at once rather than after duplicate
/// address detection, which nothing else on the guest's link could fail.
const ACCEPT_DAD: &str = "/proc/sys/net/ipv6/conf/eth0/accept_dad";

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

/// `struct in6_ifreq` from `<linux/ipv6.h>`, for setting an interface's IPv6 address.
#[repr(C)]
struct In6Ifreq {
    addr: libc::in6_addr,
    prefix_len: u32,
    ifindex: libc::c_int,
}

/// `struct in6_rtmsg` from `<linux/ipv6_route.h>`, for adding an IPv6 route.
#[repr(C)]
struct In6Rtmsg {
    dst: libc::in6_addr,
    src: libc::in6_addr,
    gateway: libc::in6_addr,
    kind: u32,
    dst_len: u16,
    src_len: u16,
    metric: u32,
    info: libc::c_ulong,
    flags: u32,
    ifindex: libc::c_int,
}

/// Brings up loopback, which the kernel leaves down.
pub fn loopback_up() -> io::Result<()> {
    let socket = control_socket()?;
    set_up(&socket, "lo")
}

/// Configures `eth0`, the default routes through the host, and the DNS server.
pub fn configure() -> io::Result<()> {
    fs::write(ACCEPT_DAD, "0")?;
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

    configure_ipv6()?;

    let [a, b, c, d] = GUEST_DNS;
    match fs::remove_file(RESOLV_CONF) {
        Ok(()) => {}
        Err(e) if e.kind() == io::ErrorKind::NotFound => {}
        Err(e) => return Err(e),
    }
    fs::write(RESOLV_CONF, format!("nameserver {a}.{b}.{c}.{d}\n"))
}

/// Adds `eth0`'s IPv6 address and the IPv6 default route, through the host's link-local
/// address on `eth0`.
fn configure_ipv6() -> io::Result<()> {
    let socket = socket(rustix::net::AddressFamily::INET6)?;
    let index = interface_ioctl(&socket, "eth0", libc::SIOCGIFINDEX, |_| {})?;
    // SAFETY: SIOCGIFINDEX filled in the index member.
    let ifindex = unsafe { index.ifr_ifru.ifru_ifindex };

    let mut address = In6Ifreq {
        addr: inet6(GUEST_ADDRESS6),
        prefix_len: u32::from(GUEST_PREFIX6),
        ifindex,
    };
    // SAFETY: SIOCSIFADDR on an IPv6 socket reads one in6_ifreq, which `address` is.
    if unsafe {
        libc::ioctl(
            socket.as_raw_fd(),
            request(libc::SIOCSIFADDR),
            &raw mut address,
        )
    } != 0
    {
        return Err(context("adding eth0's IPv6 address"));
    }

    let mut route = In6Rtmsg {
        dst: inet6([0; 8]),
        src: inet6([0; 8]),
        gateway: inet6(GUEST_GATEWAY6),
        kind: 0,
        dst_len: 0,
        src_len: 0,
        metric: 1,
        info: 0,
        flags: u32::from(libc::RTF_UP | libc::RTF_GATEWAY),
        ifindex,
    };
    // SAFETY: SIOCADDRT on an IPv6 socket reads one in6_rtmsg, which `route` is.
    if unsafe { libc::ioctl(socket.as_raw_fd(), request(libc::SIOCADDRT), &raw mut route) } != 0 {
        return Err(context("adding the IPv6 default route"));
    }
    Ok(())
}

fn control_socket() -> io::Result<OwnedFd> {
    socket(rustix::net::AddressFamily::INET)
}

fn socket(family: rustix::net::AddressFamily) -> io::Result<OwnedFd> {
    rustix::net::socket_with(
        family,
        rustix::net::SocketType::DGRAM,
        rustix::net::SocketFlags::CLOEXEC,
        None,
    )
    .map_err(|e| {
        let e = io::Error::from(e);
        io::Error::new(e.kind(), format!("opening a control socket: {e}"))
    })
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

fn inet6(address: [u16; 8]) -> libc::in6_addr {
    libc::in6_addr {
        s6_addr: Ipv6Addr::from(address).octets(),
    }
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
