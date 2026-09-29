//! Boundary between the chain and the EVM implementation.
//!
//! The chain owns state and ordering; an [`EvmEngine`] (provided by the
//! `xdag-evm` crate, backed by revm) decodes and executes Ethereum
//! transactions against an [`EvmStateAccess`] view. Keeping the trait here lets
//! the consensus crate build and test without compiling an EVM.

use xdag_types::Address;

/// Stateless facts about an EVM transaction, established before a batch
/// block is accepted.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct EvmTxInfo {
    pub hash: [u8; 32],
    pub sender: Address,
    pub nonce: u64,
    pub gas_limit: u64,
    /// Upper bound of the price paid per gas (legacy gas price or EIP-1559 max fee), wei.
    pub max_gas_price: u128,
    pub to: Option<Address>,
    pub value: u128,
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct EvmAccount {
    pub balance: u128,
    pub nonce: u64,
    pub code_hash: Option<[u8; 32]>,
}

/// Execution environment derived from the main block being applied.
#[derive(Clone, Debug)]
pub struct EvmEnv {
    pub chain_id: u64,
    pub height: u64,
    /// Seconds.
    pub timestamp: u64,
    pub coinbase: Address,
    pub prevrandao: [u8; 32],
    pub gas_limit: u64,
    pub min_gas_price: u128,
}

/// State diff produced by one transaction.
#[derive(Clone, Debug, Default)]
pub struct EvmChanges {
    /// (address, new account or None if destroyed)
    pub accounts: Vec<(Address, Option<EvmAccount>)>,
    /// new code (hash, bytes)
    pub codes: Vec<([u8; 32], Vec<u8>)>,
    /// (address, slot, value) — zero value means delete
    pub storage: Vec<(Address, [u8; 32], [u8; 32])>,
    /// Accounts whose storage must be wiped (selfdestruct in the creating tx).
    pub cleared_storage: Vec<Address>,
}

#[derive(Clone, Debug)]
pub struct EvmLog {
    pub address: Address,
    pub topics: Vec<[u8; 32]>,
    pub data: Vec<u8>,
}

#[derive(Clone, Debug)]
pub struct EvmExecResult {
    pub success: bool,
    pub gas_used: u64,
    /// Total fee paid (wei).
    pub fee: u128,
    pub logs: Vec<EvmLog>,
    pub output: Vec<u8>,
    pub contract_address: Option<Address>,
    pub changes: EvmChanges,
}

pub trait EvmStateAccess {
    /// `None` if the account does not exist.
    fn account(&mut self, a: &Address) -> Result<Option<EvmAccount>, String>;
    fn code(&mut self, code_hash: &[u8; 32]) -> Result<Vec<u8>, String>;
    fn storage(&mut self, a: &Address, slot: &[u8; 32]) -> Result<[u8; 32], String>;
    /// Hash of the main block at `height` (for BLOCKHASH).
    fn block_hash(&mut self, height: u64) -> Result<[u8; 32], String>;
}

pub trait EvmEngine: Send + Sync {
    /// Decode, check the chain id and recover the sender.
    fn check_tx(&self, raw: &[u8], chain_id: u64) -> Result<EvmTxInfo, String>;

    /// Execute against `state`. `Err` means the transaction is not
    /// executable (bad nonce, insufficient funds for gas, …) and must be
    /// skipped without any state change.
    fn execute(&self, state: &mut dyn EvmStateAccess, env: &EvmEnv, raw: &[u8]) -> Result<EvmExecResult, String>;

    /// Read-only call (eth_call / eth_estimateGas).
    fn call(
        &self,
        state: &mut dyn EvmStateAccess,
        env: &EvmEnv,
        from: Address,
        to: Option<Address>,
        data: Vec<u8>,
        value: u128,
        gas: u64,
    ) -> Result<EvmExecResult, String>;
}
