/*
 * xdagj → xdagd migration exporter.
 *
 * Reads a STOPPED xdagj node's RocksDB (read-only) and writes
 *   1. an XSNP snapshot that `xdagd snapshot import` loads: every account and
 *      every block the node knows (with the block's data whenever the node has
 *      it), and the main-chain index. Nothing is left out: xdagd must know
 *      exactly the blocks, and which of them were already executed, that the
 *      xdagj nodes it will talk to know;
 *   2. optionally, every raw block the node has, as concatenated 512-byte
 *      records for `xdagd archive import-raw` (history index).
 *
 * It runs on xdagj's own classpath, so block metadata is decoded by xdagj's
 * own Kryo setup (BlockStoreImpl); use the exact jar version the node ran.
 *
 * Build:  javac -cp xdagj-0.8.4-executable.jar -d out src/XdagjExporter.java
 * Run:    java -cp xdagj-0.8.4-executable.jar:out XdagjExporter \
 *              --store ./mainnet/rocksdb/xdagdb --network mainnet \
 *              --out state.xsnp [--blocks blocks.dat]
 *
 * XSNP layout (format 2): see crates/chain/src/snapshot.rs. All integers
 * little-endian except difficulties (32-byte big-endian). Hashes are converted
 * from xdagj's reversed Java representation to wire (C) order.
 */

import io.xdag.core.BlockInfo;
import io.xdag.core.SnapshotInfo;
import io.xdag.core.XAmount;
import io.xdag.core.XUnit;
import io.xdag.core.XdagStats;
import io.xdag.core.XdagTopStatus;
import io.xdag.db.rocksdb.BlockStoreImpl;
import io.xdag.db.rocksdb.KVSource;
import org.apache.commons.lang3.tuple.Pair;
import org.apache.tuweni.bytes.Bytes32;
import org.rocksdb.Options;
import org.rocksdb.RocksDB;
import org.rocksdb.RocksDBException;
import org.rocksdb.RocksIterator;

import java.io.BufferedInputStream;
import java.io.BufferedOutputStream;
import java.io.File;
import java.io.FileInputStream;
import java.io.FileOutputStream;
import java.io.IOException;
import java.io.InputStream;
import java.io.OutputStream;
import java.math.BigInteger;
import java.nio.file.Files;
import java.util.ArrayList;
import java.util.HashSet;
import java.util.List;
import java.util.Map;
import java.util.Set;
import java.util.TreeMap;
import java.util.function.BiConsumer;
import java.util.function.Function;

public final class XdagjExporter {

    // xdagj key prefixes (io.xdag.db.BlockStore / io.xdag.db.AddressStore)
    private static final byte HASH_BLOCK_INFO = (byte) 0x30;
    private static final byte ADDRESS = (byte) 0x30;
    private static final byte EXECUTED_NONCE_NUM = (byte) 0x50;

    // xdagj block flags (identical values in xdagd)
    private static final int BI_MAIN = 0x01;
    private static final int BI_OURS = 0x20;
    private static final int BI_EXTRA = 0x40;

    private static final int FORMAT = 2;
    /** Block status "derive it from the flags" (xdagj has no separate status). */
    private static final int STATUS_UNKNOWN = 0xff;

