//! The node: owns the chain, transaction pool, keys and the import pipeline,
//! and connects them to the P2P, RPC and mining components.

use std::collections::{HashMap, VecDeque};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, OnceLock};
use std::time::{Duration, Instant};

use crossbeam_channel::{Receiver, Sender};
use parking_lot::Mutex;
use rayon::prelude::*;
use xdag_chain::evm_api::{EvmEngine, EvmEnv, EvmExecResult};
use xdag_chain::mempool::TxPool;
use xdag_chain::pow::PowEngine;
use xdag_chain::preverify::{preverify, verify_payload_tx};
use xdag_chain::query;
use xdag_chain::{Chain, ChainEvent, ImportOutcome, Source};
use xdag_net::{ChainHandle, ConnId, NetHandle, Stats};
use xdag_storage::{Db, Table};
use xdag_types::nova::NovaTx;
use xdag_types::{Address, Block, FieldType, HashLow, KeyPair, LinkTarget, Nano, NetworkParams};

use crate::config::NodeConfig;

pub struct ImportJob {
    pub raw: Vec<u8>,
    pub payload: Option<Vec<u8>>,
    pub from: Option<ConnId>,
    pub ttl: i32,
    pub sync: bool,
    pub local: bool,
    pub reply: Option<crossbeam_channel::Sender<ImportOutcome>>,
}

#[derive(Default)]
struct Waiters {
    /// jobs waiting for a missing parent
    parents: HashMap<HashLow, Vec<ImportJob>>,
    parent_jobs: usize,
    /// jobs waiting for their Nova payload
    payloads: HashMap<HashLow, ImportJob>,
    /// legacy blocks deferred by state-dependent rules
    deferred: VecDeque<ImportJob>,
    /// recently requested blocks (rate limit requests)
    requested: HashMap<HashLow, Instant>,
}

const MAX_PARENT_WAITERS: usize = 200_000;
const MAX_DEFERRED: usize = 20_000;

pub struct Node {
    pub cfg: NodeConfig,
    pub params: Arc<NetworkParams>,
    pub db: Db,
    pub chain: Mutex<Chain>,
    pub txpool: Mutex<TxPool>,
    pub key: KeyPair,
    pub wallet: Option<Mutex<xdag_wallet::Wallet>>,
    pub evm: Option<Arc<dyn EvmEngine>>,
    #[allow(dead_code)]
    pub pow: Arc<dyn PowEngine>,
    pub rx_engine: Option<Arc<xdag_randomx::RandomXEngine>>,
    pub net: OnceLock<NetHandle>,
    import_tx: Sender<ImportJob>,
    import_rx: Receiver<ImportJob>,
    waiters: Mutex<Waiters>,
    pub started: Instant,
    pub shutdown: AtomicBool,
    /// highest nonce of legacy account transactions not yet executed, per sender
    pending_legacy: Mutex<HashMap<Address, u64>>,
}

