//! `xdagd` — XDAG full node.

mod bench;
mod client;
mod config;
mod node;
mod producer;

use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::sync::atomic::Ordering;
use std::sync::Arc;

use anyhow::{anyhow, Context, Result};
use clap::{Parser, Subcommand};
use xdag_chain::pow::{NoRandomX, PowEngine};
use xdag_chain::{Chain, ChainOptions, SystemClock};
use xdag_storage::Db;
use xdag_types::{KeyPair, Network};

use crate::config::NodeConfig;
use crate::node::{NetBridge, Node, RpcBridge};

#[derive(Parser, Debug)]
#[command(name = "xdagd", version, about = "XDAG full node (Rust)")]
struct Cli {
    /// Configuration file (TOML). Defaults are derived from --network.
    #[arg(long, global = true)]
    config: Option<PathBuf>,
    #[arg(long, global = true, value_parser = parse_network)]
    network: Option<Network>,
    #[arg(long, global = true)]
    datadir: Option<PathBuf>,
    /// P2P listen address.
    #[arg(long, global = true)]
    p2p: Option<SocketAddr>,
    /// JSON-RPC listen address.
    #[arg(long, global = true)]
    rpc: Option<SocketAddr>,
    /// Seed node (repeatable). There is no whitelist; seeds only bootstrap discovery.
    #[arg(long = "seed", global = true)]
    seeds: Vec<String>,
    /// Produce blocks (main candidates, batches, link blocks).
    #[arg(long, global = true)]
    mine: bool,
    /// Built-in miner threads.
    #[arg(long, global = true)]
    threads: Option<usize>,
    #[command(subcommand)]
    cmd: Option<Cmd>,
}

#[derive(Subcommand, Debug)]
enum Cmd {
    /// Run the node (default).
    Run,
    /// Write the effective configuration as TOML.
    Init {
        #[arg(default_value = "xdagd.toml")]
        out: PathBuf,
    },
    /// Wallet management (xdagj wallet.data compatible).
    Wallet {
        #[command(subcommand)]
        cmd: WalletCmd,
    },
    /// Export or import a state snapshot (XSNP format).
    Snapshot {
        #[command(subcommand)]
        cmd: SnapshotCmd,
    },
    /// Import historical blocks (pre-snapshot history) into the archive.
    Archive {
        #[command(subcommand)]
        cmd: ArchiveCmd,
    },
    /// Measure transaction throughput of this build on this machine.
    Bench {
        /// Number of transfers to execute.
        #[arg(long, default_value_t = 20000)]
        txs: usize,
        /// Senders (parallel nonce chains).
        #[arg(long, default_value_t = 200)]
        senders: usize,
        /// Also run a legacy (xdagj-style, one block per tx) comparison.
        #[arg(long)]
        legacy: bool,
    },
    /// Print chain status from the local database.
    Status,
    /// Sign and submit transactions through a node's JSON-RPC.
    Tx {
        #[arg(long, default_value = "http://127.0.0.1:30001")]
        url: String,
        /// Private key (hex) or a file containing it.
        #[arg(long)]
        key: String,
        #[command(subcommand)]
        cmd: TxCmd,
    },
}

#[derive(Subcommand, Debug)]
enum TxCmd {
    /// Compact native transfer (Nova).
    Native {
        to: String,
        amount: String,
        #[arg(long)]
        fee: Option<String>,
        #[arg(long, default_value = "")]
        remark: String,
    },
    /// EVM transaction (EIP-1559). Without --to it deploys `--data` as init code.
    Evm {
        #[arg(long)]
        to: Option<String>,
        /// Value in wei (1 XDAG = 10^18 wei inside the EVM).
        #[arg(long, default_value_t = 0)]
        value: u128,
        #[arg(long, default_value = "")]
        data: String,
        #[arg(long, default_value_t = 1_000_000)]
        gas: u64,
    },
    /// Print the address(es) of the key.
    Address,
}

#[derive(Subcommand, Debug)]
enum WalletCmd {
    /// Create a new HD wallet (prints the mnemonic once).
    Create,
    /// List accounts and balances.
    List,
    /// Derive the next HD account.
    NewAccount,
    /// Restore from a BIP39 mnemonic.
    Restore { mnemonic: String },
}

