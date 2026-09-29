//! Stateless block verification.
//!
//! Everything that depends only on the block bytes (and its payload) is
//! checked here, outside the chain lock, so that signature verification — the
//! dominant CPU cost — runs in parallel across cores.

use rayon::prelude::*;
use std::sync::Arc;
use xdag_types::nova::{decode_payload, payload_root, NativeTransfer, NovaTx};
use xdag_types::{Address, Block, NetworkParams, PublicKey};

use crate::evm_api::{EvmEngine, EvmTxInfo};

#[derive(Clone, Debug)]
pub enum VerifiedTx {
    Native { tx: NativeTransfer, sender: Address, hash: [u8; 32] },
    Evm { raw: Vec<u8>, info: EvmTxInfo },
}

impl VerifiedTx {
    pub fn hash(&self) -> [u8; 32] {
        match self {
            VerifiedTx::Native { hash, .. } => *hash,
            VerifiedTx::Evm { info, .. } => info.hash,
        }
    }
    pub fn sender(&self) -> Address {
        match self {
            VerifiedTx::Native { sender, .. } => *sender,
            VerifiedTx::Evm { info, .. } => info.sender,
        }
    }
}

#[derive(Clone, Debug)]
pub struct VerifiedPayload {
    pub raw: Arc<Vec<u8>>,
    pub txs: Arc<Vec<VerifiedTx>>,
}

#[derive(Clone, Debug, Default)]
pub struct PreVerified {
    /// Keys proven by the block's signatures (xdagj `verifiedKeys`).
    pub keys: Vec<PublicKey>,
    pub payload: Option<VerifiedPayload>,
}

/// Verify one payload transaction.
pub fn verify_payload_tx(tx: &NovaTx, p: &NetworkParams, evm: Option<&dyn EvmEngine>) -> Result<VerifiedTx, String> {
    let nova = p.nova.as_ref().ok_or("nova disabled")?;
    match tx {
        NovaTx::Native(t) => {
            if t.chain_id != nova.chain_id {
                return Err("wrong chain id".into());
            }
            if t.fee < nova.min_native_fee {
                return Err("fee below minimum".into());
            }
            if t.nonce == 0 {
                return Err("nonce must start at 1".into());
            }
            let (_, sender) = t.recover_signer().map_err(|e| e.to_string())?;
            Ok(VerifiedTx::Native { hash: t.tx_hash(), tx: t.clone(), sender })
        }
        NovaTx::Evm(raw) => {
            let evm = evm.ok_or("EVM not available")?;
            let info = evm.check_tx(raw, nova.chain_id)?;
            if info.gas_limit > nova.batch_gas_limit {
                return Err("gas limit above batch limit".into());
            }
            if info.max_gas_price < nova.min_gas_price {
                return Err("gas price below minimum".into());
            }
            Ok(VerifiedTx::Evm { raw: raw.clone(), info })
        }
    }
}

/// Verify a block and (for Nova batch blocks) its payload.
pub fn preverify(block: &Block, payload: Option<Arc<Vec<u8>>>, p: &NetworkParams, evm: Option<&dyn EvmEngine>) -> Result<PreVerified, String> {
    // Every block must carry an output signature: xdagj dereferences it
    // unconditionally (checkMineAndAdd) and rejects the block otherwise.
    if block.outsig.is_none() {
        return Err("block has no output signature".into());
    }
    let keys = block.verified_keys().map_err(|e| e.to_string())?;
    let nova_block = p.is_nova_time(block.time);
    let payload = match (nova_block, block.ext_root) {
        (true, Some(root)) => {
            let nova = p.nova.as_ref().unwrap();
            let raw = payload.ok_or("missing payload")?;
            if raw.len() > nova.max_payload_bytes {
                return Err("payload too large".into());
            }
            if payload_root(&raw) != root {
                return Err("payload root mismatch".into());
            }
            let txs = decode_payload(&raw, nova.max_payload_txs).map_err(|e| e.to_string())?;
            let verified: Result<Vec<VerifiedTx>, String> = txs.par_iter().map(|t| verify_payload_tx(t, p, evm)).collect();
            let verified = verified?;
            let gas: u64 = verified
                .iter()
                .map(|t| match t {
                    VerifiedTx::Evm { info, .. } => info.gas_limit,
                    _ => 0,
                })
                .sum();
            if gas > nova.batch_gas_limit {
                return Err("payload exceeds batch gas limit".into());
            }
            Some(VerifiedPayload { raw, txs: Arc::new(verified) })
        }
        _ => None,
    };
    Ok(PreVerified { keys, payload })
}

/// Pre-verify many blocks in parallel (sync, gossip bursts).
pub fn preverify_many(
    items: Vec<(Arc<Block>, Option<Arc<Vec<u8>>>)>,
    p: &NetworkParams,
    evm: Option<&dyn EvmEngine>,
) -> Vec<(Arc<Block>, Result<PreVerified, String>)> {
    items
        .into_par_iter()
        .map(|(b, pl)| {
            let r = preverify(&b, pl, p, evm);
            (b, r)
        })
        .collect()
}
