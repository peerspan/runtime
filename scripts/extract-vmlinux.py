#!/usr/bin/env python3
# SPDX-License-Identifier: Apache-2.0
# Copyright The Peerspan Authors
"""Extract an uncompressed ELF vmlinux from an x86 bzImage, which libkrun cannot load directly.

usage: extract-vmlinux.py BZIMAGE OUT
"""
import subprocess
import sys

MAGICS = [(b"\x28\xb5\x2f\xfd", ["zstd", "-dqc"]), (b"\x1f\x8b\x08", ["gzip", "-dqc"]),
          (b"\xfd7zXZ\x00", ["xz", "-dqc"])]

data = open(sys.argv[1], "rb").read()
for magic, cmd in MAGICS:
    pos = data.find(magic)
    while pos != -1:
        out = subprocess.run(cmd, input=data[pos:], capture_output=True).stdout
        if out.startswith(b"\x7fELF"):
            open(sys.argv[2], "wb").write(out)
            sys.exit(0)
        pos = data.find(magic, pos + 1)
sys.exit(f"no compressed ELF kernel found in {sys.argv[1]}")
