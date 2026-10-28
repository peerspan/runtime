// SPDX-License-Identifier: Apache-2.0
// Copyright The Peerspan Authors

//! Guest networking: what a run's guest may reach, and the host side that carries and limits
//! its traffic (decision 5 in docs-internal `docs/runtime.md`).
//!
//! A guest with a network gets a virtio-net device whose other end is a passt process of its
//! own. passt turns the guest's packets into ordinary sockets, opened from the host's network
//! namespace, so it could reach anything the host can. passt runs in the run's cgroup, and an
//! nftables table limits where sockets in that cgroup may connect: every run's traffic goes to
//! a chain of its own, and traffic from the runtime's cgroups with no chain is rejected, so the
//! filter fails closed.

use std::fmt;
use std::fs;
use std::io::Write;
use std::net::{IpAddr, Ipv4Addr, Ipv6Addr};
use std::path::Path;
use std::process::{Command, Stdio};

use anyhow::{Context, bail};
use serde::Deserialize;

/// The nftables table the runtime owns, shared by every run on the host.
const TABLE: &str = "inet peerspan_runtime";
/// Where cgroup v2 is mounted; nftables matches cgroups by their path below it.
const CGROUP_MOUNT: &str = "/sys/fs/cgroup";
/// Rejected traffic fails fast in the guest instead of hanging until a timeout.
const REJECT: &str = "reject with icmpx admin-prohibited";
/// Never reachable at any level: unspecified, loopback, link-local (including the cloud
/// metadata service at 169.254.169.254), multicast, reserved and broadcast. In IPv6 also
/// IPv4-mapped addresses and the same IPv4 ranges behind the well-known NAT64 prefix
/// (64:ff9b::/96), which would reach them through the host's IPv4 or a NAT64 gateway.
const ALWAYS_REJECTED_V4: &str =
    "{ 0.0.0.0/8, 127.0.0.0/8, 169.254.0.0/16, 224.0.0.0/4, 240.0.0.0/4 }";
const ALWAYS_REJECTED_V6: &str = "{ ::/127, fe80::/10, ff00::/8, ::ffff:0:0/96, \
    64:ff9b::/104, 64:ff9b::7f00:0/104, 64:ff9b::a9fe:0/112, 64:ff9b::e000:0/100, \
    64:ff9b::f000:0/100 }";
/// Private ranges, rejected at `internet`: RFC 1918 and shared address space (CGNAT), in IPv6
/// unique local addresses, those IPv4 ranges behind the well-known NAT64 prefix, and the
/// local-use NAT64 prefix (64:ff9b:1::/48), which may translate to any of them.
const PRIVATE_V4: &str = "{ 10.0.0.0/8, 100.64.0.0/10, 172.16.0.0/12, 192.168.0.0/16 }";
const PRIVATE_V6: &str = "{ fc00::/7, 64:ff9b:1::/48, 64:ff9b::a00:0/104, \
    64:ff9b::6440:0/106, 64:ff9b::ac10:0/108, 64:ff9b::c0a8:0/112 }";

/// What a run's guest may reach. Ordered from least to most, so a request can be compared
/// with the host owner's maximum.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Deserialize, clap::ValueEnum)]
pub enum Level {
    /// No network device; the guest has only loopback.
    #[serde(rename = "none")]
    #[value(name = "none")]
    None,
    /// Public addresses only.
    #[serde(rename = "internet")]
    #[value(name = "internet")]
    Internet,
    /// Public addresses and private ranges, such as a LAN registry.
    #[serde(rename = "internet+lan")]
    #[value(name = "internet+lan")]
    InternetLan,
}

impl fmt::Display for Level {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Level::None => "none",
            Level::Internet => "internet",
            Level::InternetLan => "internet+lan",
        })
    }
}

/// A run's place in the filter, removed when dropped.
///
/// Drop it before the run's cgroup: nftables names the cgroup by path, which must still exist.
#[derive(Debug)]
pub struct RunFilter(Names);

/// Where a run is in the filter: its chain, and its cgroup's key in the map of runs.
#[derive(Debug)]
struct Names {
    chain: String,
    cgroup: String,
}

