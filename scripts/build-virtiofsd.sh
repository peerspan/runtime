#!/bin/sh
# SPDX-License-Identifier: Apache-2.0
# Copyright The Peerspan Authors
#
# Builds virtiofsd, which serves a run's directory workspace to its VM: a pinned release from
# crates.io, with its lock file, so its dependencies are pinned too and cargo checks every
# crate against the registry's checksum. Writes OUT_DIR/virtiofsd and OUT_DIR/virtiofsd.version.
# Only Ubuntu 24.04 and later package virtiofsd, so a release carries its own. It links the
# libseccomp and libcap-ng every supported distribution has in its base system; building needs
# libseccomp-dev and libcap-ng-dev. virtiofsd is Apache-2.0 AND BSD-3-Clause.
# Builds are cached by version, so an unchanged virtiofsd is not rebuilt.
#
# usage: build-virtiofsd.sh OUT_DIR
set -eu

VIRTIOFSD_VERSION=1.14.0

out=$1
cache="${XDG_CACHE_HOME:-$HOME/.cache}/peerspan-runtime"
build="$cache/virtiofsd-$VIRTIOFSD_VERSION"
mkdir -p "$cache" "$out"

if [ ! -x "$build/bin/virtiofsd" ]; then
  rm -rf "$build"
  cargo install --quiet --locked --root "$build" --version "$VIRTIOFSD_VERSION" virtiofsd
fi

cp "$build/bin/virtiofsd" "$out/virtiofsd"
echo "$VIRTIOFSD_VERSION" > "$out/virtiofsd.version"
echo "virtiofsd $VIRTIOFSD_VERSION in $out"