impl Node {
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        cfg: NodeConfig,
        params: NetworkParams,
        db: Db,
        chain: Chain,
        key: KeyPair,
        wallet: Option<xdag_wallet::Wallet>,
        evm: Option<Arc<dyn EvmEngine>>,
        pow: Arc<dyn PowEngine>,
        rx_engine: Option<Arc<xdag_randomx::RandomXEngine>>,
    ) -> Arc<Node> {
        let (tx, rx) = crossbeam_channel::bounded(100_000);
        Arc::new(Node {
            cfg,
            params: Arc::new(params),
            db,
            chain: Mutex::new(chain),
            txpool: Mutex::new(TxPool::default()),
            key,
            wallet: wallet.map(Mutex::new),
            evm,
            pow,
            rx_engine,
            net: OnceLock::new(),
            import_tx: tx,
            import_rx: rx,
            waiters: Mutex::new(Waiters::default()),
            started: Instant::now(),
            shutdown: AtomicBool::new(false),
            pending_legacy: Mutex::new(HashMap::new()),
        })
    }

    pub fn net(&self) -> Option<&NetHandle> {
        self.net.get()
    }

    // ------------------------------------------------------------------
    // import pipeline
    // ------------------------------------------------------------------

    pub fn enqueue(&self, job: ImportJob) {
        if self.import_tx.try_send(job).is_err() {
            tracing::warn!("import queue full, dropping block");
        }
    }

    /// Import synchronously (local blocks, RPC). Waits for the outcome.
    pub fn import_local(&self, raw: Vec<u8>, payload: Option<Vec<u8>>) -> ImportOutcome {
        let (tx, rx) = crossbeam_channel::bounded(1);
        let ttl = self.cfg_ttl();
        self.enqueue(ImportJob { raw, payload, from: None, ttl, sync: false, local: true, reply: Some(tx) });
        rx.recv_timeout(Duration::from_secs(60)).unwrap_or(ImportOutcome::Error("import timed out".into()))
    }

    fn cfg_ttl(&self) -> i32 {
        5
    }

    /// Worker thread: batches jobs, pre-verifies them in parallel, imports
    /// them sequentially.
    pub fn import_loop(self: &Arc<Self>) {
        loop {
            if self.shutdown.load(Ordering::SeqCst) {
                return;
            }
            let first = match self.import_rx.recv_timeout(Duration::from_millis(200)) {
                Ok(j) => j,
                Err(crossbeam_channel::RecvTimeoutError::Timeout) => continue,
                Err(_) => return,
            };
            let mut jobs = vec![first];
            while jobs.len() < 4096 {
                match self.import_rx.try_recv() {
                    Ok(j) => jobs.push(j),
                    Err(_) => break,
                }
            }
            self.process(jobs);
        }
    }

    fn process(self: &Arc<Self>, jobs: Vec<ImportJob>) {
        let params = self.params.clone();
        let evm = self.evm.clone();
        // parse + stateless verification in parallel (signatures dominate)
        let verified: Vec<(ImportJob, Option<Arc<Block>>, Result<xdag_chain::PreVerified, String>)> = jobs
            .into_par_iter()
            .map(|j| match Block::parse(&j.raw) {
                Ok(b) => {
                    let b = Arc::new(b);
                    let pv = preverify(&b, j.payload.clone().map(Arc::new), &params, evm.as_deref());
                    (j, Some(b), pv)
                }
                Err(e) => (j, None, Err(format!("unparseable block: {e}"))),
            })
            .collect();

        let mut relay: Vec<(Vec<u8>, Option<Vec<u8>>, i32, Option<ConnId>)> = vec![];
        let mut resolved: Vec<HashLow> = vec![];
        let mut requests: Vec<(HashLow, Option<ConnId>)> = vec![];
        let mut payload_requests: Vec<(HashLow, Option<ConnId>)> = vec![];
        let mut penalties: Vec<(ConnId, String)> = vec![];
        let events;
        {
            let mut chain = self.chain.lock();
            for (job, block, pv) in verified {
                let Some(block) = block else {
                    if let Some(f) = job.from {
                        penalties.push((f, "unparseable block".into()));
                    }
                    reply(&job, ImportOutcome::Invalid("unparseable block".into()));
                    continue;
                };
                let h = block.hashlow();
                let pv = match pv {
                    Ok(p) => p,
                    Err(e) if e == "missing payload" => {
                        payload_requests.push((h, job.from));
                        self.waiters.lock().payloads.insert(h, job);
                        continue;
                    }
                    Err(e) => {
                        if let Some(f) = job.from {
                            penalties.push((f, e.clone()));
                        }
                        reply(&job, ImportOutcome::Invalid(e));
                        continue;
                    }
                };
                let src = if job.local {
                    Source::Local
                } else if job.sync {
                    Source::Sync
                } else {
                    Source::Gossip
                };
                let out = chain.import_uncommitted(block.clone(), pv, src);
                match &out {
                    ImportOutcome::ImportedBest | ImportOutcome::ImportedNotBest => {
                        if let Some((a, _)) = block.account_input() {
                            let n = block.tx_nonce.unwrap_or(0);
                            let mut pl = self.pending_legacy.lock();
                            let e = pl.entry(a).or_insert(0);
                            *e = (*e).max(n);
                        }
                        let ttl = if job.local { self.cfg_ttl() } else { job.ttl };
                        if ttl > 0 {
                            relay.push((job.raw.clone(), job.payload.clone(), ttl, job.from));
                        }
                        resolved.push(h);
                    }
                    ImportOutcome::NoParent(p) => {
                        let p = *p;
                        requests.push((p, job.from));
                        reply(&job, out.clone());
                        let mut w = self.waiters.lock();
                        if w.parent_jobs < MAX_PARENT_WAITERS {
                            w.parent_jobs += 1;
                            w.parents.entry(p).or_default().push(ImportJob { reply: None, ..job });
                        }
                        continue;
                    }
                    ImportOutcome::Deferred(_) => {
                        reply(&job, out.clone());
                        let mut w = self.waiters.lock();
                        if w.deferred.len() < MAX_DEFERRED && !job.local {
                            w.deferred.push_back(ImportJob { reply: None, ..job });
                        }
                        continue;
                    }
                    ImportOutcome::Invalid(m) => {
                        if let Some(f) = job.from {
                            if !m.contains("pool is full") {
                                penalties.push((f, m.clone()));
                            }
                        }
                    }
                    _ => {}
                }
                reply(&job, out);
            }
            if let Err(e) = chain.commit(false) {
                tracing::error!("commit of imported batch failed: {e}");
            }
            events = chain.take_events();
        }

        let net = self.net.get().cloned();
        if let Some(net) = &net {
            for (raw, payload, ttl, from) in relay {
                net.broadcast_block(&raw, payload.as_deref(), ttl, from);
            }
            let mut w = self.waiters.lock();
            let now = Instant::now();
            for (h, from) in requests {
                let recent = w.requested.get(&h).map(|t| now.duration_since(*t) < Duration::from_secs(10)).unwrap_or(false);
                if !recent {
                    w.requested.insert(h, now);
                    net.request_block(h, from, true);
                }
            }
            if w.requested.len() > 100_000 {
                w.requested.retain(|_, t| now.duration_since(*t) < Duration::from_secs(60));
            }
            drop(w);
            for (h, from) in payload_requests {
                net.request_payload(h, from);
            }
            for (f, why) in penalties {
                net.penalize(f, 10, &why);
            }
        }

        // requeue blocks that were waiting for what we just imported
        let mut requeue = vec![];
        {
            let mut w = self.waiters.lock();
            for h in resolved {
                if let Some(js) = w.parents.remove(&h) {
                    w.parent_jobs = w.parent_jobs.saturating_sub(js.len());
                    requeue.extend(js);
                }
            }
            if events.iter().any(|e| matches!(e, ChainEvent::MainSet { .. })) {
                requeue.extend(w.deferred.drain(..));
            }
        }
        for j in requeue {
            self.enqueue(j);
        }
        self.on_events(events);
    }

    fn on_events(&self, events: Vec<ChainEvent>) {
        let mut done = vec![];
        for e in events {
            match e {
                ChainEvent::TxDone { tx, sender, nonce } => {
                    if tx.len() == 32 {
                        done.push(<[u8; 32]>::try_from(tx.as_slice()).unwrap());
                    } else if let Some(a) = sender {
                        let mut pl = self.pending_legacy.lock();
                        if pl.get(&a).map(|p| *p <= nonce).unwrap_or(false) {
                            pl.remove(&a);
                        }
                    }
                }
                ChainEvent::MainSet { height, .. } if height % 64 == 0 => {
                    tracing::info!(height, "main chain height");
                }
                _ => {}
            }
        }
        if !done.is_empty() {
            let mut pool = self.txpool.lock();
            for h in done {
                pool.remove(&h);
            }
        }
    }

    // ------------------------------------------------------------------
    // status
    // ------------------------------------------------------------------

    pub fn is_synced(&self) -> bool {
        let (top_time, nmain) = {
            let meta = query::chain_meta(&self.db).unwrap_or_default();
            let t = meta.top.and_then(|t| query::block_view(&self.db, &t).ok().flatten()).map(|v| v.info.time);
            (t, meta.nmain)
        };
        let ep = self.params.epochs();
        let now = xdag_types::time::now_xdag();
        if let Some(t) = top_time {
            if t + 4 * ep.period() >= now {
                return true;
            }
        }
        // bootstrap of a fresh network: nobody claims to be ahead of us
        let ahead = self.net().map(|n| n.peers().iter().any(|p| p.latest_block > nmain as i64 + 1)).unwrap_or(false);
        let grace = Duration::from_millis(xdag_types::time::xdag_to_ms(2 * ep.period())).max(Duration::from_secs(3));
        self.started.elapsed() > grace && !ahead
    }

    pub fn rpc_status(&self) -> xdag_rpc::Status {
        let meta = query::chain_meta(&self.db).unwrap_or_default();
        xdag_rpc::Status {
            nmain: meta.nmain,
            nblocks: meta.nblocks,
            top: meta.top,
            top_diff: meta.top_diff,
            synced: self.is_synced(),
            extra: 0,
            pool_txs: self.txpool.lock().len(),
            now: xdag_types::time::now_xdag(),
        }
    }

    // ------------------------------------------------------------------
    // transactions
    // ------------------------------------------------------------------

    pub fn submit_nova(&self, tx: NovaTx, from: Option<ConnId>) -> Result<[u8; 32], String> {
        let v = verify_payload_tx(&tx, &self.params, self.evm.as_deref())?;
        let sender = v.sender();
        let rec = query::account(&self.db, &sender).map_err(|e| e.to_string())?;
        let (executed, bal) = match rec {
            Some(r) => (r.nonce, r.balance_wei().map_err(|e| e.to_string())?),
            None => (0, 0),
        };
        let h = v.hash();
        self.txpool.lock().add(v, tx.clone(), executed, bal).map_err(|e| e.to_string())?;
        if let Some(net) = self.net() {
            net.broadcast_txs(vec![(tx.kind(), tx.encode_inner())], from);
        }
        Ok(h)
    }

    pub fn next_nonce(&self, a: &Address, evm: bool) -> u64 {
        let executed = query::account(&self.db, a).ok().flatten().map(|r| r.nonce).unwrap_or(0);
        let pool = self.txpool.lock().highest_nonce(a);
        if evm {
            pool.map(|n| n + 1).unwrap_or(executed).max(executed)
        } else {
            let legacy = self.pending_legacy.lock().get(a).copied().unwrap_or(0);
            executed.max(legacy).max(pool.unwrap_or(0)) + 1
        }
    }

    /// Build a legacy account transaction from a wallet key.
    pub fn wallet_transfer(&self, from: Option<Address>, to: Address, amount: Nano, remark: &str, password: &str) -> Result<Vec<String>, String> {
        let wallet = self.wallet.as_ref().ok_or("node has no wallet")?;
        let path = self.cfg.wallet_path();
        xdag_wallet::Wallet::unlock(&path, password).map_err(|_| "wallet unlock failed".to_string())?;
        let key = {
            let w = wallet.lock();
            match from {
                Some(a) => w.key_for(&a).cloned().ok_or("address not in wallet")?,
                None => w.default_key().cloned().ok_or("empty wallet")?,
            }
        };
        let fee = self.params.min_gas;
        let total = amount.checked_add(fee).ok_or("amount overflow")?;
        let bal = query::balance(&self.db, &key.address()).map_err(|e| e.to_string())?;
        if bal < total {
            return Err("balance not enough".into());
        }
        let nonce = self.next_nonce(&key.address(), false);
        let now = xdag_types::time::now_xdag();
        let ep = self.params.epochs();
        let time = if ep.is_end_of_epoch(now) { now - 1 } else { now };
        let mut t = xdag_types::BlockTemplate::new(self.params.header_field(), time);
        t.tx_nonce = Some(nonce);
        // the receiver gets `amount`: the per-output fee is taken from the output
        t.links.push((FieldType::Input, LinkTarget::Address(key.address()), total.to_camount_legacy()));
        t.links.push((FieldType::Output, LinkTarget::Address(to), total.to_camount_legacy()));
        if !remark.is_empty() {
            let mut r = [0u8; 32];
            let n = remark.len().min(32);
            r[..n].copy_from_slice(&remark.as_bytes()[..n]);
            t.remark = Some(r);
        }
        t.sign_out = Some(key);
        t.include_out_pubkey = true;
        let b = xdag_chain::builder::seal_template(&self.params, t).map_err(|e| e.to_string())?;
        match self.import_local(b.raw().to_vec(), None) {
            o if o.is_imported() => Ok(vec![b.hashlow().to_legacy_address()]),
            o => Err(format!("{o:?}")),
        }
    }

    pub fn evm_env(&self) -> EvmEnv {
        let nova = self.params.nova.clone().unwrap_or_else(|| xdag_types::NovaParams::defaults(0));
        let meta = query::chain_meta(&self.db).unwrap_or_default();
        EvmEnv {
            chain_id: nova.chain_id,
            height: meta.nmain + 1,
            timestamp: xdag_types::time::now_ms() / 1000,
            coinbase: self.key.address(),
            prevrandao: [0u8; 32],
            gas_limit: nova.batch_gas_limit,
            min_gas_price: 0,
        }
    }
}

