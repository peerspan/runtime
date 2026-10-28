// SPDX-License-Identifier: Apache-2.0
// Copyright The Peerspan Authors

//! Files the runtime ships with, beside its own binary: passt, libkrun and the guest image, as
//! an installed release lays them out. Flags and environment variables name others.

use std::env;
use std::path::PathBuf;

/// The guest image directory in a release.
pub const GUEST: &str = "guest";
/// libkrun's file name, as the dynamic loader also finds it.
pub const LIBKRUN: &str = "libkrun.so.2";

/// `name` beside this binary, if it is there.
fn beside(name: &str) -> Option<PathBuf> {
    let path = env::current_exe().ok()?.parent()?.join(name);
    path.exists().then_some(path)
}

/// The passt the runtime ships with: a pinned release, built by `scripts/build-passt.sh`,
/// whose arguments `net::passt_args` is written against. Otherwise the one on `PATH`.
pub fn passt() -> PathBuf {
    beside("passt").unwrap_or_else(|| PathBuf::from("passt"))
}

/// The libkrun the runtime ships with, or the dynamic loader's.
pub fn libkrun() -> PathBuf {
    beside(LIBKRUN).unwrap_or_else(|| PathBuf::from(LIBKRUN))
}

/// The guest image the runtime ships with, if any.
pub fn guest() -> anyhow::Result<PathBuf> {
    beside(GUEST).ok_or_else(|| {
        anyhow::anyhow!(
            "no guest image: pass --guest or set PEERSPAN_RUNTIME_GUEST, or install a release, \
             which has one beside the binary"
        )
    })
}