#[derive(Subcommand, Debug)]
enum SnapshotCmd {
    /// Write everything this node knows to a snapshot file.
    Export { file: PathBuf },
    /// Load a snapshot into a new (empty) data directory.
    Import { file: PathBuf },
    /// Describe a snapshot file without importing it.
    Info { file: PathBuf },
}

#[derive(Subcommand, Debug)]
enum ArchiveCmd {
    /// Raw 512-byte blocks concatenated in one file (C xdag storage *.dat files,
    /// or an export of xdagj's BLOCK column family).
    ImportRaw { files: Vec<PathBuf> },
    /// Show the archived history of an address or block.
    History { target: String },
}

fn parse_network(s: &str) -> Result<Network, String> {
    match s {
        "mainnet" => Ok(Network::Mainnet),
        "testnet" => Ok(Network::Testnet),
        "devnet" => Ok(Network::Devnet),
        _ => Err("expected mainnet, testnet or devnet".into()),
    }
}

fn load_config(cli: &Cli) -> Result<NodeConfig> {
    let mut cfg = match &cli.config {
        Some(p) => {
            let s = std::fs::read_to_string(p).with_context(|| format!("reading {}", p.display()))?;
            NodeConfig::from_toml(&s).with_context(|| format!("parsing {}", p.display()))?
        }
        None => NodeConfig::for_network(cli.network.unwrap_or(Network::Devnet)),
    };
    if let Some(n) = cli.network {
        if cli.config.is_some() && n != cfg.network {
            return Err(anyhow!("--network conflicts with the configuration file"));
        }
    }
    if let Some(d) = &cli.datadir {
        cfg.datadir = d.clone();
    }
    if let Some(a) = cli.p2p {
        cfg.p2p.listen = a;
    }
    if let Some(a) = cli.rpc {
        cfg.rpc.listen = a;
    }
    cfg.p2p.seeds.extend(cli.seeds.iter().cloned());
    if cli.mine {
        cfg.mining.generate_blocks = true;
    }
    if let Some(t) = cli.threads {
        cfg.mining.threads = t;
    }
    Ok(cfg)
}

fn wallet_password(cfg: &NodeConfig) -> Option<String> {
    std::env::var("XDAG_WALLET_PASSWORD").ok().or_else(|| cfg.wallet.password.clone())
}

fn load_node_key(cfg: &NodeConfig) -> Result<(KeyPair, Option<xdag_wallet::Wallet>)> {
    let wp = cfg.wallet_path();
    if xdag_wallet::Wallet::exists(&wp) {
        let pw = wallet_password(cfg).ok_or_else(|| anyhow!("wallet {} exists: set XDAG_WALLET_PASSWORD", wp.display()))?;
        let w = xdag_wallet::Wallet::unlock(&wp, &pw).map_err(|e| anyhow!("unlock wallet: {e}"))?;
        let k = w.default_key().cloned().ok_or_else(|| anyhow!("wallet has no accounts"))?;
        return Ok((k, Some(w)));
    }
    let kp = cfg.node_key_path();
    if let Ok(s) = std::fs::read_to_string(&kp) {
        let b = hex::decode(s.trim()).context("node.key")?;
        return Ok((KeyPair::from_secret_bytes(&b).map_err(|e| anyhow!("node.key: {e}"))?, None));
    }
    std::fs::create_dir_all(&cfg.datadir)?;
    let k = KeyPair::random();
    std::fs::write(&kp, hex::encode(k.secret_bytes()))?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&kp, std::fs::Permissions::from_mode(0o600))?;
    }
    Ok((k, None))
}

fn open_db(cfg: &NodeConfig) -> Result<Db> {
    std::fs::create_dir_all(&cfg.datadir)?;
    Db::open(&cfg.db_path(), &[]).map_err(|e| anyhow!("open database: {e}"))
}