fn reply(job: &ImportJob, out: ImportOutcome) {
    if let Some(r) = &job.reply {
        let _ = r.send(out);
    }
}

// ----------------------------------------------------------------------
// P2P interface
// ----------------------------------------------------------------------

pub struct NetBridge(pub Arc<Node>);

impl ChainHandle for NetBridge {
    fn stats(&self) -> Stats {
        let meta = query::chain_meta(&self.0.db).unwrap_or_default();
        Stats {
            max_difficulty: meta.top_diff,
            total_blocks: meta.nblocks as i64,
            total_main: meta.nmain as i64,
            total_hosts: self.0.net().map(|n| n.peers().len() as i32 + 1).unwrap_or(1),
            main_time: xdag_types::time::now_xdag() as i64,
        }
    }
    fn latest_main(&self) -> i64 {
        query::chain_meta(&self.0.db).map(|m| m.nmain as i64).unwrap_or(0)
    }
    fn last_main_time(&self) -> u64 {
        let db = &self.0.db;
        let meta = query::chain_meta(db).unwrap_or_default();
        query::main_hashlow(db, meta.nmain).ok().flatten().and_then(|h| query::block_view(db, &h).ok().flatten()).map(|v| v.info.time).unwrap_or(0)
    }
    fn is_synced(&self) -> bool {
        self.0.is_synced()
    }
    fn on_block(&self, raw: Vec<u8>, payload: Option<Vec<u8>>, from: ConnId, ttl: i32, sync: bool) {
        self.0.enqueue(ImportJob { raw, payload, from: Some(from), ttl, sync, local: false, reply: None });
    }
    fn on_txs(&self, txs: Vec<(u8, Vec<u8>)>, from: ConnId) {
        for (kind, bytes) in txs {
            let tx = match kind {
                xdag_types::nova::KIND_NATIVE => match xdag_types::nova::NativeTransfer::decode(&bytes) {
                    Ok(t) => NovaTx::Native(t),
                    Err(_) => continue,
                },
                xdag_types::nova::KIND_EVM => NovaTx::Evm(bytes),
                _ => continue,
            };
            let _ = self.0.submit_nova(tx, Some(from));
        }
    }
    fn on_payload(&self, h: HashLow, payload: Vec<u8>, from: ConnId) {
        let job = self.0.waiters.lock().payloads.remove(&h);
        match job {
            Some(mut j) => {
                j.payload = Some(payload);
                self.0.enqueue(j);
            }
            None => {
                // unsolicited (sent after SYNC_BLOCK): keep it for the block that follows
                let _ = from;
            }
        }
    }
    fn get_block(&self, h: &HashLow) -> Option<(Vec<u8>, u8, Option<Vec<u8>>)> {
        // main-block candidates are kept in memory until referenced (xdagj serves them too)
        if let Some((b, payload)) = self.0.chain.lock().extra_block(h) {
            return Some((b.raw().to_vec(), 0, payload));
        }
        let v = query::block_view(&self.0.db, h).ok().flatten()?;
        let raw = v.block.as_ref()?.raw().to_vec();
        let f = v.flags() & !(xdag_chain::flags::OURS | xdag_chain::flags::REMARK);
        use xdag_chain::flags::*;
        let exec = if f == REF | MAIN_REF | APPLIED {
            1
        } else if f == REF | MAIN_REF {
            2
        } else {
            0
        };
        Some((raw, exec, v.payload))
    }
    fn blocks_in_range(&self, start: u64, end: u64, limit: usize) -> Vec<(Vec<u8>, u8)> {
        let ep = self.0.params.epochs();
        let (s, e) = (ep.epoch(start), ep.epoch(end.saturating_sub(1)) + 1);
        let hs = query::blocks_in_epochs(&self.0.db, s, e, limit).unwrap_or_default();
        hs.iter()
            .filter_map(|h| self.get_block(h))
            .filter(|(raw, _, _)| {
                let t = u64::from_le_bytes(raw[16..24].try_into().unwrap());
                t >= start && t < end
            })
            .map(|(r, x, _)| (r, x))
            .collect()
    }
    fn sums(&self, start: u64, end: u64) -> Option<[u8; 256]> {
        let ov = xdag_chain::overlay::Overlay::new(self.0.db.clone());
        xdag_chain::sums::load(&ov, start, end).ok().flatten()
    }
    fn get_payload(&self, h: &HashLow) -> Option<Vec<u8>> {
        self.0.db.get(Table::Payload, &h.0).ok().flatten()
    }
}

