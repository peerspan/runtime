// SPDX-License-Identifier: Apache-2.0
// Copyright The Peerspan Authors

//! `doctor`: checks that this host can run VMs, and says what to fix when it cannot.

use std::fs::{self, OpenOptions};
use std::path::{Path, PathBuf};
use std::process::Command;

use clap::Args;

use crate::{bundle, cgroup, krun};

#[derive(Debug, Args)]
pub struct DoctorArgs {
    /// Guest image directory [default: `guest/` beside this binary].
    #[arg(long, env = "PEERSPAN_RUNTIME_GUEST")]
    guest: Option<PathBuf>,
    /// libkrun shared library [default: beside this binary, or the dynamic loader's].
    #[arg(long, env = "PEERSPAN_LIBKRUN")]
    libkrun: Option<PathBuf>,
    /// virtiofsd binary.
    #[arg(
        long,
        env = "PEERSPAN_VIRTIOFSD",
        default_value = "/usr/libexec/virtiofsd"
    )]
    virtiofsd: PathBuf,
    /// passt binary [default: beside this binary].
    #[arg(long, env = "PEERSPAN_PASST")]
    passt: Option<PathBuf>,
}

/// What one check found: what it saw, or what is wrong and how to fix it.
type Found = Result<String, String>;

/// Runs every check, prints one line each, and returns whether all passed.
pub fn doctor(args: &DoctorArgs) -> bool {
    let checks: [(&str, Found); 11] = [
        ("platform", platform()),
        ("root", root()),
        ("kvm", kvm()),
        ("cgroup v2", cgroups()),
        ("user namespaces", user_namespaces()),
        ("nftables", nftables()),
        ("disk images", disk_images()),
        ("virtiofsd", virtiofsd(&args.virtiofsd)),
        ("libkrun", libkrun(args.libkrun.clone())),
        ("passt", passt(args.passt.clone())),
        ("guest image", guest(args.guest.clone())),
    ];
    let mut healthy = true;
    for (name, found) in checks {
        match found {
            Ok(seen) => println!("ok    {name}: {seen}"),
            Err(problem) => {
                healthy = false;
                println!("FAIL  {name}: {problem}");
            }
        }
    }
    println!(
        "{}",
        if healthy {
            "this host can run VMs"
        } else {
            "this host cannot run VMs until the failures above are fixed"
        }
    );
    healthy
}

fn platform() -> Found {
    let (os, arch) = (std::env::consts::OS, std::env::consts::ARCH);
    if (os, arch) == ("linux", "x86_64") {
        Ok(format!("{os} {arch}"))
    } else {
        Err(format!(
            "{os} {arch}; only Linux x86_64 is supported for now"
        ))
    }
}

fn root() -> Found {
    if rustix::process::geteuid().is_root() {
        Ok("running as root".into())
    } else {
        Err(
            "not root; run and serve need root to confine their VMs, so run doctor as root too"
                .into(),
        )
    }
}

fn kvm() -> Found {
    match OpenOptions::new().read(true).write(true).open("/dev/kvm") {
        Ok(_) => Ok("/dev/kvm opens".into()),
        Err(e) => Err(format!(
            "/dev/kvm: {e}; enable virtualization in the firmware, load kvm_intel or kvm_amd, \
             or, in a cloud VM, use an instance type with nested virtualization"
        )),
    }
}

fn cgroups() -> Found {
    let root = Path::new(cgroup::DEFAULT_ROOT)
        .parent()
        .unwrap_or(Path::new("/sys/fs/cgroup"));
    let available = fs::read_to_string(root.join("cgroup.controllers")).map_err(|_| {
        format!(
            "{} is not cgroup v2; boot with the unified hierarchy",
            root.display()
        )
    })?;
    let missing: Vec<&str> = ["cpu", "io", "memory", "pids"]
        .into_iter()
        .filter(|c| !available.split_whitespace().any(|a| a == *c))
        .collect();
    if missing.is_empty() {
        Ok("cpu, io, memory and pids controllers".into())
    } else {
        Err(format!(
            "{} lacks the {} controller(s)",
            root.display(),
            missing.join(", ")
        ))
    }
}

