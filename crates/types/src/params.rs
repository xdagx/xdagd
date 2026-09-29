//! Network parameters.

use crate::amount::{CAmount, Nano};
use crate::field::FieldType;
use crate::time::Epochs;
use serde::{Deserialize, Serialize};
use std::fmt;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Network {
    Mainnet,
    Testnet,
    Devnet,
}

impl Network {
    /// Byte used in the xdagj handshake.
    pub fn id(self) -> u8 {
        match self {
            Network::Mainnet => 0,
            Network::Testnet => 1,
            Network::Devnet => 2,
        }
    }
    pub fn from_id(id: u8) -> Option<Network> {
        match id {
            0 => Some(Network::Mainnet),
            1 => Some(Network::Testnet),
            2 => Some(Network::Devnet),
            _ => None,
        }
    }
    pub fn name(self) -> &'static str {
        match self {
            Network::Mainnet => "mainnet",
            Network::Testnet => "testnet",
            Network::Devnet => "devnet",
        }
    }
}

impl fmt::Display for Network {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.name())
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct RandomXParams {
    /// Main-chain height whose epoch (+lag) switches PoW from sha256d to RandomX.
    pub fork_height: u64,
    pub seed_epoch_blocks: u64,
    pub seed_lag: u64,
}

/// Parameters of the Nova upgrade (smart contracts, batches, exact amounts).
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct NovaParams {
    /// First epoch (xdag time >> epoch_bits) governed by Nova rules.
    pub activation_epoch: u64,
    /// EIP-155 chain id for EVM transactions, also bound into native Nova txs.
    pub chain_id: u64,
    /// Minimum fee of a compact native transfer.
    pub min_native_fee: Nano,
    /// Minimum EVM gas price, in wei (1 XDAG = 10^18 wei).
    pub min_gas_price: u128,
    /// Gas limit of one extension payload.
    pub batch_gas_limit: u64,
    /// Total EVM gas that one main block may execute.
    pub main_gas_limit: u64,
    /// Maximum extension payload size in bytes.
    pub max_payload_bytes: usize,
    /// Maximum number of transactions in one payload.
    pub max_payload_txs: usize,
    /// Minimum sha256d difficulty (log2) required of non-main blocks that are
    /// neither transactions nor main candidates (anti-spam for an open network).
    pub min_link_pow_bits: u32,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct NetworkParams {
    pub network: Network,
    pub network_version: u16,
    pub header_type: u8,
    /// Earliest valid block time.
    pub era: u64,
    pub main_start_amount: CAmount,
    pub apollo_fork_height: u64,
    pub apollo_fork_amount: CAmount,
    /// Rewards halve every 2^halving_log main blocks.
    pub halving_log: u32,
    pub randomx: RandomXParams,
    pub epoch_bits: u32,
    /// Minimum per-output fee of legacy transactions (xdagj MIN_GAS).
    pub min_gas: Nano,
    pub nova: Option<NovaParams>,
    pub default_p2p_port: u16,
    pub default_rpc_port: u16,
    pub default_pool_port: u16,
    pub seeds: Vec<String>,
    /// Community fund address used by the pool reward distribution.
    pub fund_address: String,
}

impl NetworkParams {
    pub fn header_field(&self) -> FieldType {
        FieldType::from_nibble(self.header_type)
    }

    pub fn epochs(&self) -> Epochs {
        Epochs { bits: self.epoch_bits }
    }

    /// Maximum tolerated clock drift for incoming blocks (xdagj: MAIN_CHAIN_PERIOD / 4).
    pub fn max_future_drift(&self) -> u64 {
        self.epochs().period() / 4
    }

    pub fn is_nova_epoch(&self, epoch: u64) -> bool {
        matches!(&self.nova, Some(n) if epoch >= n.activation_epoch)
    }

    pub fn is_nova_time(&self, t: u64) -> bool {
        self.is_nova_epoch(self.epochs().epoch(t))
    }

    pub fn mainnet() -> Self {
        NetworkParams {
            network: Network::Mainnet,
            network_version: 0,
            header_type: FieldType::Head.nibble(),
            era: 0x16940000000,
            main_start_amount: CAmount(1 << 42),
            apollo_fork_height: 1_017_323,
            apollo_fork_amount: CAmount(1 << 39),
            halving_log: 21,
            randomx: RandomXParams { fork_height: 1_540_096, seed_epoch_blocks: 4096, seed_lag: 128 },
            epoch_bits: 16,
            min_gas: Nano::from_milli(100),
            nova: None,
            default_p2p_port: 8001,
            default_rpc_port: 10001,
            default_pool_port: 7001,
            seeds: vec![],
            fund_address: "PKcBtHWDSnAWfZntqWPBLedqBShuKSTzS".into(),
        }
    }

    pub fn testnet() -> Self {
        NetworkParams {
            network: Network::Testnet,
            header_type: FieldType::HeadTest.nibble(),
            era: 0x16900000000,
            apollo_fork_height: 196_250,
            randomx: RandomXParams { fork_height: 4096, seed_epoch_blocks: 2048, seed_lag: 64 },
            default_p2p_port: 18001,
            default_rpc_port: 20001,
            default_pool_port: 17001,
            fund_address: "4duPWMbYUgAifVYkKDCWxLvRRkSByf5gb".into(),
            nova: Some(NovaParams { activation_epoch: u64::MAX, ..NovaParams::defaults(30821) }),
            ..Self::mainnet()
        }
    }

    /// Local development network: Nova active from the start and a short
    /// epoch so that multi-node tests run in seconds.
    pub fn devnet() -> Self {
        NetworkParams {
            network: Network::Devnet,
            header_type: FieldType::HeadTest.nibble(),
            era: 0x16900000000,
            apollo_fork_height: 1000,
            randomx: RandomXParams { fork_height: 4096, seed_epoch_blocks: 2048, seed_lag: 64 },
            default_p2p_port: 28001,
            default_rpc_port: 30001,
            default_pool_port: 27001,
            fund_address: "4duPWMbYUgAifVYkKDCWxLvRRkSByf5gb".into(),
            nova: Some(NovaParams { activation_epoch: 0, min_link_pow_bits: 8, ..NovaParams::defaults(30822) }),
            ..Self::mainnet()
        }
    }

    pub fn for_network(n: Network) -> Self {
        match n {
            Network::Mainnet => Self::mainnet(),
            Network::Testnet => Self::testnet(),
            Network::Devnet => Self::devnet(),
        }
    }
}

impl NovaParams {
    pub fn defaults(chain_id: u64) -> Self {
        NovaParams {
            activation_epoch: u64::MAX,
            chain_id,
            min_native_fee: Nano::from_milli(100),
            min_gas_price: 1_000_000_000, // 1 gwei
            batch_gas_limit: 30_000_000,
            main_gas_limit: 300_000_000,
            max_payload_bytes: 1 << 20,
            max_payload_txs: 8192,
            min_link_pow_bits: 16,
        }
    }
}