    public static void main(String[] args) throws Exception {
        String store = null, network = null, out = null, blocksOut = null;
        for (int i = 0; i < args.length; i++) {
            switch (args[i]) {
                case "--store" -> store = args[++i];
                case "--network" -> network = args[++i];
                case "--out" -> out = args[++i];
                case "--blocks" -> blocksOut = args[++i];
                default -> usage("unknown argument " + args[i]);
            }
        }
        if (store == null || network == null || out == null) {
            usage("--store, --network and --out are required");
        }
        int networkId = switch (network) {
            case "mainnet" -> 0;
            case "testnet" -> 1;
            case "devnet" -> 2;
            default -> { usage("network must be mainnet, testnet or devnet"); yield -1; }
        };

        RocksDB.loadLibrary();
        try (Options opts = new Options().setCreateIfMissing(false);
             RocksDB index = open(opts, store, "INDEX");
             RocksDB block = open(opts, store, "BLOCK");
             RocksDB address = open(opts, store, "ADDRESS")) {
            ReadOnlySource indexSrc = new ReadOnlySource("INDEX", index);
            ReadOnlySource blockSrc = new ReadOnlySource("BLOCK", block);
            BlockStoreImpl blockStore = new BlockStoreImpl(indexSrc, new ReadOnlySource("TIME", null), blockSrc,
                    new ReadOnlySource("TXHISTORY", null));
            new XdagjExporter(networkId, blockStore, index, block, address).run(new File(out));
            if (blocksOut != null) {
                dumpRawBlocks(block, new File(blocksOut));
            }
        }
    }

    private static void usage(String msg) {
        System.err.println(msg);
        System.err.println("usage: XdagjExporter --store <xdagj rocksdb/xdagdb dir> --network mainnet|testnet|devnet"
                + " --out <state.xsnp> [--blocks <blocks.dat>]");
        System.exit(2);
    }

    private static RocksDB open(Options opts, String store, String name) throws RocksDBException {
        File dir = new File(store, name);
        if (!dir.isDirectory()) {
            dir = new File(store, name.toLowerCase());
        }
        if (!dir.isDirectory()) {
            throw new IllegalArgumentException("no " + name + " database under " + store);
        }
        return RocksDB.openReadOnly(opts, dir.getPath());
    }

    private final int networkId;
    private final BlockStoreImpl blockStore;
    private final RocksDB index;
    private final RocksDB block;
    private final RocksDB address;

    private XdagjExporter(int networkId, BlockStoreImpl blockStore, RocksDB index, RocksDB block, RocksDB address) {
        this.networkId = networkId;
        this.blockStore = blockStore;
        this.index = index;
        this.block = block;
        this.address = address;
    }

