#!/bin/bash
# Offline checks of an xdagj snapshot against xdagd. No node is started and
# nothing touches a network: the snapshot is converted, audited, imported,
# exported again and compared, and every block it carries is parsed by both
# implementations.
#
#   snapshot_offline.sh <SNAPSHOT directory> <height> [work directory]
#
#   XDAGJ_JAR      xdagj-<version>-executable.jar            (required)
#   XDAGD          xdagd binary                              (default: xdagd)
#   JAVA, JAVAC    a JDK 21                                  (default: java, javac)
#   BLOCKDUMP      the blockdump example binary              (default: target/release/examples/blockdump)
#   RANDOMX_LIMIT  RandomX hashes the audit computes at most (default: all)
#
# The SNAPSHOT directory is the one with ADDRESS and BLOCKS in it; the height
# is the first number in a snapshot's file name. Needs about 4 GB of disk and
# 1 GB of memory for a mainnet snapshot, and about ten minutes.
set -euo pipefail
SNAP=${1:?usage: snapshot_offline.sh <SNAPSHOT directory> <height> [work directory]}
HEIGHT=${2:?the main height the snapshot was taken at}
WORK=${3:-./snapshot-test}
: "${XDAGJ_JAR:?set XDAGJ_JAR to the xdagj executable jar}"
: "${XDAGD:=xdagd}" "${JAVA:=java}" "${JAVAC:=javac}"
HERE=$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)
REPO=$(cd "$HERE/../.." && pwd)
: "${BLOCKDUMP:=$REPO/target/release/examples/blockdump}"
mkdir -p "$WORK/classes"
step() { printf '\n== %s\n' "$*"; }
xdagj() { "$JAVA" -Xmx700m -cp "$XDAGJ_JAR:$WORK/classes" "$@" 2> >(grep -v '^SLF4J\|^WARNING' >&2); }

step "1. build the exporter and the block dumper against $(basename "$XDAGJ_JAR")"
"$JAVAC" -proc:none -cp "$XDAGJ_JAR" -d "$WORK/classes" "$REPO/tools/xdagj-exporter/src/XdagjExporter.java" "$HERE/BlockDump.java"

step "2. convert the snapshot: the state an xdagj node has after loading it"
xdagj XdagjExporter --snapshot "$SNAP" --height "$HEIGHT" --network mainnet --out "$WORK/state.xsnp"
"$XDAGD" snapshot info "$WORK/state.xsnp"

step "3. audit it under mainnet's rules"
"$XDAGD" --network mainnet snapshot verify "$WORK/state.xsnp" ${RANDOMX_LIMIT:+--randomx-limit "$RANDOMX_LIMIT"} 2> >(grep -v ' INFO ' >&2)

step "4. import it into a new database, export that, compare"
rm -rf "$WORK/data"
"$XDAGD" --network mainnet --datadir "$WORK/data" snapshot import "$WORK/state.xsnp"
"$XDAGD" --network mainnet --datadir "$WORK/data" snapshot export "$WORK/exported.xsnp"
"$XDAGD" snapshot diff "$WORK/state.xsnp" "$WORK/exported.xsnp"
rm -rf "$WORK/data" "$WORK/exported.xsnp"

step "5. parse every block the snapshot carries with xdagj and with xdagd"
python3 "$HERE/xsnp_blocks.py" "$WORK/state.xsnp" "$WORK/blocks.dat"
if [ -x "$BLOCKDUMP" ]; then
  xdagj BlockDump "$WORK/blocks.dat" "$WORK/blocks-xdagj.txt"
  "$BLOCKDUMP" "$WORK/blocks.dat" "$WORK/blocks-xdagd.txt"
  cmp "$WORK/blocks-xdagj.txt" "$WORK/blocks-xdagd.txt"
  echo "both implementations read every block the same way"
else
  echo "skipped: build it with  cargo build --release -p xdag-chain --example blockdump"
fi

printf '\nall offline checks passed; the converted state is %s\n' "$WORK/state.xsnp"
