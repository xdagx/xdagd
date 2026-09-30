#!/bin/bash
# Helpers for running an xdagj devnet node next to xdagd nodes on one machine
# (see README.md). Source this file after setting:
#   IT         work directory (node data, logs)
#   XDAGD      path of the xdagd binary            (default: xdagd)
#   JAVA       path of a JDK 21 `java`             (default: java)
#   XDAGJ_JAR  xdagj-<version>-executable.jar      (default: $IT/xdagj-node/xdagj-0.8.4-executable.jar)
#
# Both nodes run as transient systemd units with a hard memory limit and with
# network access confined to the loopback interface, so a test can neither
# starve the machine nor reach (or be reached from) any real network.
: "${IT:?set IT to a work directory}"
: "${XDAGD:=xdagd}"
: "${JAVA:=java}"
: "${XDAGJ_JAR:=$IT/xdagj-node/xdagj-0.8.4-executable.jar}"
HERE=$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)

# ports: xdagj p2p 8101, rpc 10101, telnet 6101, pool 7101; xdagd p2p 8102, rpc 10201
interop_setup() {
  mkdir -p "$IT/xdagj-node/devnet/wallet" "$IT/xdagd-node" "$IT/tmp"
  cat > "$IT/xdagj-node/xdag-devnet.conf" <<CONF
admin.telnet.ip = 127.0.0.1
admin.telnet.port = 6101
admin.telnet.password = root
pool.ws.port = 7101
pool.whiteIPs = ["127.0.0.1"]
node.ip = 127.0.0.1
node.port = 8101
node.tag = xdagj-dev
node.maxInboundConnectionsPerIp = 8
node.whiteIPs = ["127.0.0.1:8102"]
node.generate.block.enable = true
node.transaction.history.enable = false
rpc.http.enabled = true
rpc.http.host = 127.0.0.1
rpc.http.port = 10101
rpc.ws.port = 10102
randomx.flags.fullmem = false
CONF
  cat > "$IT/xdagd-node/interop.toml" <<CONF
network = "devnet"
datadir = "$IT/xdagd-node/data"
node_tag = "xdagd-interop"

[p2p]
listen = "127.0.0.1:8102"
seeds = ["127.0.0.1:8101"]
allow_private = true

[rpc]
enabled = true
listen = "127.0.0.1:10201"

[mining]
generate_blocks = true
threads = 1

[nova]
# xdagj's rules only: Nova never activates
activation_epoch = 9223372036854775807
CONF
  echo "created $IT/xdagj-node/xdag-devnet.conf and $IT/xdagd-node/interop.toml"
  echo "still needed in $IT/xdagj-node: the executable jar, log4j2.xml, devnet/wallet/wallet.data (see README.md)"
}

# Append to interop.toml to reach the RandomX fork within minutes (xdagj is
# then started with start_xdagj_rx, which shortens its schedule the same way).
interop_randomx_config() {
  cat >> "$IT/xdagd-node/interop.toml" <<CONF

[randomx]
disabled = false
fork_height = 16
seed_epoch_blocks = 8
seed_lag = 2
cache_seeds = 1
CONF
}

_unit() {  # name, memory limit, working directory, log file, then the command
  local name=$1 mem=$2 dir=$3 log=$4; shift 4
  systemd-run --unit="$name" --collect --quiet \
    -p MemoryMax="$mem" -p MemorySwapMax=0 -p Nice=5 -p IPAddressDeny=any -p IPAddressAllow=localhost \
    -p WorkingDirectory="$dir" -p StandardOutput=append:"$log" -p StandardError=append:"$log" \
    --setenv=XDAGJ_WALLET_PASSWORD="${XDAGJ_WALLET_PASSWORD:-interop-pw}" \
    --setenv=XDAG_WALLET_PASSWORD="${XDAG_WALLET_PASSWORD:-xdagd-pw}" \
    --setenv=RUST_LOG="${XDAGD_LOG:-info,xdag_net=debug}" "$@"
}
_java_opts="--add-opens java.base/java.nio=ALL-UNNAMED --add-opens java.base/sun.nio.ch=ALL-UNNAMED -Xms128m -Xmx640m -XX:+UseSerialGC -XX:+ExitOnOutOfMemoryError -Dxdagj.version=0.8.4"

