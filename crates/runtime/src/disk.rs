// SPDX-License-Identifier: Apache-2.0
// Copyright The Peerspan Authors

//! Size-limited workspaces for `serve`. Each run's workspace is an ext4 file system in a sparse
//! image file of its own, mounted where the workspace goes, so a run can take no more of the
//! host's disk than the image's size, and the guest sees "no space left" at the limit.

use std::fs::{self, File};
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::OpenOptionsExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use anyhow::{Context, bail};
use rustix::mount::UnmountFlags;

/// Default size of each workspace.
pub const DEFAULT_SIZE_MIB: u32 = 10 * 1024;
/// The image beside each mounted workspace.
pub const IMAGE: &str = "workspace.img";

/// A workspace file system, unmounted and its image removed when dropped.
#[derive(Debug)]
pub struct WorkspaceDisk {
    image: PathBuf,
    mountpoint: PathBuf,
    mounted: bool,
}

impl WorkspaceDisk {
    /// Makes a file system of `size_mib` in a new sparse file at `image` and mounts it at
    /// `mountpoint`, an empty directory, empty: its `lost+found` is removed.
    ///
    /// # Errors
    ///
    /// Fails if the image cannot be created, formatted with `mkfs.ext4`, or mounted.
    pub fn create(image: &Path, mountpoint: &Path, size_mib: u32) -> anyhow::Result<Self> {
        let mut disk = WorkspaceDisk {
            image: image.to_owned(),
            mountpoint: mountpoint.to_owned(),
            mounted: false,
        };
        File::options()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(image)
            .and_then(|file| file.set_len(u64::from(size_mib) << 20))
            .with_context(|| format!("creating {}", image.display()))?;
        // No space reserved for root, and the inode tables and journal left to be zeroed
        // lazily, so the image takes a few megabytes until the run writes to it.
        command(
            Command::new("mkfs.ext4")
                .args(["-q", "-F", "-m", "0", "-E"])
                .arg("lazy_itable_init=1,lazy_journal_init=1")
                .arg(image),
        )?;
        command(
            Command::new("mount")
                .args(["-t", "ext4", "-o", "loop,nosuid,nodev"])
                .arg(image)
                .arg(mountpoint),
        )?;
        disk.mounted = true;
        fs::remove_dir(mountpoint.join("lost+found")).context("emptying the workspace")?;
        Ok(disk)
    }
}

impl Drop for WorkspaceDisk {
    fn drop(&mut self) {
        // The run's processes are gone, so the file system is idle; detaching covers the rare
        // straggler, and the loop device is freed once the last user lets go.
        if self.mounted
            && !unmount(&self.mountpoint, UnmountFlags::empty())
            && !unmount(&self.mountpoint, UnmountFlags::DETACH)
        {
            crate::log::warn(
                "could not unmount a workspace",
                &[("workspace", &self.mountpoint.display())],
            );
        }
        let _ = fs::remove_file(&self.image);
    }
}

/// Detaches workspace file systems a previous `serve` left mounted under `runs`, and removes
/// their images. Run directories hold nothing else that takes much space.
pub fn remove_stale(runs: &Path) {
    let Ok(runs) = fs::canonicalize(runs) else {
        return;
    };
    let Ok(mountinfo) = fs::read_to_string("/proc/self/mountinfo") else {
        return;
    };
    for mountpoint in mountinfo
        .lines()
        .filter_map(|line| line.split(' ').nth(4))
        .map(unescape)
        .filter(|point| {
            point.file_name() == Some("workspace".as_ref())
                && point.parent().and_then(Path::parent) == Some(&runs)
        })
    {
        if unmount(&mountpoint, UnmountFlags::DETACH) {
            crate::log::info(
                "detached a workspace left by an earlier serve",
                &[("workspace", &mountpoint.display())],
            );
        }
    }
    let Ok(entries) = fs::read_dir(&runs) else {
        return;
    };
    for entry in entries.flatten() {
        let _ = fs::remove_file(entry.path().join(IMAGE));
    }
}

fn unmount(path: &Path, flags: UnmountFlags) -> bool {
    rustix::mount::unmount(path, flags).is_ok()
}

/// Undoes mountinfo's octal escapes of space, tab, newline and backslash.
fn unescape(field: &str) -> PathBuf {
    let bytes = field.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        let octal = bytes.get(i + 1..i + 4).and_then(|digits| {
            std::str::from_utf8(digits)
                .ok()
                .and_then(|d| u8::from_str_radix(d, 8).ok())
        });
        match (bytes[i], octal) {
            (b'\\', Some(byte)) => {
                out.push(byte);
                i += 4;
            }
            (byte, _) => {
                out.push(byte);
                i += 1;
            }
        }
    }
    PathBuf::from(std::ffi::OsStr::from_bytes(&out))
}

fn command(command: &mut Command) -> anyhow::Result<()> {
    let program = command.get_program().to_string_lossy().into_owned();
    let output = command
        .stdin(Stdio::null())
        .output()
        .with_context(|| format!("running {program}"))?;
    if !output.status.success() {
        bail!(
            "{program} failed ({}): {}",
            output.status,
            String::from_utf8_lossy(&output.stderr).trim()
        );
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn unescapes_mountinfo_paths() {
        assert_eq!(unescape(r"/a\040b\134c"), PathBuf::from(r"/a b\c"));
        assert_eq!(unescape("/plain"), PathBuf::from("/plain"));
    }

    #[test]
    fn missing_runs_directory_is_nothing_to_remove() {
        remove_stale(Path::new("/nonexistent/peerspan-runtime/runs"));
    }
}
