//! Deterministic single-node DAG simulator for tests (used by this crate's
//! integration tests and by downstream crates).

use std::sync::Arc;

use crate::pow::{NoRandomX, PowEngine};
use crate::records::AccountRecord;
use crate::{preverify, Chain, ChainOptions, EvmEngine, ImportOutcome, ManualClock, Source};
use xdag_storage::{Db, TempDirGuard};
use xdag_types::block::BlockTemplate;
use xdag_types::{Address, Block, CAmount, FieldType, HashLow, KeyPair, LinkTarget, Nano, NetworkParams};

pub struct Sim {
    pub chain: Chain,
    pub clock: Arc<ManualClock>,
    pub params: NetworkParams,
    pub miner: KeyPair,
    pub epoch: u64,
    pub last_main: Option<HashLow>,
    pub evm: Option<Arc<dyn EvmEngine>>,
    _guard: TempDirGuard,
}

pub fn legacy_params() -> NetworkParams {
    let mut p = NetworkParams::devnet();
    p.nova = None;
    p
}

pub fn nova_params() -> NetworkParams {
    let mut p = NetworkParams::devnet();
    if let Some(n) = p.nova.as_mut() {
        n.min_link_pow_bits = 4;
        n.min_native_fee = Nano::from_milli(1);
    }
    p
}

impl Sim {
    pub fn new(params: NetworkParams) -> Sim {
        let (db, guard) = Db::open_temporary().unwrap();
        Self::with_db(params, db, guard)
    }

    pub fn with_db(params: NetworkParams, db: Db, guard: TempDirGuard) -> Sim {
        Self::build(params, db, guard, None, Arc::new(NoRandomX))
    }

    pub fn with_evm(params: NetworkParams, evm: Arc<dyn EvmEngine>) -> Sim {
        let (db, guard) = Db::open_temporary().unwrap();
        Self::build(params, db, guard, Some(evm), Arc::new(NoRandomX))
    }

    /// A node with a RandomX implementation (tests use a stand-in).
    pub fn with_pow(params: NetworkParams, pow: Arc<dyn PowEngine>) -> Sim {
        let (db, guard) = Db::open_temporary().unwrap();
        Self::build(params, db, guard, None, pow)
    }

    fn build(params: NetworkParams, db: Db, guard: TempDirGuard, evm: Option<Arc<dyn EvmEngine>>, pow: Arc<dyn PowEngine>) -> Sim {
        let epochs = params.epochs();
        let start_epoch = epochs.epoch(params.era) + 10;
        let clock = ManualClock::new(epochs.start_of_epoch(start_epoch));
        let opts = ChainOptions { check_future: false, ..ChainOptions::default() };
        let chain = Chain::open(db, params.clone(), opts, pow, evm.clone(), clock.clone()).unwrap();
        Sim { chain, clock, params, miner: KeyPair::random(), epoch: start_epoch, last_main: None, evm, _guard: guard }
    }

    pub fn epochs(&self) -> xdag_types::time::Epochs {
        self.params.epochs()
    }

    /// Time inside the current epoch.
    pub fn t(&self, offset: u64) -> u64 {
        self.epochs().start_of_epoch(self.epoch) + offset
    }

    pub fn import(&mut self, b: &Block) -> ImportOutcome {
        self.import_with(b, None)
    }

    pub fn import_with(&mut self, b: &Block, payload: Option<Arc<Vec<u8>>>) -> ImportOutcome {
        match preverify(b, payload, &self.params, self.evm.as_deref()) {
            Ok(pre) => self.chain.import(Arc::new(b.clone()), pre, Source::Sync),
            Err(e) => ImportOutcome::Invalid(e),
        }
    }

    /// Produce the main candidate of the current epoch (with `extra_links`),
    /// import it and move to the next epoch.
    pub fn mine(&mut self, extra_links: &[HashLow]) -> HashLow {
        let ep = self.epochs();
        let time = ep.end_of_epoch(self.t(0));
        let mut t = BlockTemplate::new(self.params.header_field(), time);
        if let Some(pre) = self.chain.pretop_for(self.epoch).unwrap() {
            t.links.push((FieldType::Out, LinkTarget::Block(pre), CAmount::ZERO));
        }
        t.links.push((FieldType::Coinbase, LinkTarget::Address(self.miner.address()), CAmount::ZERO));
        for h in extra_links {
            t.links.push((FieldType::Out, LinkTarget::Block(*h), CAmount::ZERO));
        }
        t.sign_out = Some(self.miner.clone());
        let mut nonce = [0u8; 32];
        nonce[..8].copy_from_slice(&self.epoch.to_le_bytes());
        nonce[12..].copy_from_slice(&self.miner.address().0);
        t.mining_nonce = Some(nonce);
        let b = t.build().unwrap();
        let out = self.import(&b);
        assert!(out.is_imported(), "candidate import failed: {out:?}");
        self.epoch += 1;
        self.clock.set(ep.start_of_epoch(self.epoch) + 4096);
        self.chain.tick().unwrap();
        self.last_main = Some(b.hashlow());
        b.hashlow()
    }

