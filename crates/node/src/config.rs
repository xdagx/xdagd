//! Node configuration (TOML).

use std::net::{IpAddr, SocketAddr};
use std::path::PathBuf;

use serde::{Deserialize, Serialize};
use xdag_types::{Nano, Network, NetworkParams};

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(default)]
pub struct NodeConfig {
    pub network: Network,
    pub datadir: PathBuf,
    pub node_tag: String,
    /// Page cache of the database, in MiB.
    pub db_cache_mb: usize,
    pub p2p: P2pConfig,
    pub rpc: RpcConfig,
    pub pool: PoolConfig,
    pub mining: MiningConfig,
    pub nova: NovaOverrides,
    pub randomx: RandomXConfig,
    pub wallet: WalletConfig,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct P2pConfig {
    pub listen: SocketAddr,
    pub advertise_port: Option<u16>,
    /// Seed nodes to bootstrap from. There is no whitelist: anyone may connect.
    pub seeds: Vec<String>,
    pub max_inbound: usize,
    pub max_outbound: usize,
    pub max_inbound_per_ip: usize,
    /// Accept private addresses from peer exchange (local devnets).
    pub allow_private: bool,
    /// Optional local deny list.
    pub deny: Vec<IpAddr>,
    /// Peers this node talks to besides its seeds while the network is closed.
    ///
    /// The network is open to everyone once Nova rules are in force. Before
    /// that (a network still running xdagj's rules, which are only safe among
    /// a closed set of nodes) the node talks to its seeds and these addresses
    /// only. A non-empty list keeps the node closed in any case; listing
    /// `0.0.0.0` opens it regardless of the rules (test networks).
    pub allow: Vec<IpAddr>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct RpcConfig {
    pub enabled: bool,
    pub listen: SocketAddr,
    pub cors_origin: Option<String>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct PoolConfig {
    /// WebSocket interface for external pool software (xdagj `pool.ws.port`).
    pub enabled: bool,
    pub listen: SocketAddr,
    /// Pools allowed to connect to *this* node (0.0.0.0 = any). This is the
    /// operator's own node↔pool link, not a network membership rule.
    pub allowed: Vec<IpAddr>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct MiningConfig {
    /// Produce main-block candidates, link blocks and Nova batch blocks.
    pub generate_blocks: bool,
    /// Built-in CPU miner threads (0 = rely on external pools).
    pub threads: usize,
    pub fund_address: Option<String>,
    pub fund_percent: f64,
    pub node_percent: f64,
    /// Seconds between Nova batch blocks when transactions are pending.
    pub batch_interval_ms: u64,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct NovaOverrides {
    pub activation_epoch: Option<u64>,
    pub chain_id: Option<u64>,
    pub min_link_pow_bits: Option<u32>,
    pub min_gas_price: Option<u128>,
    pub min_native_fee: Option<String>,
    /// Devnets only: shorter epochs (xdag ticks per epoch = 2^epoch_bits).
    pub epoch_bits: Option<u32>,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct RandomXConfig {
    /// Fast mode (2 GiB dataset); light mode (256 MiB) otherwise.
    pub full_mem: bool,
    /// Disable RandomX (devnets that never reach the fork height).
    pub disabled: bool,
    /// Seeds kept initialised at once (default 2, 256 MiB each in light mode).
    pub cache_seeds: Option<usize>,
    /// Devnets only: main-block height of the RandomX fork (a multiple of
    /// `seed_epoch_blocks`), blocks between seed changes (a power of two) and
    /// the seed lag, to reach the fork within minutes in tests.
    pub fork_height: Option<u64>,
    pub seed_epoch_blocks: Option<u64>,
    pub seed_lag: Option<u64>,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct WalletConfig {
    /// Path of an xdagj-compatible wallet.data (default: <datadir>/wallet/wallet.data).
    pub path: Option<PathBuf>,
    /// Password for non-interactive use (prefer the XDAG_WALLET_PASSWORD env var).
    pub password: Option<String>,
}

impl P2pConfig {
    /// Whether anyone may connect, given whether Nova rules are in force now
    /// (see [`P2pConfig::allow`]).
    pub fn open_network(&self, nova_active: bool) -> bool {
        if self.allow.iter().any(|ip| ip.is_unspecified()) {
            return true;
        }
        self.allow.is_empty() && nova_active
    }
}

impl Default for NodeConfig {
    fn default() -> Self {
        Self::for_network(Network::Devnet)
    }
}

impl NodeConfig {
    /// Parse a TOML file on top of the defaults of the network it names.
    pub fn from_toml(text: &str) -> anyhow::Result<Self> {
        let user: toml::Value = toml::from_str(text)?;
        let network = match user.get("network").and_then(|v| v.as_str()) {
            Some("mainnet") => Network::Mainnet,
            Some("testnet") => Network::Testnet,
            Some("devnet") | None => Network::Devnet,
            Some(other) => anyhow::bail!("unknown network {other}"),
        };
        let mut base = toml::Value::try_from(Self::for_network(network))?;
        merge(&mut base, user);
        Ok(base.try_into()?)
    }

    pub fn for_network(n: Network) -> Self {
        let p = NetworkParams::for_network(n);
        NodeConfig {
            network: n,
            datadir: PathBuf::from(format!("./data-{}", n.name())),
            node_tag: "xdag-rs".into(),
            db_cache_mb: xdag_storage::DEFAULT_CACHE_BYTES >> 20,
            p2p: P2pConfig {
                listen: format!("0.0.0.0:{}", p.default_p2p_port).parse().unwrap(),
                advertise_port: None,
                seeds: p.seeds.clone(),
                max_inbound: 128,
                max_outbound: 16,
                max_inbound_per_ip: 4,
                allow_private: n == Network::Devnet,
                deny: vec![],
                allow: vec![],
            },
            rpc: RpcConfig { enabled: true, listen: format!("127.0.0.1:{}", p.default_rpc_port).parse().unwrap(), cors_origin: None },
            pool: PoolConfig {
                enabled: false,
                listen: format!("127.0.0.1:{}", p.default_pool_port).parse().unwrap(),
                allowed: vec!["127.0.0.1".parse().unwrap()],
            },
            mining: MiningConfig {
                generate_blocks: false,
                threads: 0,
                fund_address: None,
                fund_percent: 5.0,
                node_percent: 5.0,
                batch_interval_ms: 1000,
            },
            nova: NovaOverrides::default(),
            randomx: RandomXConfig { full_mem: false, disabled: n == Network::Devnet, ..RandomXConfig::default() },
            wallet: WalletConfig::default(),
        }
    }

    pub fn params(&self) -> anyhow::Result<NetworkParams> {
        let mut p = NetworkParams::for_network(self.network);
        if let Some(bits) = self.nova.epoch_bits {
            anyhow::ensure!(self.network == Network::Devnet, "epoch_bits can only be changed on devnets");
            anyhow::ensure!((8..=16).contains(&bits), "epoch_bits must be within 8..=16");
            p.epoch_bits = bits;
        }
        let rx = &self.randomx;
        if rx.fork_height.is_some() || rx.seed_epoch_blocks.is_some() || rx.seed_lag.is_some() {
            anyhow::ensure!(self.network == Network::Devnet, "the RandomX schedule can only be changed on devnets");
            let r = &mut p.randomx;
            r.fork_height = rx.fork_height.unwrap_or(r.fork_height);
            r.seed_epoch_blocks = rx.seed_epoch_blocks.unwrap_or(r.seed_epoch_blocks);
            r.seed_lag = rx.seed_lag.unwrap_or(r.seed_lag);
            anyhow::ensure!(r.seed_epoch_blocks.is_power_of_two(), "randomx.seed_epoch_blocks must be a power of two");
            anyhow::ensure!(
                r.fork_height >= r.seed_epoch_blocks && r.fork_height.is_multiple_of(r.seed_epoch_blocks),
                "randomx.fork_height must be a positive multiple of seed_epoch_blocks"
            );
            anyhow::ensure!(r.seed_lag < r.fork_height, "randomx.seed_lag must be below the fork height");
        }
        let o = &self.nova;
        if o.activation_epoch.is_some()
            || o.chain_id.is_some()
            || o.min_link_pow_bits.is_some()
            || o.min_gas_price.is_some()
            || o.min_native_fee.is_some()
        {
            let mut n = p.nova.clone().unwrap_or_else(|| xdag_types::NovaParams::defaults(o.chain_id.unwrap_or(30820)));
            if let Some(v) = o.activation_epoch {
                n.activation_epoch = v;
            }
            if let Some(v) = o.chain_id {
                n.chain_id = v;
            }
            if let Some(v) = o.min_link_pow_bits {
                n.min_link_pow_bits = v;
            }
            if let Some(v) = o.min_gas_price {
                n.min_gas_price = v;
            }
            if let Some(v) = &o.min_native_fee {
                n.min_native_fee = Nano::parse_xdag(v).map_err(|e| anyhow::anyhow!("min_native_fee: {e}"))?;
            }
            p.nova = Some(n);
        }
        if let Some(f) = &self.mining.fund_address {
            p.fund_address = f.clone();
        }
        Ok(p)
    }

    pub fn db_path(&self) -> PathBuf {
        self.datadir.join("chain.redb")
    }

    pub fn wallet_path(&self) -> PathBuf {
        self.wallet.path.clone().unwrap_or_else(|| self.datadir.join("wallet").join("wallet.data"))
    }

    pub fn node_key_path(&self) -> PathBuf {
        self.datadir.join("node.key")
    }
}

fn merge(base: &mut toml::Value, user: toml::Value) {
    match (base, user) {
        (toml::Value::Table(b), toml::Value::Table(u)) => {
            for (k, v) in u {
                match b.get_mut(&k) {
                    Some(bv) => merge(bv, v),
                    None => {
                        b.insert(k, v);
                    }
                }
            }
        }
        (b, u) => *b = u,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn toml_overrides_network_defaults() {
        let c = NodeConfig::from_toml("network = \"mainnet\"\n[p2p]\nseeds = [\"1.2.3.4:8001\"]\n").unwrap();
        assert_eq!(c.network, Network::Mainnet);
        assert_eq!(c.p2p.listen.port(), 8001);
        assert_eq!(c.rpc.listen.port(), 10001);
        assert_eq!(c.p2p.seeds, vec!["1.2.3.4:8001".to_string()]);
        let text = toml::to_string_pretty(&c).unwrap();
        let again = NodeConfig::from_toml(&text).unwrap();
        assert_eq!(again.p2p.seeds, c.p2p.seeds);
    }

    #[test]
    fn the_network_is_closed_until_nova_rules_are_in_force() {
        let mut p = NodeConfig::for_network(Network::Mainnet).p2p;
        assert!(!p.open_network(false), "xdagj rules: seeds and allow list only");
        assert!(p.open_network(true), "Nova rules: open to everyone");
        p.allow = vec!["10.0.0.7".parse().unwrap()];
        assert!(!p.open_network(true), "an allow list keeps the node private");
        p.allow.push("0.0.0.0".parse().unwrap());
        assert!(p.open_network(false), "explicitly opened (test networks)");
        // and it parses from the configuration file
        let c = NodeConfig::from_toml("network = \"testnet\"\n[p2p]\nallow = [\"10.0.0.7\"]\n").unwrap();
        assert_eq!(c.p2p.allow, vec!["10.0.0.7".parse::<IpAddr>().unwrap()]);
    }

    #[test]
    fn the_randomx_schedule_can_only_be_shortened_on_devnets() {
        let toml = |net: &str, rx: &str| NodeConfig::from_toml(&format!("network = \"{net}\"\n[randomx]\n{rx}\n")).unwrap().params();
        let p = toml("devnet", "fork_height = 16\nseed_epoch_blocks = 8\nseed_lag = 2").unwrap();
        assert_eq!((p.randomx.fork_height, p.randomx.seed_epoch_blocks, p.randomx.seed_lag), (16, 8, 2));
        assert!(toml("mainnet", "fork_height = 16").is_err());
        assert!(toml("devnet", "fork_height = 12\nseed_epoch_blocks = 8").is_err(), "not a multiple of the seed epoch");
        assert!(toml("devnet", "fork_height = 16\nseed_epoch_blocks = 6").is_err(), "not a power of two");
        // untouched defaults are xdagj's
        let d = NodeConfig::for_network(Network::Mainnet).params().unwrap().randomx;
        assert_eq!((d.fork_height, d.seed_epoch_blocks, d.seed_lag), (1_540_096, 4096, 128));
    }
}
