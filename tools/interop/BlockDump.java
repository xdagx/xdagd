/*
 * Differential test of block parsing: prints, for every 512-byte block of a
 * file, what xdagj's own classes make of it. `cargo run --release -p xdag-chain
 * --example blockdump` prints the same lines from xdagd's parser; the two
 * outputs must be identical.
 *
 * Build:  javac -proc:none -cp xdagj-0.8.4-executable.jar -d out BlockDump.java
 * Run:    java -cp xdagj-0.8.4-executable.jar:out BlockDump blocks.dat xdagj.txt
 */

import io.xdag.core.Address;
import io.xdag.core.Block;
import io.xdag.core.XUnit;
import io.xdag.core.XdagBlock;
import io.xdag.crypto.keys.PublicKey;
import io.xdag.utils.BasicUtils;
import org.apache.tuweni.bytes.Bytes;
import org.apache.tuweni.bytes.Bytes32;

import java.io.BufferedInputStream;
import java.io.BufferedOutputStream;
import java.io.DataInputStream;
import java.io.EOFException;
import java.io.FileInputStream;
import java.io.FileOutputStream;
import java.io.PrintStream;
import java.util.List;

public final class BlockDump {

    public static void main(String[] args) throws Exception {
        if (args.length != 2) {
            System.err.println("usage: BlockDump <blocks.dat> <out.txt>");
            System.exit(2);
        }
        long n = 0;
        try (DataInputStream in = new DataInputStream(new BufferedInputStream(new FileInputStream(args[0]), 1 << 20));
             PrintStream out = new PrintStream(new BufferedOutputStream(new FileOutputStream(args[1]), 1 << 20))) {
            byte[] raw = new byte[512];
            while (true) {
                try {
                    in.readFully(raw);
                } catch (EOFException e) {
                    break;
                }
                out.println(n++ + " " + line(raw.clone()));
            }
        }
        System.out.println(n + " blocks");
    }

    private static String line(byte[] raw) {
        Block b;
        try {
            b = new Block(new XdagBlock(raw));
        } catch (Throwable e) {
            return "unparseable";
        }
        StringBuilder sb = new StringBuilder();
        sb.append("hash=").append(hex(reverse(b.getHash().toArray())));
        sb.append(" time=").append(Long.toHexString(b.getTimestamp()));
        sb.append(" type=").append(Long.toHexString(b.getType()));
        sb.append(" fee=").append(b.getInfo().getFee().toDecimal(0, XUnit.NANO_XDAG).toPlainString());
        sb.append(" in=").append(links(b.getInputs()));
        sb.append(" out=").append(links(b.getOutputs()));
        sb.append(" txnonce=").append(b.getTxNonceField() == null ? "-" : b.getTxNonceField().getTransactionNonce().toBigInteger().toString());
        sb.append(" remark=").append(b.getInfo().getRemark() == null ? "-" : hex(b.getInfo().getRemark()));
        sb.append(" keys=").append(keys(b.getPubKeys()));
        sb.append(" insigs=").append(b.getInsigs().values());
        sb.append(" outsig=").append(b.getOutsig() == null ? "-" : hex(b.getOutsig().getRBytes().toArray()) + hex(b.getOutsig().getSBytes().toArray()));
        sb.append(" nonce=").append(b.getNonce() == null ? "-" : hex(b.getNonce().toArray()));
        sb.append(" outsigindex=").append(b.getOutsigIndex());
        String verified;
        try {
            verified = keys(b.verifiedKeys());
        } catch (Throwable e) {
            verified = "error";
        }
        sb.append(" verified=").append(verified);
        sb.append(" diff=").append(BasicUtils.getDiffByHash(Bytes32.wrap(b.getHash().toArray())).toString(16));
        return sb.toString();
    }

    private static String links(List<Address> links) {
        StringBuilder sb = new StringBuilder("[");
        for (Address a : links) {
            if (sb.length() > 1) {
                sb.append(',');
            }
            sb.append(a.getType().asByte()).append(':');
            if (a.getIsAddress()) {
                sb.append(hex(BasicUtils.hash2byte(a.getAddress()).toArray()));
            } else {
                // Java 32-byte hashlow ([0]*8 + reversed hash[0..24]) → wire order
                byte[] j = a.getAddress().toArray();
                byte[] w = new byte[24];
                for (int i = 0; i < 24; i++) {
                    w[i] = j[31 - i];
                }
                sb.append(hex(w));
            }
            sb.append(':').append(a.getAmount().toDecimal(0, XUnit.NANO_XDAG).toPlainString());
        }
        return sb.append(']').toString();
    }

    private static String keys(List<PublicKey> keys) {
        StringBuilder sb = new StringBuilder("[");
        for (PublicKey k : keys) {
            if (sb.length() > 1) {
                sb.append(',');
            }
            sb.append(hex(k.toBytes().toArray()));
        }
        return sb.append(']').toString();
    }

    private static byte[] reverse(byte[] b) {
        byte[] r = new byte[b.length];
        for (int i = 0; i < b.length; i++) {
            r[i] = b[b.length - 1 - i];
        }
        return r;
    }

    private static String hex(byte[] b) {
        return Bytes.wrap(b).toUnprefixedHexString();
    }
}
