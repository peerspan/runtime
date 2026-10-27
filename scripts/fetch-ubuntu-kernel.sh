#!/bin/sh
# SPDX-License-Identifier: Apache-2.0
# Copyright The Peerspan Authors
# Unpack the current Ubuntu generic kernel and modules into DIR, for hosts whose own
# kernel lacks virtio-mmio command-line devices or virtio-fs (e.g. cloud CI kernels).
# usage: fetch-ubuntu-kernel.sh DIR   (prints the kernel version)
set -eu
dir=$1; mkdir -p "$dir"; cd "$dir"
pkg=$(apt-cache depends linux-image-generic | grep -o 'linux-image-[0-9][^ ]*-generic' | head -1)
ver=${pkg#linux-image-}
apt-get download -q "$pkg" "linux-modules-$ver" >/dev/null
for deb in *.deb; do dpkg-deb -x "$deb" .; done
echo "$ver"