    private void run(File out) throws Exception {
        XdagStats stats = blockStore.getXdagStatus();
        XdagTopStatus topStatus = blockStore.getXdagTopStatus();
        if (stats == null) {
            throw new IllegalStateException("no chain statistics in the INDEX database");
        }
        long nmain = stats.nmain;

        // pass 1: main-chain index from the block metadata itself (xdagj's
        // height keys are not cleaned up after reorganisations)
        TreeMap<Long, byte[]> mains = new TreeMap<>();
        forEachPrefix(index, new byte[]{HASH_BLOCK_INFO}, (k, v) -> {
            BlockInfo bi = info(k);
            if (bi != null && (bi.getFlags() & BI_MAIN) != 0 && bi.getHeight() > 0 && bi.getHeight() <= nmain) {
                mains.put(bi.getHeight(), wireHashlow(bi.getHashlow()));
            }
        });
        if (!mains.containsKey(nmain)) {
            throw new IllegalStateException("main block " + nmain + " (the top of the main chain) is missing");
        }

        // top: xdagj's top block if it was saved, else the last main block
        byte[] top = mains.get(nmain);
        BigInteger topDiff = null;
        if (topStatus != null && topStatus.getTop() != null) {
            BlockInfo ti = blockStore.getBlockInfo(Bytes32.wrap(topStatus.getTop()));
            if (ti != null) {
                top = wireHashlow(ti.getHashlow());
                topDiff = ti.getDifficulty();
            }
        }
        if (topDiff == null) {
            topDiff = infoByWire(top).getDifficulty();
        }

        // pass 2: blocks, into a temporary file (the count comes first)
        File tmp = new File(out.getPath() + ".blocks.tmp");
        long[] counts = new long[3]; // total, with data, metadata only
        try (OutputStream bo = new BufferedOutputStream(new FileOutputStream(tmp), 1 << 20)) {
            Le w = new Le(bo);
            forEachPrefix(index, new byte[]{HASH_BLOCK_INFO}, (k, v) -> {
                BlockInfo bi = info(k);
                if (bi == null) {
                    return;
                }
                try {
                    writeBlock(w, bi, counts);
                } catch (IOException e) {
                    throw new RuntimeException(e);
                }
            });
        }

        // accounts
        Map<String, long[]> accounts = new TreeMap<>();
        forEachPrefix(address, new byte[]{ADDRESS}, (k, v) -> {
            if (k.length == 21 && v != null && v.length == 8) {
                accounts.computeIfAbsent(hex(k, 1, 21), x -> new long[2])[0] = beLong(v);
            }
        });
        forEachPrefix(address, new byte[]{EXECUTED_NONCE_NUM}, (k, v) -> {
            if (k.length == 21 && v != null && v.length == 8) {
                accounts.computeIfAbsent(hex(k, 1, 21), x -> new long[2])[1] = beLong(v);
            }
        });

        try (OutputStream os = new BufferedOutputStream(new FileOutputStream(out), 1 << 20)) {
            Le w = new Le(os);
            w.bytes(new byte[]{'X', 'S', 'N', 'P'});
            w.u8(FORMAT);
            w.u8(networkId);
            w.u64(nmain);
            w.bytes(top);
            w.bytes(be32(topDiff));
            w.u64(0); // RandomX schedule: rebuilt by the importer from the main chain
            w.u64(accounts.size());
            for (Map.Entry<String, long[]> e : accounts.entrySet()) {
                w.bytes(unhex(e.getKey()));
                w.u8(0); // balance in xdagj C units (1 XDAG = 2^32)
                w.u64(e.getValue()[0]);
                w.u64(e.getValue()[1]);
                w.u8(0); // no contract code
            }
            w.u64(counts[0]);
            try (InputStream in = new BufferedInputStream(new FileInputStream(tmp), 1 << 20)) {
                in.transferTo(os);
            }
            w.u64(mains.size());
            for (Map.Entry<Long, byte[]> e : mains.entrySet()) {
                w.u64(e.getKey());
                w.bytes(e.getValue());
            }
            w.u8(0); // no execution-record tables
        }
        Files.delete(tmp.toPath());
        System.out.printf("snapshot: main height %d, %d accounts, %d blocks (%d with data, %d metadata only), %d main-chain entries%n",
                nmain, accounts.size(), counts[0], counts[1], counts[2], mains.size());
        if (mains.size() != nmain) {
            System.out.printf("note: %d main blocks below the node's own snapshot height are not stored by xdagj%n",
                    nmain - mains.size());
        }
    }

    /**
     * One XSNP block entry. Blocks the node has the data of are carried with it;
     * blocks it inherited from its own snapshot are carried as metadata plus
     * the key material xdagj kept for them.
     */
    private void writeBlock(Le w, BlockInfo bi, long[] counts) throws IOException {
        int flags = bi.getFlags() & ~BI_OURS & ~BI_EXTRA;
        byte[] javaHashlow = bi.getHashlow();
        if (javaHashlow == null || javaHashlow.length != 32) {
            throw new IllegalStateException("block info without a hashlow");
        }
        byte[] raw = get(block, javaHashlow);
        if (raw != null && raw.length != 512) {
            raw = null;
        }
        int kind;
        byte[] data;
        byte[] hash;
        if (raw != null) {
            kind = 3;
            data = raw;
            hash = new byte[32]; // recomputed from the data by the importer
            counts[1]++;
        } else {
            SnapshotInfo si = bi.getSnapshotInfo();
            if (si != null && si.getData() != null && si.getType() && si.getData().length == 33) {
                kind = 1;
                data = si.getData();
            } else if (si != null && si.getData() != null && !si.getType() && si.getData().length == 512) {
                kind = 2;
                data = si.getData();
            } else {
                kind = 0;
                data = new byte[0];
            }
            hash = bi.getHash() != null && bi.getHash().length == 32 ? reverse(bi.getHash()) : new byte[32];
            counts[2]++;
        }
        counts[0]++;
        w.bytes(wireHashlow(javaHashlow));
        w.bytes(hash);
        w.u64(bi.getTimestamp());
        w.u8(flags);
        w.u8(STATUS_UNKNOWN);
        w.u64(bi.getHeight());
        w.bytes(be32(bi.getDifficulty()));
        optHash(w, bi.getMaxDiffLink());
        w.u64(nano(bi.getAmount()));
        w.u64(nano(bi.getFee()));
        byte[] remark = bi.getRemark();
        if (remark != null && remark.length == 32) {
            w.u8(1);
            w.bytes(remark);
        } else {
            w.u8(0);
        }
        w.u8(kind);
        w.bytes(data);
        optHash(w, bi.getRef());
    }