fn build_node(cfg: NodeConfig) -> Result<Arc<Node>> {
    let params = cfg.params()?;
    let db = open_db(&cfg)?;
    let (key, wallet) = load_node_key(&cfg)?;
    let rx_engine = if cfg.randomx.disabled { None } else { Some(Arc::new(xdag_randomx::RandomXEngine::new(cfg.randomx.full_mem))) };
    let pow: Arc<dyn PowEngine> = match &rx_engine {
        Some(e) => e.clone(),
        None => Arc::new(NoRandomX),
    };
    let evm: Option<Arc<dyn xdag_chain::EvmEngine>> = if params.nova.is_some() { Some(Arc::new(xdag_evm::RevmEngine::new())) } else { None };
    let mut our_keys = vec![*key.public()];
    if let Some(w) = &wallet {
        our_keys.extend(w.accounts.iter().map(|k| *k.public()));
    }
    let opts = ChainOptions { our_keys, ..ChainOptions::default() };
    let chain = Chain::open(db.clone(), params.clone(), opts, pow.clone(), evm.clone(), Arc::new(SystemClock))
        .map_err(|e| anyhow!("open chain: {e} (database: {})", cfg.db_path().display()))?;
    Ok(Node::new(cfg, params, db, chain, key, wallet, evm, pow, rx_engine))
}

async fn run(cfg: NodeConfig) -> Result<()> {
    let node = build_node(cfg.clone())?;
    let params = node.params.clone();
    tracing::info!(
        network = %params.network,
        address = %node.key.address(),
        nova = ?params.nova.as_ref().map(|n| (n.activation_epoch, n.chain_id)),
        "starting xdagd {}",
        env!("CARGO_PKG_VERSION")
    );
    {
        let n = node.clone();
        std::thread::Builder::new().name("import".into()).spawn(move || n.import_loop())?;
    }
    let net_cfg = xdag_net::NetConfig {
        network: params.network.id(),
        network_version: params.network_version as i16,
        listen: cfg.p2p.listen,
        advertise_port: cfg.p2p.advertise_port,
        seeds: cfg.p2p.seeds.clone(),
        node_tag: cfg.node_tag.clone(),
        generate_block: cfg.mining.generate_blocks,
        max_inbound: cfg.p2p.max_inbound,
        max_outbound: cfg.p2p.max_outbound,
        max_inbound_per_ip: cfg.p2p.max_inbound_per_ip,
        allow_private: cfg.p2p.allow_private,
        deny: cfg.p2p.deny.clone(),
        allow: cfg.p2p.allow.iter().copied().filter(|ip| !ip.is_unspecified()).collect(),
        peers_file: Some(cfg.datadir.join("peers.json")),
        ..xdag_net::NetConfig::default()
    };
    let net = xdag_net::start(net_cfg, xdag_net::Identity::new(node.key.clone()), Arc::new(NetBridge(node.clone()))).await?;
    let _ = node.net.set(net.clone());

    if cfg.rpc.enabled {
        let b: Arc<dyn xdag_rpc::Backend> = Arc::new(RpcBridge(node.clone()));
        let addr = cfg.rpc.listen;
        let cors = cfg.rpc.cors_origin.clone();
        tokio::spawn(async move {
            if let Err(e) = xdag_rpc::serve(addr, b, cors).await {
                tracing::error!("RPC server failed: {e}");
            }
        });
    }

    let rx_fn: Option<xdag_pool::RandomXFn> = node.rx_engine.clone().map(|e| {
        let f: xdag_pool::RandomXFn = Arc::new(move |k: &[u8; 32], input: &[u8]| e.hash(k, input));
        f
    });
    let coord = xdag_pool::Coordinator::new(rx_fn);
    if cfg.pool.enabled {
        let c = coord.clone();
        let (addr, allowed) = (cfg.pool.listen, cfg.pool.allowed.clone());
        tokio::spawn(async move {
            if let Err(e) = xdag_pool::server::serve(addr, c, allowed).await {
                tracing::error!("pool server failed: {e}");
            }
        });
    }
    let miner = if cfg.mining.generate_blocks && cfg.mining.threads > 0 {
        Some(xdag_pool::miner::Miner::start(coord.clone(), cfg.mining.threads, node.key.address()))
    } else {
        None
    };
    {
        let (n, c) = (node.clone(), coord.clone());
        std::thread::Builder::new().name("producer".into()).spawn(move || producer::run(n, c))?;
    }

    tokio::signal::ctrl_c().await?;
    tracing::info!("shutting down");
    node.shutdown.store(true, Ordering::SeqCst);
    net.shutdown();
    if let Some(m) = miner {
        m.stop();
    }
    node.chain.lock().commit(true).map_err(|e| anyhow!("final commit: {e}"))?;
    Ok(())
}

