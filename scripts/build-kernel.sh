#!/bin/sh
# SPDX-License-Identifier: Apache-2.0
# Copyright The Peerspan Authors
#
# Builds the guest kernel: a pinned kernel.org release configured by guest/kernel.config on top
# of allnoconfig, with everything the guest needs built in. Writes OUT_DIR/vmlinux, the
# resolved OUT_DIR/kernel.config, and OUT_DIR/kernel.version. Builds are cached by version and
# config, so an unchanged kernel is not rebuilt.
#
# usage: build-kernel.sh OUT_DIR
set -eu

KERNEL_VERSION=6.18.59
KERNEL_SHA256=53a55ac9863a5547e292b8e381d4e4c2240dd1127d5428a7c44dbc550be23963

out=$1
here=$(cd "$(dirname "$0")" && pwd)
fragment="$here/../guest/kernel.config"
cache="${XDG_CACHE_HOME:-$HOME/.cache}/peerspan-runtime"
key="$KERNEL_VERSION-$(sha256sum "$fragment" | cut -c1-12)"
build="$cache/kernel-$key"
mkdir -p "$cache" "$out"

if [ ! -f "$build/vmlinux" ]; then
  tarball="$cache/linux-$KERNEL_VERSION.tar.xz"
  if ! echo "$KERNEL_SHA256  $tarball" | sha256sum -c --status 2>/dev/null; then
    major=$(echo "$KERNEL_VERSION" | cut -d. -f1)
    curl -fsSL -o "$tarball" \
      "https://cdn.kernel.org/pub/linux/kernel/v$major.x/linux-$KERNEL_VERSION.tar.xz"
    echo "$KERNEL_SHA256  $tarball" | sha256sum -c --quiet
  fi
  src="$cache/linux-$KERNEL_VERSION"
  [ -d "$src" ] || tar -xJf "$tarball" -C "$cache"
  rm -rf "$build"
  mkdir -p "$build"
  make -s -C "$src" O="$build" allnoconfig KCONFIG_ALLCONFIG="$fragment"
  # Every line of the fragment must survive into the resolved config, or a dependency is
  # missing from the fragment.
  grep '^CONFIG_' "$fragment" | while read -r line; do
    grep -qx "$line" "$build/.config" || { echo "kernel config dropped $line" >&2; exit 1; }
  done
  make -s -C "$src" O="$build" -j"$(nproc)" vmlinux
fi

cp "$build/vmlinux" "$out/vmlinux"
cp "$build/.config" "$out/kernel.config"
echo "$KERNEL_VERSION" > "$out/kernel.version"
echo "kernel $KERNEL_VERSION in $out ($(du -h "$out/vmlinux" | cut -f1))"