    private BlockInfo info(byte[] key) {
        if (key.length != 33) {
            return null;
        }
        byte[] javaHashlow = new byte[32];
        System.arraycopy(key, 1, javaHashlow, 0, 32);
        return blockStore.getBlockInfo(Bytes32.wrap(javaHashlow));
    }

    private BlockInfo infoByWire(byte[] wire) {
        byte[] j = new byte[32];
        byte[] r = reverse(wire);
        System.arraycopy(r, 0, j, 8, 24);
        return blockStore.getBlockInfo(Bytes32.wrap(j));
    }

    private static void dumpRawBlocks(RocksDB block, File out) throws IOException {
        long[] n = new long[1];
        try (OutputStream os = new BufferedOutputStream(new FileOutputStream(out), 1 << 20)) {
            try (RocksIterator it = block.newIterator()) {
                for (it.seekToFirst(); it.isValid(); it.next()) {
                    byte[] v = it.value();
                    if (v != null && v.length == 512) {
                        os.write(v);
                        n[0]++;
                    }
                }
            }
        }
        System.out.printf("raw blocks: %d written to %s%n", n[0], out);
    }

    // ------------------------------------------------------------------ helpers

    /** Java 32-byte hashlow ([0]*8 + reversed hash[0..24]) → 24-byte wire order. */
    private static byte[] wireHashlow(byte[] java) {
        byte[] out = new byte[24];
        int off = java.length - 24;
        for (int i = 0; i < 24; i++) {
            out[i] = java[off + 23 - i];
        }
        return out;
    }

    private static void optHash(Le w, byte[] javaHashlow) throws IOException {
        if (javaHashlow != null && javaHashlow.length >= 24) {
            w.u8(1);
            w.bytes(wireHashlow(javaHashlow));
        } else {
            w.u8(0);
        }
    }

    private static long nano(XAmount a) {
        return a == null ? 0 : a.toDecimal(0, XUnit.NANO_XDAG).longValueExact();
    }

    private static byte[] be32(BigInteger v) {
        byte[] out = new byte[32];
        if (v == null) {
            return out;
        }
        byte[] b = v.toByteArray();
        int n = Math.min(b.length, 32);
        System.arraycopy(b, b.length - n, out, 32 - n, n);
        return out;
    }

    private static byte[] reverse(byte[] b) {
        byte[] r = new byte[b.length];
        for (int i = 0; i < b.length; i++) {
            r[i] = b[b.length - 1 - i];
        }
        return r;
    }

    private static long beLong(byte[] b) {
        long v = 0;
        for (int i = 0; i < 8; i++) {
            v = (v << 8) | (b[i] & 0xff);
        }
        return v;
    }

    private static String hex(byte[] b, int from, int to) {
        StringBuilder sb = new StringBuilder();
        for (int i = from; i < to; i++) {
            sb.append(String.format("%02x", b[i] & 0xff));
        }
        return sb.toString();
    }

    private static byte[] unhex(String s) {
        byte[] out = new byte[s.length() / 2];
        for (int i = 0; i < out.length; i++) {
            out[i] = (byte) Integer.parseInt(s.substring(2 * i, 2 * i + 2), 16);
        }
        return out;
    }

