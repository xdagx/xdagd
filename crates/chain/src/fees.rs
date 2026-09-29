//! Fee and reward rules (xdagj 0.8.4 semantics).

use xdag_types::{Block, CAmount, Nano, NetworkParams};

use crate::ChainError;

/// xdagj `getTxFee`: 0 for non-transactions; otherwise the header fee word
/// (signed nano; negative means 0) plus `MIN_GAS` per output field.
/// Arithmetic overflow made xdagj throw (block rejected) → `Err`.
pub fn tx_fee(block: &Block, p: &NetworkParams) -> Result<Nano, ChainError> {
    if !block.is_tx() {
        return Ok(Nano::ZERO);
    }
    let per_output = p.min_gas.0.checked_mul(block.outputs.len() as u64).ok_or(ChainError::Overflow)?;
    let f = block.header_fee;
    if f == 0 {
        Ok(Nano(per_output))
    } else if f < 0 {
        Ok(Nano::ZERO)
    } else {
        (f as u64).checked_add(per_output).map(Nano).ok_or(ChainError::Overflow)
    }
}

/// xdagj `outPutNum`: number of output fields of a transaction.
pub fn output_count(block: &Block) -> Option<usize> {
    if block.is_tx() {
        Some(block.outputs.len())
    } else {
        None
    }
}

/// xdagj `outPutLimit`: the fee charged per output, `max(MIN_GAS, fee / n)`.
/// A transaction without outputs makes xdagj divide by zero → `Err`.
pub fn output_limit(block: &Block, p: &NetworkParams) -> Result<Nano, ChainError> {
    let Some(n) = output_count(block) else { return Ok(Nano::ZERO) };
    let fee = tx_fee(block, p)?;
    if n == 0 {
        return Err(ChainError::Invalid("division by zero in outPutLimit".into()));
    }
    let per = fee.0 / n as u64;
    Ok(if p.min_gas.0 > per { p.min_gas } else { Nano(per) })
}

/// Reward of main block number `nmain` (xdagj `getReward`).
pub fn reward(nmain: u64, p: &NetworkParams) -> Nano {
    let start = if nmain >= p.apollo_fork_height { p.apollo_fork_amount } else { p.main_start_amount };
    // xdagj: start.toXAmount() then >> (nmain >> 21), then ofXAmount.
    let c = start.0 >> (nmain >> p.halving_log).min(63);
    CAmount(c).to_nano_legacy().unwrap_or(Nano::ZERO)
}

/// Total supply after `nmain` main blocks (xdagj `getSupply`).
pub fn supply(nmain: u64, p: &NetworkParams) -> Nano {
    let start = if nmain >= p.apollo_fork_height { p.apollo_fork_amount } else { p.main_start_amount };
    let mut amount = start.0 as u128;
    let period = 1u128 << p.halving_log;
    let mut res: u128 = 0;
    let mut n = nmain as u128;
    while (n >> p.halving_log) > 0 {
        res = res.wrapping_add(period * amount);
        n -= period;
        amount >>= 1;
    }
    res = res.wrapping_add(n * amount);
    if nmain >= p.apollo_fork_height {
        let diff = (p.main_start_amount.0 - p.apollo_fork_amount.0) as u128;
        res = res.wrapping_add((p.apollo_fork_height as u128 - 1) * diff);
    }
    CAmount(res as u64).to_nano_legacy().unwrap_or(Nano::ZERO)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn mainnet_rewards() {
        let p = NetworkParams::mainnet();
        assert_eq!(reward(1, &p), Nano::from_xdag(1024));
        assert_eq!(reward(1_017_322, &p), Nano::from_xdag(1024));
        assert_eq!(reward(1_017_323, &p), Nano::from_xdag(128));
        assert_eq!(reward(2_097_152, &p), Nano::from_xdag(64));
        assert_eq!(reward(4_194_304, &p), Nano::from_xdag(32));
    }
}
