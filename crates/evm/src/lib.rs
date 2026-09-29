//! EVM for XDAG Nova, backed by revm.
//!
//! * Spec: Prague (no blob transactions, no EIP-7702 set-code transactions).
//! * Native token: XDAG with 18 decimals inside the EVM (1 nano-XDAG = 10^9 wei).
//! * Base fee is 0; every transaction must pay at least the network's minimum
//!   gas price, and the whole fee goes to the main block's coinbase address.
//! * One account space: XDAG (hash160) and EVM (keccak) addresses are both
//!   plain 20-byte accounts.

pub mod tx;

use revm::context::result::{EVMError, ExecutionResult, Output};
use revm::context::{BlockEnv, CfgEnv, Context, TxEnv};
use revm::database_interface::DBErrorMarker;
use revm::primitives::hardfork::SpecId;
use revm::primitives::{Address as RAddress, Bytes, TxKind, B256, KECCAK_EMPTY, U256};
use revm::state::{AccountInfo, Bytecode};
use revm::{Database, ExecuteEvm, MainBuilder, MainContext};

use xdag_chain::evm_api::{EvmAccount, EvmChanges, EvmEngine, EvmEnv, EvmExecResult, EvmLog, EvmStateAccess, EvmTxInfo};
use xdag_types::Address;

pub use tx::{decode, EthTx};

#[derive(Debug)]
pub struct DbError(pub String);

impl std::fmt::Display for DbError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}
impl std::error::Error for DbError {}
impl DBErrorMarker for DbError {}

fn ra(a: &Address) -> RAddress {
    RAddress::from(a.0)
}

fn xa(a: &RAddress) -> Address {
    Address(a.0 .0)
}

/// revm `Database` over the chain's state view.
struct StateDb<'a> {
    state: &'a mut dyn EvmStateAccess,
}

impl Database for StateDb<'_> {
    type Error = DbError;

    fn basic(&mut self, address: RAddress) -> Result<Option<AccountInfo>, DbError> {
        let a = self.state.account(&xa(&address)).map_err(DbError)?;
        Ok(a.map(|a| {
            let code_hash = a.code_hash.map(B256::from).unwrap_or(KECCAK_EMPTY);
            // `AccountInfo::default()` carries an empty bytecode; contracts must
            // leave `code` unset so revm loads it through `code_by_hash`.
            let code = if a.code_hash.is_some() { None } else { Some(Bytecode::default()) };
            AccountInfo { balance: U256::from(a.balance), nonce: a.nonce, code_hash, code, ..Default::default() }
        }))
    }

    fn code_by_hash(&mut self, code_hash: B256) -> Result<Bytecode, DbError> {
        if code_hash == KECCAK_EMPTY || code_hash == B256::ZERO {
            return Ok(Bytecode::default());
        }
        let code = self.state.code(&code_hash.0).map_err(DbError)?;
        Ok(Bytecode::new_raw(Bytes::from(code)))
    }

    fn storage(&mut self, address: RAddress, index: U256) -> Result<U256, DbError> {
        let v = self.state.storage(&xa(&address), &index.to_be_bytes::<32>()).map_err(DbError)?;
        Ok(U256::from_be_bytes(v))
    }

    fn block_hash(&mut self, number: u64) -> Result<B256, DbError> {
        Ok(B256::from(self.state.block_hash(number).map_err(DbError)?))
    }
}

#[derive(Clone, Default)]
pub struct RevmEngine;

impl RevmEngine {
    pub fn new() -> Self {
        RevmEngine
    }

    fn block_env(env: &EvmEnv) -> BlockEnv {
        BlockEnv {
            number: U256::from(env.height),
            beneficiary: ra(&env.coinbase),
            timestamp: U256::from(env.timestamp),
            gas_limit: env.gas_limit,
            basefee: 0,
            difficulty: U256::ZERO,
            prevrandao: Some(B256::from(env.prevrandao)),
            ..Default::default()
        }
    }

    fn cfg(env: &EvmEnv) -> CfgEnv {
        let mut cfg = CfgEnv::default();
        cfg.chain_id = env.chain_id;
        cfg.spec = SpecId::PRAGUE;
        cfg
    }