// ----------------------------------------------------------------------
// RPC interface
// ----------------------------------------------------------------------

pub struct RpcBridge(pub Arc<Node>);

impl xdag_rpc::Backend for RpcBridge {
    fn params(&self) -> &NetworkParams {
        &self.0.params
    }
    fn db(&self) -> &Db {
        &self.0.db
    }
    fn status(&self) -> xdag_rpc::Status {
        self.0.rpc_status()
    }
    fn submit_block(&self, raw: Vec<u8>) -> Result<HashLow, String> {
        let b = Block::parse(&raw).map_err(|e| e.to_string())?;
        if b.inputs.is_empty() {
            return Err("THE TX NEEDS INPUT".into());
        }
        match self.0.import_local(raw, None) {
            o if o.is_imported() => Ok(b.hashlow()),
            ImportOutcome::Exist | ImportOutcome::InMem => Ok(b.hashlow()),
            o => Err(format!("{o:?}")),
        }
    }
    fn submit_nova_tx(&self, tx: NovaTx) -> Result<[u8; 32], String> {
        self.0.submit_nova(tx, None)
    }
    fn evm_call(&self, from: Address, to: Option<Address>, data: Vec<u8>, value: u128, gas: Option<u64>) -> Result<EvmExecResult, String> {
        let evm = self.0.evm.as_ref().ok_or("EVM not enabled on this network")?;
        let env = self.0.evm_env();
        let gas = gas.unwrap_or(env.gas_limit).min(env.gas_limit);
        let mut state = query::DbState(&self.0.db);
        evm.call(&mut state, &env, from, to, data, value, gas)
    }
    fn peers(&self) -> Vec<xdag_net::PeerInfo> {
        self.0.net().map(|n| n.peers()).unwrap_or_default()
    }
    fn coinbase(&self) -> Option<Address> {
        Some(self.0.key.address())
    }
    fn next_nonce(&self, a: &Address, evm: bool) -> u64 {
        self.0.next_nonce(a, evm)
    }
    fn wallet_transfer(&self, from: Option<Address>, to: Address, amount: Nano, remark: &str, password: &str) -> Result<Vec<String>, String> {
        self.0.wallet_transfer(from, to, amount, remark, password)
    }
    fn pending(&self) -> Vec<([u8; 32], Address, u64, bool)> {
        self.0.txpool.lock().list()
    }
    fn client_version(&self) -> String {
        format!("xdag-rs/v{}/{}-{}", env!("CARGO_PKG_VERSION"), std::env::consts::OS, std::env::consts::ARCH)
    }
}
