#!/usr/bin/env python3
"""Compare, on both nodes, the blocks xdagj generated itself (they lose against
xdagd's mined candidates, so they are not on the main chain)."""
import base64, json, os, re, sys, urllib.request
IT = os.environ.get('IT', '.')
last = int(sys.argv[1]) if len(sys.argv) > 1 else 10
def rpc(port, m, p=[]):
    req = urllib.request.Request(f"http://127.0.0.1:{port}", data=json.dumps({"jsonrpc": "2.0", "id": 1, "method": m, "params": p}).encode(), headers={"content-type": "application/json"})
    return json.load(urllib.request.urlopen(req, timeout=10)).get("result")
log = open(IT + "/xdagj-node/logs/xdag-debug.log", errors="replace").read()
own = re.findall(r"Broadcast locally generated blockchain, waiting to be verified\. block hash = \[0x([0-9a-f]{64})\]", log)
info = rpc(10201, "xdag_getChainInfo")["randomx"]
print(f"xdagj generated {len(own)} candidates; xdagd RandomX: fork epoch {info['forkEpoch']}, seeds {[(s['height'], s['switchEpoch']) for s in info['seeds']]}")
bad = n = 0
for h in own[-last:]:
    addr = base64.b64encode(bytes.fromhex(h)[8:][::-1]).decode()
    j = rpc(10101, "xdag_getBlockByHash", [addr, "1"]); d = rpc(10201, "xdag_getBlockByHash", [addr, "1"])
    if j is None or d is None:
        print("  ", addr, "not known to", "xdagj" if j is None else "xdagd"); continue
    n += 1; same = j["diff"] == d["diff"]; bad += not same
    ep = j["timeStamp"] >> 16
    rx = info["forkEpoch"] is not None and ep > info["forkEpoch"]
    print("  ", addr, "epoch", ep, "RandomX" if rx else "sha256d", "| diff xdagj", j["diff"], "xdagd", d["diff"], "| OK" if same else "| MISMATCH")
print(f"compared {n} xdagj-generated blocks: {bad} mismatches")
sys.exit(1 if bad else 0)