    #[allow(clippy::too_many_arguments)]
    fn run(&self, state: &mut dyn EvmStateAccess, env: &EvmEnv, tx: TxEnv, gas_price: u128, disable_nonce: bool) -> Result<EvmExecResult, String> {
        let mut cfg = Self::cfg(env);
        cfg.disable_nonce_check = disable_nonce;
        let block = Self::block_env(env);
        let db = StateDb { state };
        let mut evm = Context::mainnet().with_db(db).with_cfg(cfg).with_block(block).build_mainnet();
        let out = evm.transact(tx).map_err(|e| match e {
            EVMError::Transaction(t) => format!("invalid transaction: {t}"),
            EVMError::Header(h) => format!("invalid header: {h}"),
            EVMError::Database(d) => format!("database: {d}"),
            other => format!("{other}"),
        })?;
        let (success, gas_used, logs, output, contract) = match out.result {
            ExecutionResult::Success { gas, logs, output, .. } => {
                let (bytes, created) = match output {
                    Output::Call(b) => (b.to_vec(), None),
                    Output::Create(b, a) => (b.to_vec(), a.map(|a| xa(&a))),
                };
                (true, gas.tx_gas_used(), logs, bytes, created)
            }
            ExecutionResult::Revert { gas, logs, output } => (false, gas.tx_gas_used(), logs, output.to_vec(), None),
            ExecutionResult::Halt { gas, logs, .. } => (false, gas.tx_gas_used(), logs, vec![], None),
        };
        let mut changes = EvmChanges::default();
        for (addr, acct) in out.state.iter() {
            if !acct.is_touched() {
                continue;
            }
            let a = xa(addr);
            if acct.is_selfdestructed() {
                changes.accounts.push((a, None));
                changes.cleared_storage.push(a);
                continue;
            }
            if acct.is_empty() && !acct.is_created() && acct.is_loaded_as_not_existing() {
                continue; // EIP-161: touched but still empty and non-existent
            }
            let code_hash = (acct.info.code_hash != KECCAK_EMPTY && acct.info.code_hash != B256::ZERO).then_some(acct.info.code_hash.0);
            if let (Some(h), Some(code)) = (code_hash, acct.info.code.as_ref()) {
                if acct.is_created() {
                    changes.codes.push((h, code.original_bytes().to_vec()));
                }
            }
            let balance: u128 = acct.info.balance.try_into().map_err(|_| "balance exceeds 128 bits".to_string())?;
            changes.accounts.push((a, Some(EvmAccount { balance, nonce: acct.info.nonce, code_hash })));
            if acct.is_created() {
                changes.cleared_storage.push(a);
            }
            for (slot, v) in acct.storage.iter() {
                if v.is_changed() {
                    changes.storage.push((a, slot.to_be_bytes::<32>(), v.present_value.to_be_bytes::<32>()));
                }
            }
        }
        let logs = logs
            .into_iter()
            .map(|l| EvmLog { address: xa(&l.address), topics: l.data.topics().iter().map(|t| t.0).collect(), data: l.data.data.to_vec() })
            .collect();
        Ok(EvmExecResult { success, gas_used, fee: gas_used as u128 * gas_price, logs, output, contract_address: contract, changes })
    }
}

impl EvmEngine for RevmEngine {
    fn check_tx(&self, raw: &[u8], chain_id: u64) -> Result<EvmTxInfo, String> {
        let t = tx::decode(raw)?;
        if t.chain_id != chain_id {
            return Err(format!("wrong chain id {} (expected {chain_id})", t.chain_id));
        }
        let value = t.value_u128().ok_or("value too large")?;
        Ok(EvmTxInfo {
            hash: t.hash,
            sender: t.sender,
            nonce: t.nonce,
            gas_limit: t.gas_limit,
            max_gas_price: t.effective_gas_price(0),
            to: t.to,
            value,
        })
    }

    fn execute(&self, state: &mut dyn EvmStateAccess, env: &EvmEnv, raw: &[u8]) -> Result<EvmExecResult, String> {
        let t = tx::decode(raw)?;
        if t.chain_id != env.chain_id {
            return Err("wrong chain id".into());
        }
        let price = t.effective_gas_price(0);
        if price < env.min_gas_price {
            return Err("gas price below minimum".into());
        }
        let value = t.value_u128().ok_or("value too large")?;
        let tx_env = TxEnv {
            tx_type: t.tx_type,
            caller: ra(&t.sender),
            gas_limit: t.gas_limit,
            gas_price: t.max_fee_per_gas,
            kind: match t.to {
                Some(a) => TxKind::Call(ra(&a)),
                None => TxKind::Create,
            },
            value: U256::from(value),
            data: Bytes::from(t.data.clone()),
            nonce: t.nonce,
            chain_id: Some(t.chain_id),
            access_list: revm::context::transaction::AccessList(
                t.access_list
                    .iter()
                    .map(|i| revm::context::transaction::AccessListItem {
                        address: ra(&i.address),
                        storage_keys: i.keys.iter().map(|k| B256::from(*k)).collect(),
                    })
                    .collect(),
            ),
            gas_priority_fee: t.max_priority_fee_per_gas,
            ..Default::default()
        };
        self.run(state, env, tx_env, price, false)
    }

    fn call(
        &self,
        state: &mut dyn EvmStateAccess,
        env: &EvmEnv,
        from: Address,
        to: Option<Address>,
        data: Vec<u8>,
        value: u128,
        gas: u64,
    ) -> Result<EvmExecResult, String> {
        let nonce = state.account(&from)?.map(|a| a.nonce).unwrap_or(0);
        let tx_env = TxEnv {
            tx_type: 0,
            caller: ra(&from),
            gas_limit: gas,
            gas_price: 0,
            kind: match to {
                Some(a) => TxKind::Call(ra(&a)),
                None => TxKind::Create,
            },
            value: U256::from(value),
            data: Bytes::from(data),
            nonce,
            chain_id: Some(env.chain_id),
            ..Default::default()
        };
        self.run(state, env, tx_env, 0, true)
    }
}

#[cfg(test)]
mod tests;