fn user_namespaces() -> Found {
    match fs::read_to_string("/proc/sys/user/max_user_namespaces")
        .ok()
        .and_then(|n| n.trim().parse::<u64>().ok())
    {
        Some(0) => {
            Err("user.max_user_namespaces is 0; the workspace server needs one per run".into())
        }
        Some(n) => Ok(format!("up to {n}")),
        None => Err("the kernel has no user namespaces; the workspace server needs them".into()),
    }
}

fn nftables() -> Found {
    let version = run(Command::new("nft").arg("--version"))
        .map_err(|e| format!("nft: {e}; install nftables"))?;
    run(Command::new("nft").args(["list", "tables"]))
        .map_err(|e| format!("nft cannot list tables: {e}"))?;
    Ok(version)
}

fn disk_images() -> Found {
    run(Command::new("mkfs.ext4").arg("-V"))
        .map_err(|e| format!("mkfs.ext4: {e}; install e2fsprogs (serve's workspaces need it)"))?;
    if !Path::new("/dev/loop-control").exists() {
        return Err(
            "no /dev/loop-control; load the loop module (serve's workspaces need it)".into(),
        );
    }
    Ok("mkfs.ext4 and loop devices".into())
}

fn virtiofsd(path: &Path) -> Found {
    let version = run(Command::new(path).arg("--version")).map_err(|e| {
        format!(
            "{}: {e}; install virtiofsd 1.10 or later, or pass --virtiofsd",
            path.display()
        )
    })?;
    Ok(format!("{version} at {}", path.display()))
}

fn libkrun(path: Option<PathBuf>) -> Found {
    let path = path.unwrap_or_else(bundle::libkrun);
    krun::Libkrun::load(&path).map_err(|e| {
        format!(
            "{e:#}; it must be the peerspan branch of peerspan/libkrun, built with \
             `make VHOST_USER=1 NET=1`"
        )
    })?;
    // SAFETY: loading the library ran its initializers already, above.
    let networking = unsafe { libloading::Library::new(&path) }
        .ok()
        .and_then(|library| {
            // SAFETY: the symbol is only looked up, not called.
            unsafe { library.get::<*const ()>(b"krun_net_device_new_unixstream_fd\0") }
                .ok()
                .map(|_| ())
        });
    match networking {
        Some(()) => Ok(format!("{}, with networking", path.display())),
        None => Err(format!(
            "{} was built without networking; rebuild it with `make VHOST_USER=1 NET=1`",
            path.display()
        )),
    }
}

fn passt(path: Option<PathBuf>) -> Found {
    let path = path.unwrap_or_else(bundle::passt);
    let version = run(Command::new(&path).arg("--version")).map_err(|e| {
        format!(
            "{}: {e}; build it with scripts/build-passt.sh, or pass --passt",
            path.display()
        )
    })?;
    let first = version.lines().next().unwrap_or_default().to_owned();
    Ok(format!("{first} at {}", path.display()))
}

fn guest(path: Option<PathBuf>) -> Found {
    let dir = match path {
        Some(dir) => dir,
        None => bundle::guest().map_err(|e| format!("{e:#}"))?,
    };
    let missing: Vec<&str> = ["vmlinux", "initramfs.cpio.gz", "rootfs"]
        .into_iter()
        .filter(|name| !dir.join(name).exists())
        .collect();
    if missing.is_empty() {
        Ok(dir.display().to_string())
    } else {
        Err(format!(
            "{} lacks {}; build it with scripts/build-guest.sh",
            dir.display(),
            missing.join(", ")
        ))
    }
}

/// Runs `command` and returns the first line of its output, or why it failed.
fn run(command: &mut Command) -> Result<String, String> {
    let output = command.output().map_err(|e| e.to_string())?;
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        return Err(format!("{} ({})", stderr.trim(), output.status));
    }
    let text = String::from_utf8_lossy(&output.stdout);
    Ok(text.lines().next().unwrap_or_default().trim().to_owned())
}
