#!/usr/bin/env python3
"""Compare the main chains (and optionally balances) of the xdagj and xdagd test nodes."""
import json, sys, urllib.request
J = 10101
D = int(sys.argv[1]) if len(sys.argv) > 1 and sys.argv[1].isdigit() else 10201
def rpc(port, m, p=[]):
    req = urllib.request.Request(f"http://127.0.0.1:{port}", data=json.dumps({"jsonrpc": "2.0", "id": 1, "method": m, "params": p}).encode(), headers={"content-type": "application/json"})
    r = json.load(urllib.request.urlopen(req, timeout=10))
    return r.get("result") if "result" in r else {"error": r.get("error")}
sj, sd = rpc(J, "xdag_getStatus"), rpc(D, "xdag_getStatus")
print(f"xdagj nmain {sj['nmain']} nblock {sj['nblock']} diff {sj['curDiff']} supply {sj['netSupply']}")
print(f"xdagd nmain {sd['nmain']} nblock {sd['nblock']} diff {sd['curDiff']} supply {sd['netSupply']} synced {sd.get('synced')}")
n = min(int(sj["nmain"]), int(sd["nmain"]))
# the newest main blocks (both nodes list at most 100 at a time)
bj = {b["height"]: b for b in rpc(J, "xdag_getBlocksByNumber", [str(min(n + 2, 100))])}
bd = {b["height"]: b for b in rpc(D, "xdag_getBlocksByNumber", [str(min(n + 2, 100))])}
first = max(1, min(list(bj) + list(bd)))
diffs = 0
for h in range(first, n + 1):
    a, b = bj.get(h), bd.get(h)
    if a is None or b is None:
        diffs += 1; print(f"  height {h}: missing on {'xdagj' if a is None else 'xdagd'}"); continue
    for k in ("address", "hash", "balance", "diff", "state", "type", "blockTime", "remark"):
        if a.get(k) != b.get(k):
            diffs += 1; print(f"  height {h} {k}: xdagj={a.get(k)!r} xdagd={b.get(k)!r}")
print(f"main blocks {first}..{n}: {diffs} differences")
for addr in sys.argv[2:]:
    print(f"  balance {addr}: xdagj={rpc(J, 'xdag_getBalance', [addr])} xdagd={rpc(D, 'xdag_getBalance', [addr])}")
sys.exit(1 if diffs else 0)