    /// Build (not import) a main candidate for `epoch` on top of `prev`,
    /// ground until its sha256d difficulty has exactly `min_bits` leading zero
    /// bits, so its difficulty lies in [2^(32+bits), 2^(33+bits)).
    pub fn candidate_at(&self, epoch: u64, prev: Option<HashLow>, miner: &KeyPair, links: &[HashLow], min_bits: u32) -> Block {
        let ep = self.epochs();
        let time = ep.end_of_epoch(ep.start_of_epoch(epoch));
        let mut t = BlockTemplate::new(self.params.header_field(), time);
        if let Some(p) = prev {
            t.links.push((FieldType::Out, LinkTarget::Block(p), CAmount::ZERO));
        }
        t.links.push((FieldType::Coinbase, LinkTarget::Address(miner.address()), CAmount::ZERO));
        for h in links {
            t.links.push((FieldType::Out, LinkTarget::Block(*h), CAmount::ZERO));
        }
        t.sign_out = Some(miner.clone());
        let mut nonce = [0u8; 32];
        nonce[12..].copy_from_slice(&miner.address().0);
        t.mining_nonce = Some(nonce);
        let b = t.build().unwrap();
        let mut raw = *b.raw();
        let mut ctr = 0u64;
        loop {
            let h = xdag_types::hash::sha256d(&raw);
            if crate::pow::pow_zero_bits(&h) == min_bits {
                return Block::parse(&raw).unwrap();
            }
            ctr += 1;
            raw[480..488].copy_from_slice(&ctr.to_le_bytes());
        }
    }

    pub fn mine_n(&mut self, n: usize) -> Vec<HashLow> {
        (0..n).map(|_| self.mine(&[])).collect()
    }

    /// Pool payout: spend `amount` from a main block's balance to `to`.
    pub fn payout(&mut self, from_block: HashLow, to: Address, amount: Nano) -> Block {
        let mut t = BlockTemplate::new(self.params.header_field(), self.t(100));
        t.links.push((FieldType::In, LinkTarget::Block(from_block), amount.to_camount_legacy()));
        t.links.push((FieldType::Output, LinkTarget::Address(to), amount.to_camount_legacy()));
        t.sign_out = Some(self.miner.clone());
        t.include_out_pubkey = true;
        crate::builder::seal_template(&self.params, t).unwrap()
    }

    /// Legacy account transaction (as built by xdagj wallets).
    pub fn transfer(&self, from: &KeyPair, to: Address, amount: Nano, nonce: u64, offset: u64) -> Block {
        let mut t = BlockTemplate::new(self.params.header_field(), self.t(offset));
        t.tx_nonce = Some(nonce);
        t.links.push((FieldType::Input, LinkTarget::Address(from.address()), amount.to_camount_legacy()));
        t.links.push((FieldType::Output, LinkTarget::Address(to), amount.to_camount_legacy()));
        t.sign_out = Some(from.clone());
        t.include_out_pubkey = true;
        crate::builder::seal_template(&self.params, t).unwrap()
    }

    pub fn balance(&self, a: &Address) -> Nano {
        crate::query::balance(self.chain.db(), a).unwrap()
    }

    pub fn account(&self, a: &Address) -> Option<AccountRecord> {
        crate::query::account(self.chain.db(), a).unwrap()
    }

    pub fn block_amount(&mut self, h: &HashLow) -> i64 {
        self.chain.state(h).unwrap().amount
    }

    /// Genesis-style allocation: credit accounts directly (tests and benchmarks).
    pub fn genesis_alloc(&mut self, allocs: &[(Address, u128)]) {
        for (a, wei) in allocs {
            let mut r = self.chain.account(a).unwrap().unwrap_or_default();
            r.balance = crate::records::Balance::Wei(*wei);
            self.chain.put_account(a, &r).unwrap();
        }
        self.chain.commit(true).unwrap();
    }

    /// Current time of the simulated clock.
    pub fn now(&self) -> u64 {
        use crate::Clock;
        self.clock.now_xdag()
    }

    pub fn state_fingerprint(&self) -> Vec<(Vec<u8>, Vec<u8>)> {
        let db = self.chain.db();
        let mut v = vec![];
        for t in [
            xdag_storage::Table::Account,
            xdag_storage::Table::BlockState,
            xdag_storage::Table::MainHeight,
            xdag_storage::Table::Storage,
            xdag_storage::Table::Code,
        ] {
            db.for_each(t, |k, val| {
                let mut key = vec![t as u8];
                key.extend_from_slice(k);
                v.push((key, val.to_vec()));
                true
            })
            .unwrap();
        }
        v
    }
}
