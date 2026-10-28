#!/bin/sh
# SPDX-License-Identifier: Apache-2.0
# Copyright The Peerspan Authors
#
# Installs a peerspan-runtime release, then checks the host with `peerspan-runtime doctor`.
#
#   curl -fsSL https://raw.githubusercontent.com/peerspan/runtime/main/scripts/install.sh | sudo sh
#
# Releases go in PREFIX/lib/peerspan-runtime/VERSION, with `current` linking the installed one
# and PREFIX/bin/peerspan-runtime linking its binary. Where systemd runs, the serve unit is
# installed but not enabled.
#
# usage: install.sh [--version VERSION] [--prefix DIR] [--from TARBALL] [--no-service]
#   --version  a release tag [default: the latest release]
#   --prefix   where to install [default: /usr/local]
#   --from     install this release tarball instead of downloading one
#   --no-service  leave the systemd unit out
set -eu

repo=peerspan/runtime
version=""
prefix=/usr/local
from=""
service=1
die() { echo "install.sh: $*" >&2; exit 1; }
while [ $# -gt 0 ]; do
  case $1 in
    --version) version=$2; shift 2 ;;
    --prefix) prefix=$2; shift 2 ;;
    --from) from=$2; shift 2 ;;
    --no-service) service=0; shift ;;
    *) die "unknown option $1" ;;
  esac
done

[ "$(uname -s) $(uname -m)" = "Linux x86_64" ] || die "only Linux x86_64 is supported for now"
[ "$(id -u)" -eq 0 ] || die "run as root: the runtime needs root to confine its VMs"

tmp=$(mktemp -d)
trap 'rm -rf "$tmp"' EXIT
if [ -z "$from" ]; then
  if [ -z "$version" ]; then
    version=$(curl -fsSL "https://api.github.com/repos/$repo/releases/latest" \
      | sed -n 's/.*"tag_name": *"\([^"]*\)".*/\1/p' | head -1)
    [ -n "$version" ] || die "could not find the latest release of $repo"
  fi
  name="peerspan-runtime-$version-linux-x86_64.tar.gz"
  url="https://github.com/$repo/releases/download/$version/$name"
  echo "downloading $url"
  curl -fsSL -o "$tmp/$name" "$url" || die "could not download $url"
  curl -fsSL -o "$tmp/$name.sha256" "$url.sha256" || die "could not download $url.sha256"
  (cd "$tmp" && sha256sum -c --quiet "$name.sha256") || die "$name does not match its checksum"
  from="$tmp/$name"
fi

top=$(tar -tzf "$from" | head -1 | cut -d/ -f1)
version=${top#peerspan-runtime-}
version=${version%-linux-x86_64}
[ -n "$version" ] && [ "$top" = "peerspan-runtime-$version-linux-x86_64" ] \
  || die "$from is not a peerspan-runtime release"

lib="$prefix/lib/peerspan-runtime"
mkdir -p "$lib" "$prefix/bin"
rm -rf "$lib/.$version.new"
mkdir "$lib/.$version.new"
tar -xzf "$from" -C "$lib/.$version.new" --strip-components=1 --no-same-owner
# Root runs these files, so only root may change them, whatever the archive says.
chown -R 0:0 "$lib/.$version.new"
chmod -R go-w "$lib/.$version.new"
rm -rf "$lib/$version"
mv "$lib/.$version.new" "$lib/$version"
ln -sfn "$version" "$lib/current"
ln -sfn "$lib/current/peerspan-runtime" "$prefix/bin/peerspan-runtime"
echo "installed peerspan-runtime $version in $lib/$version"

if [ "$service" -eq 1 ] && [ -d /run/systemd/system ]; then
  sed "s|/usr/local/bin/peerspan-runtime|$prefix/bin/peerspan-runtime|" \
    "$lib/current/peerspan-runtime.service" > /etc/systemd/system/peerspan-runtime.service
  systemctl daemon-reload
  echo "installed the serve unit; start it with: systemctl enable --now peerspan-runtime"
fi

echo
"$prefix/bin/peerspan-runtime" doctor || {
  echo
  echo "On Debian and Ubuntu, the usual missing pieces are: apt-get install virtiofsd nftables e2fsprogs"
}