impl RunFilter {
    /// Limits sockets in `cgroup`, a run's cgroup, to what `level` allows.
    ///
    /// # Errors
    ///
    /// Fails if `nft` cannot be run or rejects the rules; the run must not start unfiltered.
    pub fn install(cgroup: &Path, level: Level) -> anyhow::Result<RunFilter> {
        let relative = cgroup
            .strip_prefix(CGROUP_MOUNT)
            .with_context(|| format!("{} is not under {CGROUP_MOUNT}", cgroup.display()))?;
        let depth = relative.components().count();
        let Some(root) = relative.parent().filter(|_| depth >= 2) else {
            bail!(
                "a run's cgroup must be nested below {CGROUP_MOUNT}: {}",
                cgroup.display()
            );
        };
        let names = Names::of(cgroup)?;
        let script = format!(
            "{base}{chain}add element {TABLE} runs {{ \"{cgroup}\" : jump {name} }}\n",
            base = base_rules(path_str(root)?, depth),
            chain = run_chain(&names.chain, level, &resolvers()),
            cgroup = names.cgroup,
            name = names.chain,
        );
        nft(&script).context("installing the run's network filter")?;
        Ok(RunFilter(names))
    }
}

impl Names {
    /// The names of the run whose cgroup is `cgroup`.
    fn of(cgroup: &Path) -> anyhow::Result<Names> {
        let relative = cgroup
            .strip_prefix(CGROUP_MOUNT)
            .with_context(|| format!("{} is not under {CGROUP_MOUNT}", cgroup.display()))?;
        let name = relative
            .file_name()
            .and_then(|n| n.to_str())
            .context("cgroup name is not UTF-8")?;
        Ok(Names {
            chain: name.to_owned(),
            cgroup: path_str(relative)?.to_owned(),
        })
    }

    fn delete_element(&self) -> String {
        format!("delete element {TABLE} runs {{ \"{}\" }}\n", self.cgroup)
    }

    fn delete_chain(&self) -> String {
        format!("delete chain {TABLE} {}\n", self.chain)
    }
}

/// Removes what supervisors that died left in the filter for `cgroups`, their runs' cgroups,
/// which must still exist: nftables resolves a cgroup's path when it parses the rules.
///
/// # Errors
///
/// Fails if `nft` cannot be run or rejects the removal.
pub fn remove_stale(cgroups: &[&Path]) -> anyhow::Result<()> {
    if cgroups.is_empty() {
        return Ok(());
    }
    let listing = Command::new("nft")
        .args(["list", "table"])
        .args(TABLE.split(' '))
        .stderr(Stdio::null())
        .output()
        .context("running nft")?;
    // No table, nothing left in it.
    if !listing.status.success() {
        return Ok(());
    }
    let listing = String::from_utf8_lossy(&listing.stdout);
    let mut script = String::new();
    for cgroup in cgroups {
        let names = Names::of(cgroup)?;
        if listing.contains(&format!("\"{}\" : jump", names.cgroup)) {
            script.push_str(&names.delete_element());
        }
        if listing.contains(&format!("chain {} {{", names.chain)) {
            script.push_str(&names.delete_chain());
        }
    }
    if script.is_empty() {
        return Ok(());
    }
    nft(&script).context("removing stale network filters")
}

impl Drop for RunFilter {
    fn drop(&mut self) {
        let script = self.0.delete_element() + &self.0.delete_chain();
        if let Err(e) = nft(&script) {
            crate::log::warn(
                "could not remove a run's network filter",
                &[("error", &format!("{e:#}"))],
            );
        }
    }
}

/// The table, the map from run cgroups to their chains, and the output hook. Rewritten on
/// every install, in the same transaction, so it always matches the current cgroup root.
fn base_rules(root: &str, depth: usize) -> String {
    let parent_depth = depth - 1;
    format!(
        "table {TABLE} {{
  map runs {{ typeof socket cgroupv2 level {depth} : verdict; }}
  chain output {{ type filter hook output priority 0; policy accept; }}
}}
flush chain {TABLE} output
add rule {TABLE} output socket cgroupv2 level {depth} vmap @runs
add rule {TABLE} output socket cgroupv2 level {parent_depth} \"{root}\" {REJECT}
"
    )
}

/// One run's chain: DNS to the host's resolvers, then the host's own addresses and the ranges
/// `level` excludes are rejected, and the rest accepted.
fn run_chain(name: &str, level: Level, resolvers: &[IpAddr]) -> String {
    let mut rules = vec![];
    for resolver in resolvers {
        let family = if resolver.is_ipv4() { "ip" } else { "ip6" };
        rules.push(format!(
            "{family} daddr {resolver} meta l4proto {{ tcp, udp }} th dport 53 accept"
        ));
    }
    rules.push(format!("fib daddr type local {REJECT}"));
    rules.push(format!("ip daddr {ALWAYS_REJECTED_V4} {REJECT}"));
    rules.push(format!("ip6 daddr {ALWAYS_REJECTED_V6} {REJECT}"));
    if level < Level::InternetLan {
        rules.push(format!("ip daddr {PRIVATE_V4} {REJECT}"));
        rules.push(format!("ip6 daddr {PRIVATE_V6} {REJECT}"));
    }
    rules.push("accept".to_owned());
    let mut script = format!("add chain {TABLE} {name}\n");
    for rule in rules {
        script.push_str(&format!("add rule {TABLE} {name} {rule}\n"));
    }
    script
}

