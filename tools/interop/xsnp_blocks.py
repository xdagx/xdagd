#!/usr/bin/env python3
"""Write every block an XSNP snapshot carries with its data (full blocks and
blocks kept as key material) to a file of 512-byte records.

    xsnp_blocks.py <file.xsnp> <blocks.dat>

The result is the input of the block-parsing comparison (BlockDump.java
against `cargo run -p xdag-chain --example blockdump`).
"""
import struct, sys

f = open(sys.argv[1], "rb", buffering=1 << 20)
assert f.read(4) == b"XSNP" and f.read(1) == b"\x02", "not an XSNP 2 file"
f.read(1 + 8 + 24 + 32)
f.read(struct.unpack("<Q", f.read(8))[0])
for _ in range(struct.unpack("<Q", f.read(8))[0]):  # accounts
    f.read(20)
    f.read(8 if f.read(1)[0] == 0 else 16)
    f.read(8)
    if f.read(1)[0]:
        f.read(32)
n = 0
with open(sys.argv[2], "wb") as out:
    for _ in range(struct.unpack("<Q", f.read(8))[0]):
        f.read(24 + 32 + 8 + 1 + 1 + 8 + 32)
        if f.read(1)[0]:
            f.read(24)
        f.read(16)
        if f.read(1)[0]:
            f.read(32)
        kind = f.read(1)[0]
        data = f.read({0: 0, 1: 33, 2: 512, 3: 512, 4: 512}[kind])
        if kind == 4:
            f.read(struct.unpack("<Q", f.read(8))[0])
        if f.read(1)[0]:
            f.read(24)
        if kind >= 2:
            out.write(data)
            n += 1
print(n, "blocks")
