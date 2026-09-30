import io.xdag.config.RandomXConstants;

/**
 * Starts an unmodified xdagj node after shortening the devnet/testnet RandomX
 * schedule (xdagj keeps these three constants non-final for its own tests),
 * so that a local devnet reaches the RandomX fork within minutes.
 */
public class InteropMain {
    public static void main(String[] args) throws Exception {
        RandomXConstants.RANDOMX_TESTNET_FORK_HEIGHT = Long.getLong("interop.rx.fork", RandomXConstants.RANDOMX_TESTNET_FORK_HEIGHT);
        RandomXConstants.SEEDHASH_EPOCH_TESTNET_BLOCKS = Long.getLong("interop.rx.epoch", RandomXConstants.SEEDHASH_EPOCH_TESTNET_BLOCKS);
        RandomXConstants.SEEDHASH_EPOCH_TESTNET_LAG = Long.getLong("interop.rx.lag", RandomXConstants.SEEDHASH_EPOCH_TESTNET_LAG);
        System.out.printf("interop: RandomX fork height %d, seed epoch %d blocks, lag %d%n", RandomXConstants.RANDOMX_TESTNET_FORK_HEIGHT,
                RandomXConstants.SEEDHASH_EPOCH_TESTNET_BLOCKS, RandomXConstants.SEEDHASH_EPOCH_TESTNET_LAG);
        io.xdag.Bootstrap.main(args);
    }
}
