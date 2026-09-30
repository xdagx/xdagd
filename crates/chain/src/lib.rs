//! XDAG chain engine: DAG insertion, main-chain selection and block execution.
//!
//! The consensus logic follows xdagj 0.8.4 (`BlockchainImpl`) rule by rule so
//! that the existing chain validates identically ("legacy rules"). Deviations
//! are limited to places where xdagj's behaviour depended on node-local or
//! peer-supplied state; each is documented at the code and in
//! `docs/bugs-fixed.md`. From the Nova activation epoch on, the Nova rules
//! apply (exact amounts, batch payloads, EVM, fee-on-failure, anti-spam PoW).

pub mod apply;
pub mod archive;
pub mod builder;
pub mod chain;
pub mod evm_api;
pub mod fees;
pub mod keys;
pub mod mempool;
pub mod overlay;
pub mod pow;
pub mod preverify;
pub mod query;
pub mod records;
pub mod snapshot;
pub mod sums;
pub mod testkit;
pub mod verify;

pub use chain::{Chain, ChainEvent, ChainOptions, Clock, ImportOutcome, ManualClock, Source, SystemClock};
pub use evm_api::{EvmAccount, EvmChanges, EvmEngine, EvmEnv, EvmExecResult, EvmStateAccess, EvmTxInfo};
pub use preverify::{preverify, PreVerified, VerifiedPayload, VerifiedTx};
pub use records::{flags, BlockInfo, BlockState, TxStatus};

#[derive(Debug, thiserror::Error)]
pub enum ChainError {
    #[error("storage: {0}")]
    Storage(#[from] xdag_storage::StorageError),
    #[error("corrupt database: {0}")]
    Corrupt(String),
    #[error("arithmetic overflow")]
    Overflow,
    #[error("invalid: {0}")]
    Invalid(String),
    #[error("{0}")]
    Other(String),
}

pub type Result<T> = std::result::Result<T, ChainError>;