fn print_status(cfg: &NodeConfig) -> Result<()> {
    let db = open_db(cfg)?;
    let meta = xdag_chain::query::chain_meta(&db)?;
    println!("network:    {}", cfg.network);
    println!("main height: {}", meta.nmain);
    println!("blocks:     {}", meta.nblocks);
    println!("top:        {:?}", meta.top.map(|t| t.to_legacy_address()));
    println!("difficulty: {:#x}", meta.top_diff);
    println!("schema:     {:?}", db.schema_version()?);
    Ok(())
}

fn wallet_cmd(cfg: &NodeConfig, cmd: WalletCmd) -> Result<()> {
    let path = cfg.wallet_path();
    let pw = || wallet_password(cfg).ok_or_else(|| anyhow!("set XDAG_WALLET_PASSWORD"));
    match cmd {
        WalletCmd::Create => {
            if xdag_wallet::Wallet::exists(&path) {
                return Err(anyhow!("{} already exists", path.display()));
            }
            let w = xdag_wallet::Wallet::create(&path, &pw()?).map_err(|e| anyhow!("{e}"))?;
            w.flush().map_err(|e| anyhow!("{e}"))?;
            println!("mnemonic (write it down, it is not shown again):\n  {}", w.mnemonic);
            println!("address: {}", w.accounts[0].address());
        }
        WalletCmd::Restore { mnemonic } => {
            let w = xdag_wallet::Wallet::from_mnemonic(&path, &pw()?, &mnemonic).map_err(|e| anyhow!("{e}"))?;
            w.flush().map_err(|e| anyhow!("{e}"))?;
            println!("restored: {}", w.accounts[0].address());
        }
        WalletCmd::NewAccount => {
            let mut w = xdag_wallet::Wallet::unlock(&path, &pw()?).map_err(|e| anyhow!("{e}"))?;
            let k = w.add_hd_account().map_err(|e| anyhow!("{e}"))?;
            w.flush().map_err(|e| anyhow!("{e}"))?;
            println!("{}", k.address());
        }
        WalletCmd::List => {
            let w = xdag_wallet::Wallet::unlock(&path, &pw()?).map_err(|e| anyhow!("{e}"))?;
            let db = open_db(cfg).ok();
            for k in &w.accounts {
                let bal = db.as_ref().and_then(|d| xdag_chain::query::balance(d, &k.address()).ok()).unwrap_or_default();
                println!("{}  {}  (evm {})", k.address(), bal, k.evm_address().to_hex());
            }
        }
    }
    Ok(())
}

fn snapshot_cmd(cfg: &NodeConfig, cmd: SnapshotCmd) -> Result<()> {
    if let SnapshotCmd::Info { file } = &cmd {
        return snapshot_info(file);
    }
    let node = build_node(cfg.clone())?;
    let mut chain = node.chain.lock();
    match cmd {
        SnapshotCmd::Info { .. } => unreachable!("handled above"),
        SnapshotCmd::Export { file } => {
            let f = std::io::BufWriter::new(std::fs::File::create(&file)?);
            xdag_chain::snapshot::export(&mut chain, f).map_err(|e| anyhow!("{e}"))?;
            println!("snapshot written to {}", file.display());
        }
        SnapshotCmd::Import { file } => {
            let mut f = std::io::BufReader::new(std::fs::File::open(&file)?);
            let h = xdag_chain::snapshot::import(&mut chain, &mut f).map_err(|e| {
                anyhow!(
                    "{e}\nif the import had already started, the database is incomplete and the node will refuse it: delete {} and import again",
                    cfg.db_path().display()
                )
            })?;
            println!("snapshot loaded at height {}", h.nmain);
        }
    }
    Ok(())
}

