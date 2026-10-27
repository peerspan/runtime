#!/bin/sh
# SPDX-License-Identifier: Apache-2.0
# Copyright The Peerspan Authors
#
# Builds passt, the guest's network backend: a pinned release, checked against its commit and
# linked statically so it runs on any Linux host beside the runtime. Writes OUT_DIR/passt,
# OUT_DIR/passt.avx2 (which passt switches to on CPUs with AVX2) and OUT_DIR/passt.version. passt is GPL-2.0-or-later; its source is the pinned commit below.
# Builds are cached by release, so an unchanged passt is not rebuilt.
#
# usage: build-passt.sh OUT_DIR
set -eu

PASST_RELEASE=2026_10_02.cba3570
PASST_COMMIT=cba357068dc586e7540971eb40bb56c5e62c4de1

out=$1
cache="${XDG_CACHE_HOME:-$HOME/.cache}/peerspan-runtime"
build="$cache/passt-$PASST_RELEASE"
mkdir -p "$cache" "$out"

if [ ! -x "$build/passt" ]; then
  rm -rf "$build"
  git -c advice.detachedHead=false clone --quiet --depth 1 --branch "$PASST_RELEASE" https://passt.top/passt "$build"
  commit=$(git -C "$build" rev-parse HEAD)
  if [ "$commit" != "$PASST_COMMIT" ]; then
    echo "passt $PASST_RELEASE is commit $commit, want $PASST_COMMIT" >&2
    rm -rf "$build"
    exit 1
  fi
  make -s -C "$build" -j"$(nproc)" VERSION="$PASST_RELEASE" static
fi

cp "$build/passt" "$out/passt"
if [ -f "$build/passt.avx2" ]; then cp "$build/passt.avx2" "$out/passt.avx2"; fi
echo "$PASST_RELEASE" > "$out/passt.version"
echo "passt $PASST_RELEASE in $out"
