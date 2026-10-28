#!/bin/sh
# SPDX-License-Identifier: Apache-2.0
# Copyright The Peerspan Authors
#
# Builds libkrun, the VMM: a pinned commit of the peerspan branch of peerspan/libkrun, with
# vhost-user devices and networking. Writes OUT_DIR/libkrun.so.2 and OUT_DIR/libkrun.version.
# libkrun is Apache-2.0. Builds are cached by commit. Needs clang and libclang-dev.
#
# usage: build-libkrun.sh OUT_DIR
set -eu

LIBKRUN_COMMIT=bd09e2592ec98466a5a97484dd8a16ece41a0921

out=$1
cache="${XDG_CACHE_HOME:-$HOME/.cache}/peerspan-runtime"
build="$cache/libkrun-$LIBKRUN_COMMIT"
mkdir -p "$cache" "$out"

if [ ! -f "$build/target/release/libkrun.so.2.0.0" ]; then
  rm -rf "$build"
  git init --quiet "$build"
  git -C "$build" fetch --quiet --depth 1 https://github.com/peerspan/libkrun "$LIBKRUN_COMMIT"
  git -C "$build" -c advice.detachedHead=false checkout --quiet FETCH_HEAD
  make -s -C "$build" VHOST_USER=1 NET=1
fi

cp "$build/target/release/libkrun.so.2.0.0" "$out/libkrun.so.2"
echo "$LIBKRUN_COMMIT" > "$out/libkrun.version"
echo "libkrun $LIBKRUN_COMMIT in $out"
