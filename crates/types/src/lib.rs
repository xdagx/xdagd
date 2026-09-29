//! XDAG protocol primitives.
//!
//! Everything consensus-relevant about the byte-level format lives here:
//! blocks, fields, amounts, addresses, signatures and difficulty. The goal is
//! bit-compatibility with xdagj 0.8.4 for existing chain data, plus the new
//! structures introduced by the Nova upgrade.

pub mod address;
pub mod amount;
pub mod base58;
pub mod block;
pub mod crypto;
pub mod difficulty;
pub mod field;
pub mod hash;
pub mod nova;
pub mod params;
pub mod time;
pub mod wire;

pub use address::Address;
pub use amount::{CAmount, Nano};
pub use block::{Block, BlockTemplate, Link, LinkTarget, BLOCK_SIZE};
pub use crypto::{KeyPair, PublicKey, RecSignature, Signature};
pub use difficulty::{Difficulty, U256};
pub use field::FieldType;
pub use hash::{BlockHash, HashLow};
pub use params::{Network, NetworkParams, NovaParams};

#[derive(Debug, thiserror::Error, Clone, PartialEq, Eq)]
pub enum Error {
    #[error("block must be 512 bytes, got {0}")]
    InvalidBlockSize(usize),
    #[error("amount does not fit a signed 64-bit value")]
    AmountOverflow,
    #[error("invalid amount")]
    InvalidAmount,
    #[error("invalid signature")]
    InvalidSignature,
    #[error("block has public keys but no output signature")]
    MissingOutputSignature,
    #[error("invalid public or private key")]
    InvalidKey,
    #[error("invalid address")]
    InvalidAddress,
    #[error("invalid hex")]
    InvalidHex,
    #[error("too many fields for one block")]
    TooManyFields,
    #[error("malformed payload: {0}")]
    Payload(String),
}
