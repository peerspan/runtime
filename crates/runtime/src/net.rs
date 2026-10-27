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

use std::env;
use std::fmt;
use std::fs;
use std::io::Write;
use std::net::IpAddr;
use std::path::{Path, PathBuf};
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
/// metadata service at 169.254.169.254), multicast, reserved and broadcast, and IPv4-mapped
/// IPv6, which would reach the same IPv4 addresses.
const ALWAYS_REJECTED_V4: &str =
    "{ 0.0.0.0/8, 127.0.0.0/8, 169.254.0.0/16, 224.0.0.0/4, 240.0.0.0/4 }";
const ALWAYS_REJECTED_V6: &str = "{ ::/127, fe80::/10, ff00::/8, ::ffff:0:0/96 }";
/// Private ranges, rejected at `internet`: RFC 1918, shared address space (CGNAT) and unique
/// local IPv6.
const PRIVATE_V4: &str = "{ 10.0.0.0/8, 100.64.0.0/10, 172.16.0.0/12, 192.168.0.0/16 }";
const PRIVATE_V6: &str = "{ fc00::/7 }";

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
pub struct RunFilter {
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
        let name = relative
            .file_name()
            .and_then(|n| n.to_str())
            .context("cgroup name is not UTF-8")?;
        let filter = RunFilter {
            chain: name.to_owned(),
            cgroup: path_str(relative)?.to_owned(),
        };
        let script = format!(
            "{base}{chain}add element {TABLE} runs {{ \"{cgroup}\" : jump {name} }}\n",
            base = base_rules(path_str(root)?, depth),
            chain = run_chain(name, level, &resolvers()),
            cgroup = filter.cgroup,
        );
        nft(&script).context("installing the run's network filter")?;
        Ok(filter)
    }
}

impl Drop for RunFilter {
    fn drop(&mut self) {
        let script = format!(
            "delete element {TABLE} runs {{ \"{}\" }}\ndelete chain {TABLE} {}\n",
            self.cgroup, self.chain
        );
        if let Err(e) = nft(&script) {
            eprintln!("peerspan-runtime: removing the run's network filter: {e:#}");
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

/// The passt the runtime ships with, beside its own binary: a pinned release, built by
/// `scripts/build-passt.sh`, whose arguments [`passt_args`] is written against.
pub fn bundled_passt() -> PathBuf {
    env::current_exe()
        .ok()
        .and_then(|exe| exe.parent().map(|dir| dir.join("passt")))
        .unwrap_or_else(|| PathBuf::from("passt"))
}

/// The arguments passt runs with for one guest, after `--fd` and its descriptor: IPv4 only
/// for now, the fixed guest addresses with no DHCP, DNS queries to the guest's DNS address
/// forwarded to the host's resolvers, no inbound ports, and no mapping of the gateway address
/// to the host's loopback.
pub fn passt_args() -> Vec<String> {
    use peerspan_runtime_protocol::{GUEST_ADDRESS, GUEST_DNS, GUEST_GATEWAY, GUEST_PREFIX};
    let dotted = |a: [u8; 4]| std::net::Ipv4Addr::from(a).to_string();
    [
        "--foreground",
        "--quiet",
        "--ipv4-only",
        "--address",
        &dotted(GUEST_ADDRESS),
        "--netmask",
        &GUEST_PREFIX.to_string(),
        "--gateway",
        &dotted(GUEST_GATEWAY),
        "--dns-forward",
        &dotted(GUEST_DNS),
        "--no-map-gw",
        "--no-dhcp",
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
        assert!(!lan.contains(PRIVATE_V4));
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