    private static byte[] get(RocksDB db, byte[] key) {
        try {
            return db.get(key);
        } catch (RocksDBException e) {
            throw new RuntimeException(e);
        }
    }

    private static void forEachPrefix(RocksDB db, byte[] prefix, BiConsumer<byte[], byte[]> f) {
        try (RocksIterator it = db.newIterator()) {
            for (it.seek(prefix); it.isValid(); it.next()) {
                byte[] k = it.key();
                if (!startsWith(k, prefix)) {
                    break;
                }
                f.accept(k, it.value());
            }
        }
    }

    private static boolean startsWith(byte[] k, byte[] prefix) {
        if (k.length < prefix.length) {
            return false;
        }
        for (int i = 0; i < prefix.length; i++) {
            if (k[i] != prefix[i]) {
                return false;
            }
        }
        return true;
    }

    /** Little-endian writer. */
    private static final class Le {
        private final OutputStream os;

        Le(OutputStream os) {
            this.os = os;
        }

        void bytes(byte[] b) throws IOException {
            os.write(b);
        }

        void u8(int v) throws IOException {
            os.write(v & 0xff);
        }

        void u64(long v) throws IOException {
            for (int i = 0; i < 8; i++) {
                os.write((int) (v >>> (8 * i)) & 0xff);
            }
        }
    }

    /** Read-only KVSource over a RocksDB handle, for xdagj's BlockStoreImpl. */
    private static final class ReadOnlySource implements KVSource<byte[], byte[]> {
        private final String name;
        private final RocksDB db;

        ReadOnlySource(String name, RocksDB db) {
            this.name = name;
            this.db = db;
        }

        private static UnsupportedOperationException readOnly() {
            return new UnsupportedOperationException("the exporter opens xdagj databases read-only");
        }

        private RocksDB db() {
            if (db == null) {
                throw new UnsupportedOperationException(name + " is not opened by the exporter");
            }
            return db;
        }

        @Override public String getName() { return name; }
        @Override public void setName(String name) { throw readOnly(); }
        @Override public boolean isAlive() { return db != null; }
        @Override public void init() { }
        @Override public void close() { }
        @Override public void reset() { throw readOnly(); }
        @Override public void put(byte[] key, byte[] val) { throw readOnly(); }
        @Override public byte[] get(byte[] key) { return XdagjExporter.get(db(), key); }
        @Override public void delete(byte[] key) { throw readOnly(); }

        @Override
        public Set<byte[]> keys() throws RuntimeException {
            Set<byte[]> out = new HashSet<>();
            try (RocksIterator it = db().newIterator()) {
                for (it.seekToFirst(); it.isValid(); it.next()) {
                    out.add(it.key());
                }
            }
            return out;
        }

        @Override
        public List<byte[]> prefixKeyLookup(byte[] key) {
            List<byte[]> out = new ArrayList<>();
            forEachPrefix(db(), key, (k, v) -> out.add(k));
            return out;
        }

        @Override
        public void fetchPrefix(byte[] key, Function<Pair<byte[], byte[]>, Boolean> func) {
            try (RocksIterator it = db().newIterator()) {
                for (it.seek(key); it.isValid() && startsWith(it.key(), key); it.next()) {
                    if (Boolean.TRUE.equals(func.apply(Pair.of(it.key(), it.value())))) {
                        break;
                    }
                }
            }
        }

        @Override
        public List<byte[]> prefixValueLookup(byte[] key) {
            List<byte[]> out = new ArrayList<>();
            forEachPrefix(db(), key, (k, v) -> out.add(v));
            return out;
        }

        @Override
        public List<Pair<byte[], byte[]>> prefixKeyAndValueLookup(byte[] key) {
            List<Pair<byte[], byte[]>> out = new ArrayList<>();
            forEachPrefix(db(), key, (k, v) -> {
                if (v != null) {
                    out.add(Pair.of(k, v));
                }
            });
            return out;
        }
    }
}