start_xdagj() {
  _unit xdagj-interop 1700M "$IT/xdagj-node" "$IT/xdagj-node/stdout.log" \
    "$JAVA" $_java_opts -Djava.io.tmpdir="$IT/tmp" -cp ".:$XDAGJ_JAR" io.xdag.Bootstrap -d
}
# xdagj with the RandomX fork at height 16, a seed every 8 blocks and lag 2
# (needs InteropMain.class, compiled from InteropMain.java, in $IT/wrapper)
start_xdagj_rx() {
  _unit xdagj-interop 1700M "$IT/xdagj-node" "$IT/xdagj-node/stdout.log" \
    "$JAVA" $_java_opts -Djava.io.tmpdir="$IT/tmp" -Dinterop.rx.fork=16 -Dinterop.rx.epoch=8 -Dinterop.rx.lag=2 \
    -cp ".:$IT/wrapper:$XDAGJ_JAR" InteropMain -d
}
start_xdagd() {  # optional argument: another configuration file
  _unit xdagd-interop 700M "$IT/xdagd-node" "$IT/xdagd-node/stdout.log" "$XDAGD" --config "${1:-$IT/xdagd-node/interop.toml}" run
}

# --- a development network that starts from an xdagj snapshot -----------------
# (README.md, "从主网快照开始的开发网"). Both nodes keep their devnet identity:
# network id 2 and HEAD_TEST blocks, which no mainnet node accepts.

# Replace interop.toml by one with mainnet's RandomX schedule, under which the
# snapshot's main chain selects its seeds.
interop_snapshot_config() {
  cat > "$IT/xdagd-node/interop.toml" <<CONF
network = "devnet"
datadir = "$IT/xdagd-node/data"
node_tag = "xdagd-interop"
db_cache_mb = 64

[p2p]
listen = "127.0.0.1:8102"
seeds = ["127.0.0.1:8101"]
allow_private = true

[rpc]
enabled = true
listen = "127.0.0.1:10201"

[mining]
generate_blocks = true
threads = 1

[nova]
activation_epoch = 9223372036854775807

[randomx]
disabled = false
fork_height = 1540096
seed_epoch_blocks = 4096
seed_lag = 128
cache_seeds = 1
CONF
}
# xdagj started from the snapshot in devnet/rocksdb/xdagdb/SNAPSHOT, with
# mainnet's RandomX schedule. Arguments: the snapshot's height and time (hex),
# as in its file name. xdagj needs about 1.7 GB here: two seeds, two 256 MB
# caches each.
start_xdagj_snapshot() {
  _unit xdagj-interop 2000M "$IT/xdagj-node" "$IT/xdagj-node/stdout.log" \
    "$JAVA" $_java_opts -Djava.io.tmpdir="$IT/tmp" -Dinterop.rx.fork=1540096 -Dinterop.rx.epoch=4096 -Dinterop.rx.lag=128 \
    -cp ".:$IT/wrapper:$XDAGJ_JAR" InteropMain -d --enablesnapshot true "$1" "$2"
}
stop_xdagj() { systemctl stop xdagj-interop 2>/dev/null; }
stop_xdagd() { systemctl stop xdagd-interop 2>/dev/null; }

rpc() { curl -s -m 8 -H 'content-type: application/json' -d "{\"jsonrpc\":\"2.0\",\"id\":1,\"method\":\"$2\",\"params\":${3:-[]}}" "http://127.0.0.1:$1"; }
rpcj() { rpc 10101 "$@"; }
rpcd() { rpc 10201 "$@"; }
interop_mem() {
  for u in xdagj-interop xdagd-interop; do
    printf "%s: %s MB  " "$u" $(( $(systemctl show -p MemoryCurrent --value "$u" 2>/dev/null | grep -E '^[0-9]+$' || echo 0) / 1048576 ))
  done
  echo
}
