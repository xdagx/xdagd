#!/usr/bin/env python3
"""Ask both test nodes for the balances recorded in a snapshot file.

    compare_state.py <file.xsnp> [blocks to sample (default 2000)] [xdagd rpc port]

Every account of the snapshot and a sample of its blocks are looked up on the
xdagj node and on the xdagd node through JSON-RPC (xdag_getBalance); the two
answers must agree with each other and with the snapshot.
"""
import base64, hashlib, json, random, struct, sys, urllib.request
from decimal import Decimal, ROUND_HALF_UP

J, D = 10101, int(sys.argv[3]) if len(sys.argv) > 3 else 10201
SAMPLE = int(sys.argv[2]) if len(sys.argv) > 2 else 2000
B58 = "123456789ABCDEFGHJKLMNPQRSTUVWXYZabcdefghijkmnopqrstuvwxyz"


def base58check(data):
    data += hashlib.sha256(hashlib.sha256(data).digest()).digest()[:4]
    n, out = int.from_bytes(data, "big"), ""
    while n:
        n, r = divmod(n, 58)
        out = B58[r] + out
    return "1" * (len(data) - len(data.lstrip(b"\0"))) + out


def rpc(port, method, params):
    body = json.dumps({"jsonrpc": "2.0", "id": 1, "method": method, "params": params}).encode()
    req = urllib.request.Request(f"http://127.0.0.1:{port}", data=body, headers={"content-type": "application/json"})
    r = json.load(urllib.request.urlopen(req, timeout=20))
    return r.get("result") if "result" in r else "error: %s" % r.get("error")


def nano(text):  # "-0.642000000" -> -642000000
    sign = -1 if text.startswith("-") else 1
    whole, frac = text.lstrip("-").split(".")
    return sign * (int(whole) * 10**9 + int(frac))


def c_units_to_nano(c):  # as xdagj reads a stored balance (XAmount.ofXAmount)
    xdag = Decimal((c >> 32) + (c & 0xffffffff) / 2**32)
    return int(xdag.scaleb(9).quantize(1, rounding=ROUND_HALF_UP))


f = open(sys.argv[1], "rb", buffering=1 << 20)
assert f.read(4) == b"XSNP" and f.read(1) == b"\x02", "not an XSNP 2 file"
f.read(1 + 8 + 24 + 32)
f.read(struct.unpack("<Q", f.read(8))[0])
accounts = []
for _ in range(struct.unpack("<Q", f.read(8))[0]):
    addr, kind = f.read(20), f.read(1)[0]
    bal = c_units_to_nano(struct.unpack("<Q", f.read(8))[0]) if kind == 0 else int.from_bytes(f.read(16), "little") // 10**9
    f.read(8)
    if f.read(1)[0]:
        f.read(32)
    accounts.append((base58check(addr), bal))
blocks, special = [], []
total = struct.unpack("<Q", f.read(8))[0]
rng = random.Random(1)
for i in range(total):
    head = f.read(24 + 32 + 8 + 1 + 1 + 8 + 32)
    flags = head[64]
    if f.read(1)[0]:
        f.read(24)
    amount, _fee = struct.unpack("<qQ", f.read(16))
    if f.read(1)[0]:
        f.read(32)
    kind = f.read(1)[0]
    f.read({0: 0, 1: 33, 2: 512, 3: 512, 4: 512}[kind])
    if kind == 4:
        f.read(struct.unpack("<Q", f.read(8))[0])
    if f.read(1)[0]:
        f.read(24)
    entry = (base64.b64encode(head[:24]).decode(), amount)
    # every unusual block, and a uniform sample of the rest (reservoir)
    if amount < 0 or kind != 1 or flags & 0x7f not in (0x1c, 0x1f):
        special.append(entry)
    elif len(blocks) < SAMPLE:
        blocks.append(entry)
    elif rng.randrange(i + 1) < SAMPLE:
        blocks[rng.randrange(SAMPLE)] = entry
special = rng.sample(special, min(len(special), SAMPLE))

bad = 0
for what, items in (("accounts", accounts), ("blocks", blocks), ("unusual blocks", special)):
    wrong = 0
    for name, want in items:
        j, d = rpc(J, "xdag_getBalance", [name]), rpc(D, "xdag_getBalance", [name])
        if j != d or not isinstance(j, str) or nano(j) != want:
            wrong += 1
            if wrong <= 5:
                print(f"  {name}: snapshot {want} nano, xdagj {j}, xdagd {d}")
    print(f"{what}: {len(items)} asked, {wrong} answers differ")
    bad += wrong
sys.exit(1 if bad else 0)
