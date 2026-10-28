#!/bin/sh
# SPDX-License-Identifier: Apache-2.0
# Copyright The Peerspan Authors
#
# Builds a release: OUT_DIR/peerspan-runtime-VERSION-linux-x86_64.tar.gz and its .sha256,
# holding the runtime with libkrun, passt and the guest image beside it, as it finds them.
# The binaries need the glibc of the host that builds them, so build releases on the oldest
# distribution to support; the script prints the glibc version the result needs.
#
# usage: build-release.sh OUT_DIR [VERSION]
set -eu

out=$1
repo=$(cd "$(dirname "$0")/.." && pwd)
version=${2:-$(git -C "$repo" describe --tags --always --dirty)}
name="peerspan-runtime-$version-linux-x86_64"
stage="$out/$name"
rm -rf "$stage"
mkdir -p "$stage"

cargo build --quiet --release --manifest-path "$repo/Cargo.toml" -p peerspan-runtime
cp "$repo/target/release/peerspan-runtime" "$stage/"
"$repo/scripts/build-libkrun.sh" "$stage"
"$repo/scripts/build-passt.sh" "$stage"
"$repo/scripts/build-guest.sh" "$stage/guest"
cp "$repo/dist/peerspan-runtime.service" "$stage/"
echo "$version" > "$stage/VERSION"

kernel=$(sed -n 's/^KERNEL_VERSION=//p' "$repo/scripts/build-kernel.sh")
alpine=$(sed -n 's/^ALPINE_VERSION=//p' "$repo/scripts/build-guest.sh")
mkdir -p "$stage/licenses"
cp "$repo/LICENSE" "$repo/NOTICE" "$stage/licenses/"
cat > "$stage/licenses/THIRD_PARTY.md" <<NOTES
# Third-party components

This release bundles the following, unmodified unless noted.

| Component | Version | License | Source |
| --- | --- | --- | --- |
| libkrun (\`libkrun.so.2\`) | peerspan/libkrun \`$(cat "$stage/libkrun.version")\`, a thin fork | Apache-2.0 | https://github.com/peerspan/libkrun |
| passt (\`passt\`, \`passt.avx2\`) | $(cat "$stage/passt.version") | GPL-2.0-or-later | https://passt.top/passt, at that release |
| Linux (\`guest/vmlinux\`) | $kernel, configured by \`guest/kernel.config\` | GPL-2.0 | https://kernel.org, at that release |
| Alpine Linux minirootfs (\`guest/rootfs\`) | $alpine | Various, chiefly GPL-2.0 (BusyBox) and MIT (musl) | https://alpinelinux.org, at that release |

The runtime's own Rust dependencies are under MIT, Apache-2.0 and similarly permissive
licenses, among them CDLA-Permissive-2.0 for webpki-roots.
NOTES

glibc=$(for f in "$stage/peerspan-runtime" "$stage/libkrun.so.2"; do
  objdump -T "$f" | grep -oE 'GLIBC_[0-9]+\.[0-9]+' ; done | sort -t. -k2 -n -u | tail -1)

# Owned by root whoever builds it: root runs these files.
tar -C "$out" --owner=0 --group=0 --numeric-owner -czf "$out/$name.tar.gz" "$name"
(cd "$out" && sha256sum "$name.tar.gz" > "$name.tar.gz.sha256")
rm -rf "$stage"
echo "release $out/$name.tar.gz ($(du -h "$out/$name.tar.gz" | cut -f1)), needs ${glibc#GLIBC_} or later"
