use super::*;
use std::collections::HashMap;
use xdag_types::KeyPair;

#[derive(Default)]
struct MemState {
    accounts: HashMap<Address, EvmAccount>,
    code: HashMap<[u8; 32], Vec<u8>>,
    storage: HashMap<(Address, [u8; 32]), [u8; 32]>,
}

impl EvmStateAccess for MemState {
    fn account(&mut self, a: &Address) -> Result<Option<EvmAccount>, String> {
        Ok(self.accounts.get(a).cloned())
    }
    fn code(&mut self, h: &[u8; 32]) -> Result<Vec<u8>, String> {
        Ok(self.code.get(h).cloned().unwrap_or_default())
    }
    fn storage(&mut self, a: &Address, slot: &[u8; 32]) -> Result<[u8; 32], String> {
        Ok(self.storage.get(&(*a, *slot)).copied().unwrap_or([0u8; 32]))
    }
    fn block_hash(&mut self, _h: u64) -> Result<[u8; 32], String> {
        Ok([0u8; 32])
    }
}

impl MemState {
    fn apply(&mut self, c: &EvmChanges) {
        for (h, code) in &c.codes {
            self.code.insert(*h, code.clone());
        }
        for a in &c.cleared_storage {
            self.storage.retain(|(x, _), _| x != a);
        }
        for (a, s, v) in &c.storage {
            if *v == [0u8; 32] {
                self.storage.remove(&(*a, *s));
            } else {
                self.storage.insert((*a, *s), *v);
            }
        }
        for (a, acct) in &c.accounts {
            match acct {
                Some(x) => {
                    self.accounts.insert(*a, x.clone());
                }
                None => {
                    self.accounts.remove(a);
                }
            }
        }
    }
}

fn env() -> EvmEnv {
    EvmEnv {
        chain_id: 30822,
        height: 10,
        timestamp: 1_700_000_000,
        coinbase: Address([0xcb; 20]),
        prevrandao: [1u8; 32],
        gas_limit: 30_000_000,
        min_gas_price: 1_000_000_000,
    }
}

const WEI: u128 = 1_000_000_000_000_000_000;

/// Counter contract: storage[0] += 1 on every call; returns storage[0] on call with data 0x01.
/// init: stores runtime and returns it.
fn counter_initcode() -> Vec<u8> {
    // runtime:
    // 60 01 36 14 60 13 57   PUSH1 1 CALLDATASIZE EQ PUSH1 0x13 JUMPI  (if calldatasize==1 → read)
    // 60 00 54 60 01 01 60 00 55 00   PUSH1 0 SLOAD PUSH1 1 ADD PUSH1 0 SSTORE STOP
    // pad to 0x13: JUMPDEST at 0x13
    // 5b 60 00 54 60 00 52 60 20 60 00 f3   JUMPDEST PUSH1 0 SLOAD PUSH1 0 MSTORE PUSH1 32 PUSH1 0 RETURN
    let mut runtime = vec![0x60, 0x01, 0x36, 0x14, 0x60, 0x13, 0x57, 0x60, 0x00, 0x54, 0x60, 0x01, 0x01, 0x60, 0x00, 0x55, 0x00];
    while runtime.len() < 0x13 {
        runtime.push(0x00);
    }
    runtime.extend_from_slice(&[0x5b, 0x60, 0x00, 0x54, 0x60, 0x00, 0x52, 0x60, 0x20, 0x60, 0x00, 0xf3]);
    // initcode: copy runtime from code offset and return it
    let n = runtime.len() as u8;
    let mut init = vec![0x60, n, 0x80, 0x60, 0x0c, 0x60, 0x00, 0x39, 0x60, 0x00, 0xf3, 0x00];
    // PUSH1 n DUP1 PUSH1 0x0c PUSH1 0 CODECOPY PUSH1 0 RETURN STOP(pad) → runtime starts at 0x0c
    init.extend_from_slice(&runtime);
    init
}

#[test]
fn value_transfer_pays_coinbase() {
    let eng = RevmEngine::new();
    let alice = KeyPair::random();
    let bob = KeyPair::random().evm_address();
    let mut st = MemState::default();
    st.accounts.insert(alice.evm_address(), EvmAccount { balance: 10 * WEI, nonce: 0, code_hash: None });
    let raw = tx::sign_eip1559(&alice, 30822, 0, 1_000_000_000, 2_000_000_000, 21000, Some(bob), WEI, b"");
    let info = eng.check_tx(&raw, 30822).unwrap();
    assert_eq!(info.sender, alice.evm_address());
    let r = eng.execute(&mut st, &env(), &raw).unwrap();
    assert!(r.success);
    assert_eq!(r.gas_used, 21000);
    assert_eq!(r.fee, 21000 * 1_000_000_000);
    st.apply(&r.changes);
    assert_eq!(st.accounts[&bob].balance, WEI);
    assert_eq!(st.accounts[&alice.evm_address()].balance, 9 * WEI - r.fee);
    assert_eq!(st.accounts[&alice.evm_address()].nonce, 1);
    assert_eq!(st.accounts[&Address([0xcb; 20])].balance, r.fee);
    // replay → nonce too low → not executable
    assert!(eng.execute(&mut st, &env(), &raw).is_err());
}

#[test]
fn deploy_and_call_contract() {
    let eng = RevmEngine::new();
    let alice = KeyPair::random();
    let mut st = MemState::default();
    st.accounts.insert(alice.evm_address(), EvmAccount { balance: 10 * WEI, nonce: 0, code_hash: None });
    let deploy = tx::sign_legacy(&alice, 30822, 0, 1_000_000_000, 200_000, None, 0, &counter_initcode());
    let r = eng.execute(&mut st, &env(), &deploy).unwrap();
    assert!(r.success, "deploy failed: {r:?}");
    let contract = r.contract_address.unwrap();
    st.apply(&r.changes);
    assert!(st.accounts[&contract].code_hash.is_some());
    for n in 1..=3u64 {
        let call = tx::sign_eip1559(&alice, 30822, n, 1_000_000_000, 1_000_000_000, 100_000, Some(contract), 0, b"");
        let r = eng.execute(&mut st, &env(), &call).unwrap();
        assert!(r.success);
        st.apply(&r.changes);
    }
    let read = eng.call(&mut st, &env(), alice.evm_address(), Some(contract), vec![1], 0, 100_000).unwrap();
    assert!(read.success);
    assert_eq!(read.output[31], 3);
}

#[test]
fn underpriced_and_foreign_chain_are_rejected() {
    let eng = RevmEngine::new();
    let alice = KeyPair::random();
    let mut st = MemState::default();
    st.accounts.insert(alice.evm_address(), EvmAccount { balance: 10 * WEI, nonce: 0, code_hash: None });
    let cheap = tx::sign_legacy(&alice, 30822, 0, 1, 21000, Some(Address([1; 20])), 1, b"");
    assert!(eng.execute(&mut st, &env(), &cheap).is_err());
    let foreign = tx::sign_legacy(&alice, 1, 0, 1_000_000_000, 21000, Some(Address([1; 20])), 1, b"");
    assert!(eng.check_tx(&foreign, 30822).is_err());
    let broke = KeyPair::random();
    let poor = tx::sign_legacy(&broke, 30822, 0, 1_000_000_000, 21000, Some(Address([1; 20])), 1, b"");
    assert!(eng.execute(&mut st, &env(), &poor).is_err());
}
