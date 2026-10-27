#!/bin/sh
# SPDX-License-Identifier: Apache-2.0
# Copyright The Peerspan Authors
#
# Builds a guest image directory for `peerspan-runtime run --guest`:
#   vmlinux            the kernel, as the ELF image libkrun loads
#   initramfs.cpio.gz  peerspan-guest-init as /init, plus the kernel modules it loads
#   rootfs/            a pinned Alpine minirootfs, shared read-only into the guest
#
# usage: build-guest.sh OUT_DIR
# By default the kernel is the one scripts/build-kernel.sh builds, with everything built in.
# Environment:
#   GUEST_KERNEL          another vmlinux with everything built in
#   GUEST_ROOT            instead, a distribution kernel and its modules under this root (for
#                         example / for the host's), loaded as modules from the initramfs
#   GUEST_KERNEL_VERSION  that distribution kernel's version (default: uname -r)
#
# A distribution kernel needs virtio-mmio command-line devices; virtio-fs and virtio-vsock may
# be built in or modules. Ubuntu's generic kernel has all three.
set -eu

ALPINE_VERSION=3.22.7
ALPINE_SHA256=57e364a241bab0699cd8a760c0fd7b270e9cbcbd3cee822cb2b07ca6d925c16c
# Loaded in this order; a module missing from the kernel tree is assumed built in.
MODULES="virtiofs vsock vmw_vsock_virtio_transport_common vmw_vsock_virtio_transport"

out=$1
here=$(cd "$(dirname "$0")" && pwd)
repo=$(dirname "$here")
root=${GUEST_ROOT:-/}
ver=${GUEST_KERNEL_VERSION:-$(uname -r)}
arch=$(uname -m)

mkdir -p "$out"
out=$(cd "$out" && pwd)
work=$(mktemp -d)
trap 'rm -rf "$work"' EXIT

cargo build --quiet --release --manifest-path "$repo/Cargo.toml" \
  -p peerspan-guest-init --target "$arch-unknown-linux-musl"

if [ -n "${GUEST_ROOT:-}" ]; then
  "$here/extract-vmlinux.py" "$root/boot/vmlinuz-$ver" "$out/vmlinux"
else
  if [ -z "${GUEST_KERNEL:-}" ]; then
    "$here/build-kernel.sh" "$work/kernel" > /dev/null
    GUEST_KERNEL="$work/kernel/vmlinux"
    cp "$work/kernel/kernel.config" "$out/kernel.config"
  fi
  cp "$GUEST_KERNEL" "$out/vmlinux"
  MODULES=""
  ver=$(cat "$(dirname "$GUEST_KERNEL")/kernel.version" 2>/dev/null || echo "custom")
fi

initramfs="$work/initramfs"
mkdir -p "$initramfs/proc" "$initramfs/sys" "$initramfs/dev" "$initramfs/rootfs" \
  "$initramfs/lib/modules"
cp "$repo/target/$arch-unknown-linux-musl/release/peerspan-guest-init" "$initramfs/init"
: > "$initramfs/lib/modules/order"
for name in $MODULES; do
  ko=$(find "$root/lib/modules/$ver" "$root/usr/lib/modules/$ver" -name "$name.ko*" 2>/dev/null \
    | head -1 || true)
  case "$ko" in
    *.zst) zstd -dqc "$ko" > "$initramfs/lib/modules/$name.ko" ;;
    *.xz) xz -dqc "$ko" > "$initramfs/lib/modules/$name.ko" ;;
    *.ko) cp "$ko" "$initramfs/lib/modules/$name.ko" ;;
    *) continue ;;
  esac
  echo "$name.ko" >> "$initramfs/lib/modules/order"
done
(cd "$initramfs" && find . | cpio -o -H newc 2>/dev/null | gzip -9) > "$out/initramfs.cpio.gz"

tarball="alpine-minirootfs-$ALPINE_VERSION-$arch.tar.gz"
cache="${XDG_CACHE_HOME:-$HOME/.cache}/peerspan-runtime"
mkdir -p "$cache"
if ! echo "$ALPINE_SHA256  $cache/$tarball" | sha256sum -c --status 2>/dev/null; then
  minor=$(echo "$ALPINE_VERSION" | cut -d. -f1,2)
  curl -fsSL -o "$cache/$tarball" \
    "https://dl-cdn.alpinelinux.org/alpine/v$minor/releases/$arch/$tarball"
  echo "$ALPINE_SHA256  $cache/$tarball" | sha256sum -c --quiet
fi
rm -rf "$out/rootfs"
mkdir -p "$out/rootfs"
tar -xzf "$cache/$tarball" -C "$out/rootfs"
mkdir -p "$out/rootfs/workspace"

echo "guest image in $out (kernel $ver, $(wc -l < "$initramfs/lib/modules/order") modules)"