/// The host's DNS servers, which passt forwards the guest's queries to. With systemd-resolved
/// this is its stub on 127.0.0.53, a local address the filter would otherwise reject.
fn resolvers() -> Vec<IpAddr> {
    fs::read_to_string("/etc/resolv.conf")
        .unwrap_or_default()
        .lines()
        .filter_map(|line| line.trim().strip_prefix("nameserver"))
        .filter_map(|address| address.trim().parse().ok())
        .collect()
}

fn path_str(path: &Path) -> anyhow::Result<&str> {
    path.to_str()
        .with_context(|| format!("{} is not UTF-8", path.display()))
}

/// Applies `script` as one nftables transaction.
fn nft(script: &str) -> anyhow::Result<()> {
    let mut child = Command::new("nft")
        .args(["-f", "-"])
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .spawn()
        .context("running nft")?;
    child
        .stdin
        .take()
        .context("nft's standard input")?
        .write_all(script.as_bytes())?;
    let output = child.wait_with_output()?;
    if !output.status.success() {
        bail!(
            "nft failed ({}): {}",
            output.status,
            String::from_utf8_lossy(&output.stderr).trim()
        );
    }
    Ok(())
}

/// The arguments passt runs with for one guest, after `--fd` and its descriptor: the fixed
/// guest addresses with no DHCP, DHCPv6 or router advertisements, DNS queries to the guest's
/// DNS address forwarded to the host's resolvers, no inbound ports, and no mapping of the
/// gateway addresses to the host's loopback. passt answers neighbour discovery for the IPv6
/// gateway, and on a host without IPv6 the guest's IPv6 connections fail at once.
pub fn passt_args() -> Vec<String> {
    use peerspan_runtime_protocol::{
        GUEST_ADDRESS, GUEST_ADDRESS6, GUEST_DNS, GUEST_GATEWAY, GUEST_GATEWAY6, GUEST_PREFIX,
    };
    let dotted = |a: [u8; 4]| Ipv4Addr::from(a).to_string();
    [
        "--foreground",
        "--quiet",
        "--address",
        &dotted(GUEST_ADDRESS),
        "--netmask",
        &GUEST_PREFIX.to_string(),
        "--gateway",
        &dotted(GUEST_GATEWAY),
        "--address",
        &Ipv6Addr::from(GUEST_ADDRESS6).to_string(),
        "--gateway",
        &Ipv6Addr::from(GUEST_GATEWAY6).to_string(),
        "--dns-forward",
        &dotted(GUEST_DNS),
        "--no-map-gw",
        "--no-dhcp",
        "--no-dhcpv6",
        "--no-ra",
        "--tcp-ports",
        "none",
        "--udp-ports",
        "none",
    ]
    .map(str::to_owned)
    .to_vec()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn levels_order_from_least_to_most() {
        assert!(Level::None < Level::Internet);
        assert!(Level::Internet < Level::InternetLan);
    }

    #[test]
    fn internet_rejects_private_ranges_and_lan_does_not() {
        let internet = run_chain("run-1", Level::Internet, &[]);
        let lan = run_chain("run-1", Level::InternetLan, &[]);
        assert!(internet.contains(PRIVATE_V4));
        assert!(internet.contains(PRIVATE_V6));
        assert!(!lan.contains(PRIVATE_V4));
        assert!(!lan.contains(PRIVATE_V6));
        for chain in [&internet, &lan] {
            assert!(chain.contains("fib daddr type local"));
            assert!(chain.contains(ALWAYS_REJECTED_V4));
            assert!(chain.trim_end().ends_with("accept"));
        }
    }

    #[test]
    fn dns_to_resolvers_is_accepted_before_local_addresses_are_rejected() {
        let chain = run_chain("run-1", Level::Internet, &["127.0.0.53".parse().unwrap()]);
        let dns = chain.find("127.0.0.53").unwrap();
        assert!(dns < chain.find("fib daddr type local").unwrap());
    }

    #[test]
    fn base_rules_reject_the_runtime_root_one_level_up() {
        let rules = base_rules("peerspan-runtime", 2);
        assert!(rules.contains("level 2 vmap @runs"));
        assert!(rules.contains("level 1 \"peerspan-runtime\" reject"));
    }
}