fn snapshot_info(file: &std::path::Path) -> Result<()> {
    let f = std::io::BufReader::new(std::fs::File::open(file)?);
    let s = xdag_chain::snapshot::describe(f).map_err(|e| anyhow!("{e}"))?;
    let xdag = |nano: u128| format!("{}.{:09}", nano / 1_000_000_000, nano % 1_000_000_000);
    println!("format:            XSNP {}", xdag_chain::snapshot::VERSION);
    println!("network:           {}", Network::from_id(s.header.network).map(|n| n.name()).unwrap_or("unknown"));
    println!("main height:       {}", s.header.nmain);
    println!("top:               {}  (difficulty {})", s.header.top.to_legacy_address(), xdag_types::difficulty::to_quantity_hex(s.header.top_diff));
    println!("randomx schedule:  {}", if s.header.rx.is_some() { "included" } else { "derived on import" });
    println!("accounts:          {}", s.accounts);
    println!("  digest:          {}{}", hex::encode(s.accounts_digest), if s.accounts_sorted { "" } else { "  (accounts not in address order)" });
    println!("  balances:        {} XDAG", xdag(s.legacy_nano + s.wei / 1_000_000_000));
    println!("blocks:            {}  ({} with data, {} metadata only)", s.blocks, s.blocks_with_data, s.blocks - s.blocks_with_data);
    println!("  balances:        {}{} XDAG", if s.block_nano < 0 { "-" } else { "" }, xdag(s.block_nano.unsigned_abs()));
    println!("main-chain index:  {} entries", s.mains);
    let records: Vec<String> = s.records.iter().map(|(t, n)| format!("{} {n}", t.name())).collect();
    println!("record tables:     {}", if records.is_empty() { "none".to_string() } else { records.join(", ") });
    Ok(())
}

fn archive_cmd(cfg: &NodeConfig, cmd: ArchiveCmd) -> Result<()> {
    let db = open_db(cfg)?;
    match cmd {
        ArchiveCmd::ImportRaw { files } => {
            let mut total = 0usize;
            for f in files {
                let n = xdag_chain::archive::import_raw_file(&db, Path::new(&f)).map_err(|e| anyhow!("{e}"))?;
                println!("{}: {n} blocks", f.display());
                total += n;
            }
            println!("archived {total} blocks");
        }
        ArchiveCmd::History { target } => {
            for e in xdag_chain::archive::history(&db, &target, 1000).map_err(|e| anyhow!("{e}"))? {
                println!("{e}");
            }
        }
    }
    Ok(())
}

fn main() -> Result<()> {
    let cli = Cli::parse();
    let cfg = load_config(&cli)?;
    tracing_subscriber::fmt().with_env_filter(tracing_subscriber::EnvFilter::try_from_default_env().unwrap_or_else(|_| "info".into())).init();
    match cli.cmd.unwrap_or(Cmd::Run) {
        Cmd::Run => {
            let rt = tokio::runtime::Builder::new_multi_thread().enable_all().build()?;
            rt.block_on(run(cfg))
        }
        Cmd::Init { out } => {
            std::fs::write(&out, toml::to_string_pretty(&cfg)?)?;
            println!("wrote {}", out.display());
            Ok(())
        }
        Cmd::Wallet { cmd } => wallet_cmd(&cfg, cmd),
        Cmd::Snapshot { cmd } => snapshot_cmd(&cfg, cmd),
        Cmd::Archive { cmd } => archive_cmd(&cfg, cmd),
        Cmd::Bench { txs, senders, legacy } => bench::run(txs, senders, legacy),
        Cmd::Status => print_status(&cfg),
        Cmd::Tx { url, key, cmd } => {
            let k = client::load_key(&key)?;
            match cmd {
                TxCmd::Native { to, amount, fee, remark } => {
                    println!("{}", client::send_native(&url, &k, &to, &amount, fee.as_deref(), &remark)?)
                }
                TxCmd::Evm { to, value, data, gas } => println!("{}", client::send_evm(&url, &k, to.as_deref(), value, &data, gas)?),
                TxCmd::Address => println!("xdag {}\nevm  {}", k.address(), k.evm_address().to_hex()),
            }
            Ok(())
        }
    }
}
