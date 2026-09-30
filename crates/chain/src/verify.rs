//! Offline audit of a snapshot file (`xdagd snapshot verify`).
//!
//! Re-derives what can be re-derived from the file alone and compares it with
//! what the file records:
//!
//! * every block carried with its raw data (as a full block or as key
//!   material) must parse, hash to its key and carry its recorded time, and an
//!   account transaction must be signed by the account it spends;
//! * its recorded cumulative difficulty and max-difficulty link must follow
//!   from the blocks it links to and from its own proof of work: sha256d or,
//!   for main-block candidates after the fork, RandomX under the seed the main
//!   chain selects for the block's epoch;
//! * the main-chain index must be a chain: below each main block, the next
//!   block of an earlier epoch on its max-difficulty path that gained
//!   difficulty over its own link (xdagj's rule for marking the main chain)
//!   is the main block one height lower, and cumulative difficulty grows
//!   with height;
//! * the balances are added up and compared with the block rewards issued.
//!
//! Snapshots converted from xdagj hold most blocks as metadata only and lack
//! blocks xdagj dropped, so part of the checks cannot be made for part of the
//! blocks; the report says how many were checked.
//!
//! The file is read twice and about 48 bytes per block are kept in memory.

use std::cmp::Reverse;
use std::collections::{BTreeMap, BinaryHeap};
use std::io::Read;
use std::sync::Arc;

use xdag_types::crypto::pubkey_address;
use xdag_types::time::Epochs;
use xdag_types::{Block, CAmount, Difficulty, HashLow, NetworkParams, U256};

use crate::evm_api::EvmEngine;
use crate::pow::{own_difficulty, PowEngine, RxSchedule, Seed};
use crate::records::{flags, Balance, SnapshotKey};
use crate::snapshot::{SnapshotData, SnapshotReader};
use crate::{fees, ChainError, Result};

/// Longest same-epoch walk along max-difficulty links.
const MAX_WALK: usize = 1 << 16;
/// Findings listed individually per kind.
const SHOWN: usize = 8;

#[derive(Clone, Debug)]
pub struct VerifyOptions {
    /// Check the RandomX proof of work of at most this many main-block
    /// candidates, the newest ones (each takes one RandomX hash; every seed
    /// they span takes a cache initialisation).
    pub randomx_limit: usize,
}

impl Default for VerifyOptions {
    fn default() -> Self {
        VerifyOptions { randomx_limit: usize::MAX }
    }
}

#[derive(Clone, Debug, Default)]
pub struct VerifyReport {
    /// Human-readable report.
    pub lines: Vec<String>,
    /// Checks that were made and failed.
    pub failures: u64,
}

/// What is kept of every block: enough to follow max-difficulty links.
#[derive(Clone, Copy)]
struct Row {
    /// First 8 bytes of the hashlow.
    fp: u64,
    time: u64,
    /// Cumulative difficulty, high and low half.
    diff: [u64; 2],
    mdl: u64,
    height: u32,
    flags: u8,
    has_mdl: bool,
}

impl Row {
    fn diff(&self) -> Difficulty {
        U256::from((self.diff[0] as u128) << 64 | self.diff[1] as u128)
    }
}

fn fp(h: &HashLow) -> u64 {
    u64::from_le_bytes(h.0[..8].try_into().unwrap())
}

enum Exit {
    At(Row),
    /// The path ends inside the epoch.
    None,
    /// The path leads to a block the file does not contain.
    Unknown,
}

struct Table {
    rows: Vec<Row>,
    ep: Epochs,
}

impl Table {
    fn get(&self, fp: u64) -> Option<&Row> {
        self.rows.binary_search_by_key(&fp, |r| r.fp).ok().map(|i| &self.rows[i])
    }

    /// First block on `start`'s max-difficulty path that lies in another epoch.
    fn exit(&self, start: &Row) -> Exit {
        let e = self.ep.epoch(start.time);
        let mut cur = *start;
        for _ in 0..MAX_WALK {
            if !cur.has_mdl {
                return Exit::None;
            }
            let Some(m) = self.get(cur.mdl) else { return Exit::Unknown };
            if self.ep.epoch(m.time) != e {
                return Exit::At(*m);
            }
            cur = *m;
        }
        Exit::Unknown
    }

    /// The main-chain block below `start`: walking down the max-difficulty
    /// path, the first block of an earlier epoch whose difficulty exceeds that
    /// of its own max-difficulty link (xdagj marks exactly these blocks).
    fn below(&self, start: &Row) -> Exit {
        let e = self.ep.epoch(start.time);
        let mut cur = *start;
        for _ in 0..MAX_WALK {
            if !cur.has_mdl {
                return Exit::None;
            }
            let Some(b) = self.get(cur.mdl).copied() else { return Exit::Unknown };
            let gains = match b.has_mdl {
                false => true,
                true => match self.get(b.mdl) {
                    Some(m) => b.diff() > m.diff(),
                    None => return Exit::Unknown,
                },
            };
            if gains && self.ep.epoch(b.time) < e {
                return Exit::At(b);
            }
            cur = b;
        }
        Exit::Unknown
    }

    /// Cumulative difficulty `block` gets through the link `r` (xdagj
    /// `calculateBlockDiff`); `None` if the file lacks a block it depends on.
    fn through(&self, block_epoch: u64, r: &HashLow, own: Difficulty) -> Option<Difficulty> {
        let ri = self.get(fp(r))?;
        if self.ep.epoch(ri.time) < block_epoch {
            return Some(ri.diff() + own);
        }
        let mut cur = ri.diff();
        match self.exit(ri) {
            Exit::At(x) => {
                if self.ep.epoch(x.time) < block_epoch && x.diff() + own > cur {
                    cur = x.diff() + own;
                }
            }
            Exit::None => {}
            Exit::Unknown => return None,
        }
        Some(cur)
    }
}

enum DiffCheck {
    /// Every link was resolved: difficulty and max-difficulty link are exact.
    Full,
    /// The recorded link was resolved and gives the recorded difficulty.
    Partial,
    /// The file lacks the blocks needed.
    Unverifiable,
    Wrong(String),
}

fn check_difficulty(t: &Table, b: &Block, recorded: Difficulty, recorded_link: Option<HashLow>, own: u128) -> DiffCheck {
    let own = U256::from(own);
    let epoch = t.ep.epoch(b.time);
    let (mut max, mut max_link, mut complete) = (own, None, true);
    for r in b.block_links() {
        match t.through(epoch, &r, own) {
            Some(cur) if cur > max => {
                max = cur;
                max_link = Some(r);
            }
            Some(_) => {}
            None => complete = false,
        }
    }
    if complete {
        return if (max, max_link) == (recorded, recorded_link) {
            DiffCheck::Full
        } else {
            DiffCheck::Wrong(format!("difficulty {recorded:x} via {recorded_link:?} recorded, {max:x} via {max_link:?} computed"))
        };
    }
    // blocks the file lacks can only have raised the result
    if max > recorded {
        return DiffCheck::Wrong(format!("difficulty {recorded:x} recorded, at least {max:x} computed"));
    }
    match recorded_link {
        Some(l) => match t.through(epoch, &l, own) {
            Some(cur) if cur == recorded => DiffCheck::Partial,
            Some(cur) => DiffCheck::Wrong(format!("difficulty {recorded:x} recorded, {cur:x} computed through its max-difficulty link")),
            None => DiffCheck::Unverifiable,
        },
        None if recorded == own => DiffCheck::Partial,
        None => DiffCheck::Wrong(format!("difficulty {recorded:x} recorded without a link, own difficulty {own:x}")),
    }
}

/// Proof of work that applies to a main-block candidate of some epoch.
enum Pow {
    Sha256d,
    RandomX(Seed),
    /// The main blocks that decide it are not in the file.
    Unknown,
}

/// The RandomX seeds of the main chain, as far as the file has the blocks.
struct SeedHistory {
    forked: bool,
    fork_epoch: Option<u64>,
    seeds: Vec<Seed>,
    nmain: u64,
    seed_epoch_blocks: u64,
}

impl SeedHistory {
    fn pow_for(&self, epoch: u64) -> Pow {
        if !self.forked || self.fork_epoch.is_some_and(|f| epoch <= f) {
            return Pow::Sha256d;
        }
        // the newest seed in effect, provided the one after it is known not to be
        let Some(i) = self.seeds.iter().rposition(|s| s.switch_epoch <= epoch) else { return Pow::Unknown };
        let next = self.seeds[i].height + self.seed_epoch_blocks;
        if next <= self.nmain && self.seeds.get(i + 1).is_none_or(|s| s.height != next) {
            return Pow::Unknown;
        }
        Pow::RandomX(self.seeds[i].clone())
    }
}

/// A candidate whose RandomX hash is computed after the second pass.
struct Deferred {
    time: u64,
    hashlow: HashLow,
    block: Block,
    recorded: Difficulty,
    recorded_link: Option<HashLow>,
    seed: Seed,
}

impl PartialEq for Deferred {
    fn eq(&self, o: &Self) -> bool {
        (self.time, self.hashlow) == (o.time, o.hashlow)
    }
}
impl Eq for Deferred {}
impl PartialOrd for Deferred {
    fn partial_cmp(&self, o: &Self) -> Option<std::cmp::Ordering> {
        Some(self.cmp(o))
    }
}
impl Ord for Deferred {
    fn cmp(&self, o: &Self) -> std::cmp::Ordering {
        (self.time, self.hashlow.0).cmp(&(o.time, o.hashlow.0))
    }
}

#[derive(Default)]
struct Findings {
    failures: u64,
    shown: BTreeMap<&'static str, Vec<String>>,
    counts: BTreeMap<&'static str, u64>,
}

impl Findings {
    fn fail(&mut self, kind: &'static str, line: String) {
        self.failures += 1;
        *self.counts.entry(kind).or_default() += 1;
        let v = self.shown.entry(kind).or_default();
        if v.len() < SHOWN {
            v.push(line);
        }
    }

    fn count(&self, kind: &'static str) -> u64 {
        self.counts.get(kind).copied().unwrap_or(0)
    }

    /// The findings of one kind, as report lines.
    fn list(&self, kind: &'static str, out: &mut Vec<String>) {
        let Some(v) = self.shown.get(kind) else { return };
        out.extend(v.iter().map(|l| format!("    {l}")));
        let more = self.count(kind) - v.len() as u64;
        if more > 0 {
            out.push(format!("    ... and {more} more"));
        }
    }
}

fn xdag(nano: i128) -> String {
    let a = nano.unsigned_abs();
    format!("{}{}.{:09}", if nano < 0 { "-" } else { "" }, a / 1_000_000_000, a % 1_000_000_000)
}

/// Audit the snapshot `open` yields (it is read twice) under the rules `p`.
pub fn verify<R: Read>(
    mut open: impl FnMut() -> Result<R>,
    p: &NetworkParams,
    pow: &dyn PowEngine,
    evm: Option<&Arc<dyn EvmEngine>>,
    opts: &VerifyOptions,
) -> Result<VerifyReport> {
    let ep = p.epochs();
    let max_payload = p.nova.as_ref().map(|n| n.max_payload_bytes).unwrap_or(0);
    let mut out = vec![];
    let mut f = Findings::default();

    // ---- first pass: totals and the table of all blocks ---------------------
    let (mut r, header) = SnapshotReader::new(open()?, max_payload)?;
    if header.network != p.network.id() {
        return Err(ChainError::Invalid("snapshot is for another network".into()));
    }
    out.push(format!(
        "network {}, main height {}, top {} (difficulty {:x})",
        p.network,
        header.nmain,
        header.top.to_legacy_address(),
        header.top_diff
    ));

    let accounts = r.section()?;
    let (mut account_nano, mut funded, mut with_nonce) = (0i128, 0u64, 0u64);
    for _ in 0..accounts {
        let (a, rec) = r.account()?;
        let nano = match rec.balance {
            Balance::Legacy(c) => CAmount(c).to_nano_legacy().map_err(|_| ChainError::Invalid(format!("balance of {a:?} out of range")))?.0 as i128,
            Balance::Wei(w) => (w / xdag_types::amount::WEI_PER_NANO) as i128,
        };
        account_nano += nano;
        funded += (nano > 0) as u64;
        with_nonce += (rec.nonce > 0) as u64;
    }
    out.push(format!("accounts: {accounts} ({funded} with a balance, {with_nonce} that have sent transactions), {} XDAG", xdag(account_nano)));

    let blocks = r.section()?;
    let mut rows = Vec::with_capacity(blocks as usize);
    let mut kinds = [0u64; 5];
    let mut by_flags: BTreeMap<u8, u64> = BTreeMap::new();
    let (mut block_nano, mut before_era, mut unheighted_mains) = (0i128, 0u64, 0u64);
    let mut negative = vec![];
    for _ in 0..blocks {
        let b = r.block()?;
        kinds[match &b.data {
            SnapshotData::None => 0,
            SnapshotData::Key(SnapshotKey::PublicKey(_)) => 1,
            SnapshotData::Key(SnapshotKey::RawBlock(_)) => 2,
            SnapshotData::Full { payload: None, .. } => 3,
            SnapshotData::Full { .. } => 4,
        }] += 1;
        *by_flags.entry(b.flags & !(flags::OURS | flags::EXTRA)).or_default() += 1;
        block_nano += b.amount as i128;
        if b.amount < 0 {
            negative.push(format!(
                "{} holds {} XDAG (time {:x}, flags {:02x})",
                b.hashlow.to_legacy_address(),
                xdag(b.amount as i128),
                b.time,
                b.flags
            ));
        }
        before_era += (b.time < p.era) as u64;
        if b.hash.0 != [0u8; 32] && b.hash.hashlow() != b.hashlow {
            f.fail("hash", format!("{}: the recorded hash {:?} is not the hash of this block", b.hashlow.to_legacy_address(), b.hash));
        }
        let main = b.flags & flags::MAIN != 0;
        unheighted_mains += (main && b.height == 0) as u64;
        if b.difficulty >> 128 != U256::ZERO || b.height > u32::MAX as u64 {
            return Err(ChainError::Invalid(format!("block {:?}: difficulty or height out of range", b.hashlow)));
        }
        let d: u128 = b.difficulty.to();
        rows.push(Row {
            fp: fp(&b.hashlow),
            time: b.time,
            diff: [(d >> 64) as u64, d as u64],
            mdl: b.max_diff_link.as_ref().map(fp).unwrap_or(0),
            height: if main { b.height as u32 } else { 0 },
            flags: b.flags,
            has_mdl: b.max_diff_link.is_some(),
        });
    }
    rows.sort_unstable_by_key(|r| r.fp);
    if rows.windows(2).any(|w| w[0].fp == w[1].fp) {
        return Err(ChainError::Invalid("two block entries share their first 8 hash bytes: duplicate entries".into()));
    }
    let t = Table { rows, ep };
    out.push(format!(
        "blocks: {blocks} ({} with their data; without: {} with a public key, {} with the block as key material, {} without a key), {} XDAG",
        kinds[3] + kinds[4],
        kinds[1],
        kinds[2],
        kinds[0],
        xdag(block_nano)
    ));
    out.push(format!("  by flags: {}", by_flags.iter().map(|(fl, n)| format!("{fl:02x}: {n}")).collect::<Vec<_>>().join(", ")));
    if before_era > 0 {
        out.push(format!("  {before_era} blocks carry a time before the network's start"));
    }
    if !negative.is_empty() {
        out.push(format!("  {} blocks have a negative balance:", negative.len()));
        out.extend(negative.iter().take(SHOWN).map(|l| format!("    {l}")));
    }
    if f.count("hash") > 0 {
        out.push(format!("  {} blocks record a hash that is not theirs:", f.count("hash")));
        f.list("hash", &mut out);
    }

    // supply
    let issued: i128 = (1..=header.nmain).map(|h| fees::reward(h, p).0 as i128).sum();
    let held = account_nano + block_nano;
    out.push(format!(
        "supply: {} XDAG held; block rewards of {} main blocks: {} XDAG; difference {}{}",
        xdag(held),
        header.nmain,
        xdag(issued),
        if held >= issued { "+" } else { "" },
        xdag(held - issued)
    ));

    // main-chain index
    let mains = r.section()?;
    let mut index: Vec<(u64, u64)> = Vec::with_capacity(mains as usize);
    let mut seed_sources: BTreeMap<u64, HashLow> = BTreeMap::new();
    let rx = &p.randomx;
    for _ in 0..mains {
        let (h, hl) = r.main()?;
        index.push((h, fp(&hl)));
        if h + rx.seed_lag >= rx.fork_height && (h + rx.seed_lag) & (rx.seed_epoch_blocks - 1) == 0 {
            seed_sources.insert(h, hl);
        }
    }
    drop(r);
    index.sort_unstable();
    let main_at = |h: u64| index.binary_search_by_key(&h, |e| e.0).ok().and_then(|i| t.get(index[i].1));
    match t.get(fp(&header.top)) {
        None => f.fail("top", "the top block is not in the file".into()),
        Some(top) if top.diff() != header.top_diff => {
            f.fail("top", format!("the header gives the top block difficulty {:x}, its entry {:x}", header.top_diff, top.diff()))
        }
        _ => {}
    }

    let (mut linked, mut unlinked, mut linkless, mut ranges) = (0u64, 0u64, 0u64, vec![]);
    let mut prev: Option<(u64, Row)> = None;
    for (h, key) in &index {
        let Some(row) = t.get(*key).copied() else {
            f.fail("main", format!("main block {h} has no block entry"));
            prev = None;
            continue;
        };
        if row.flags & flags::MAIN == 0 || row.height as u64 != *h {
            f.fail("main", format!("main block {h}: its entry has flags {:02x} and height {}", row.flags, row.height));
        }
        match prev {
            Some((ph, pr)) if ph + 1 == *h => {
                if ep.epoch(row.time) <= ep.epoch(pr.time) {
                    f.fail("main", format!("main block {h} (time {:x}) is not in a later epoch than main block {ph} (time {:x})", row.time, pr.time));
                }
                if row.diff() <= pr.diff() {
                    f.fail("main", format!("main block {h} has difficulty {:x}, main block {ph} {:x}", row.diff(), pr.diff()));
                }
                match t.below(&row) {
                    Exit::At(x) if x.fp == pr.fp => linked += 1,
                    Exit::Unknown => unlinked += 1,
                    // metadata inherited from a snapshot may lack the link
                    Exit::None if !row.has_mdl => linkless += 1,
                    _ => f.fail("main", format!("the max-difficulty path of main block {h} does not lead to main block {ph}")),
                }
            }
            _ => ranges.push((*h, *h)),
        }
        if let Some(last) = ranges.last_mut() {
            last.1 = *h;
        }
        prev = Some((*h, row));
    }
    let ranges: Vec<String> = ranges.iter().map(|(a, b)| if a == b { a.to_string() } else { format!("{a}..{b}") }).collect();
    out.push(format!(
        "main chain: {mains} entries, heights {}{}",
        ranges.iter().take(SHOWN).cloned().collect::<Vec<_>>().join(", "),
        if ranges.len() > SHOWN { format!(" and {} more ranges", ranges.len() - SHOWN) } else { String::new() }
    ));
    if unheighted_mains > 0 {
        out.push(format!("  {unheighted_mains} blocks are marked as main blocks without a height (not in the index)"));
    }
    out.push(format!(
        "  consecutive main blocks: {linked} linked by the max-difficulty path with growing difficulty and epoch, {} wrong; not checkable: {linkless} (no link recorded), {unlinked} (path leaves the file)",
        f.count("main")
    ));
    f.list("main", &mut out);
    f.list("top", &mut out);

    // RandomX seeds the main chain defines
    let forked = header.nmain >= rx.fork_height;
    let fork_epoch = main_at(rx.fork_height).map(|b| ep.epoch(b.time) + rx.seed_lag);
    let mut seeds = vec![];
    if forked {
        let mut h = rx.fork_height;
        while h <= header.nmain {
            if let (Some(b), Some(src)) = (main_at(h), seed_sources.get(&(h - rx.seed_lag))) {
                let mut key = [0u8; 32];
                key[..24].copy_from_slice(&src.0);
                seeds.push(Seed { height: h, switch_epoch: ep.epoch(b.time) + rx.seed_lag + 1, key });
            }
            h += rx.seed_epoch_blocks;
        }
        let expected = (header.nmain - rx.fork_height) / rx.seed_epoch_blocks + 1;
        out.push(format!("RandomX: {} of the {expected} seeds since the fork can be derived from the main blocks in the file", seeds.len()));
    }
    let history = SeedHistory { forked, fork_epoch, seeds, nmain: header.nmain, seed_epoch_blocks: rx.seed_epoch_blocks };

    // ---- second pass: the blocks that come with their data ----------------------
    let (mut r, _) = SnapshotReader::new(open()?, max_payload)?;
    for _ in 0..r.section()? {
        r.account()?;
    }
    let (mut with_data, mut transport, mut account_txs, mut keyed, mut self_signed) = (0u64, 0u64, 0u64, 0u64, 0u64);
    let (mut full, mut partial, mut unverifiable, mut unknown_pow, mut skipped_rx, mut unrecorded) = (0u64, 0u64, 0u64, 0u64, 0u64, 0u64);
    let (mut foreign_keys, mut old_rule) = (vec![], vec![]);
    let mut deferred: BinaryHeap<Reverse<Deferred>> = BinaryHeap::new();
    let mut settle = |f: &mut Findings, hashlow: &HashLow, c: DiffCheck| match c {
        DiffCheck::Full => full += 1,
        DiffCheck::Partial => partial += 1,
        DiffCheck::Unverifiable => unverifiable += 1,
        DiffCheck::Wrong(m) => f.fail("difficulty", format!("{}: {m}", hashlow.to_legacy_address())),
    };
    for _ in 0..r.section()? {
        let blk = r.block()?;
        let (mut raw, payload, key_only) = match blk.data {
            SnapshotData::Full { raw, payload } => (raw, payload, false),
            SnapshotData::Key(SnapshotKey::RawBlock(raw)) => (raw, None, true),
            _ => continue,
        };
        with_data += 1;
        let name = blk.hashlow.to_legacy_address();
        let mut parsed = Block::parse(&raw[..]);
        if parsed.as_ref().is_ok_and(|b| b.hashlow() != blk.hashlow) && raw[..8] != [0u8; 8] {
            // stored with a transport header, which is not part of the block
            raw[..8].fill(0);
            parsed = Block::parse(&raw[..]);
            transport += 1;
        }
        let b = match parsed {
            Ok(b) => b,
            Err(e) => {
                f.fail("block", format!("{name}: {e}"));
                continue;
            }
        };
        if b.hashlow() != blk.hashlow || (blk.hash.0 != [0u8; 32] && b.hash() != blk.hash) {
            if key_only {
                // only ever used to check a signature, never as the block itself
                foreign_keys.push(format!("{name} ({} XDAG)", xdag(blk.amount as i128)));
            } else {
                f.fail("block", format!("{name}: the data hashes to {:?}", b.hash()));
            }
            continue;
        }
        if b.time != blk.time {
            f.fail("block", format!("{name}: time {:x} recorded, {:x} in the block", blk.time, b.time));
        }
        // (key material may come from another network's chain: only what it signs matters)
        if !key_only && (b.type_word & 0xf) as u8 != p.header_type {
            f.fail("block", format!("{name}: not a block of this network (header type {:x})", b.type_word & 0xf));
        }
        match crate::preverify::preverify(&b, payload.map(Arc::new), p, evm.map(|e| &**e)) {
            Ok(pre) => {
                if !b.pubkeys.is_empty() {
                    keyed += 1;
                    self_signed += (!pre.keys.is_empty()) as u64;
                }
                if let Some((from, _)) = b.account_input() {
                    account_txs += 1;
                    if !pre.keys.iter().any(|k| pubkey_address(k) == from) {
                        f.fail("signature", format!("{name}: not signed by the account it spends"));
                    }
                }
            }
            Err(e) => f.fail("signature", format!("{name}: {e}")),
        }

        if key_only && blk.difficulty == U256::ZERO && blk.max_diff_link.is_none() {
            // inherited without its difficulty
            unrecorded += 1;
            continue;
        }
        // the blocks `own_difficulty` scores with RandomX once the fork is active
        let candidate = ep.is_end_of_epoch(b.time) && b.inputs.is_empty() && (b.nonce.is_some() || !p.is_nova_time(b.time));
        let seed = match if candidate { history.pow_for(ep.epoch(b.time)) } else { Pow::Sha256d } {
            Pow::Sha256d => None,
            Pow::RandomX(s) => Some(s),
            Pow::Unknown => {
                unknown_pow += 1;
                continue;
            }
        };
        match seed {
            None => {
                let own = own_difficulty(&b, p, &RxSchedule::default(), pow);
                let check = check_difficulty(&t, &b, blk.difficulty, blk.max_diff_link, own);
                // xdagj used to score transactions by their hash like any other
                // block; since then a block with inputs counts as 1
                if matches!(check, DiffCheck::Wrong(_)) && !b.inputs.is_empty() && !p.is_nova_time(b.time) {
                    let by_hash = xdag_types::difficulty::hash_difficulty(&b.hash().0);
                    if matches!(check_difficulty(&t, &b, blk.difficulty, blk.max_diff_link, by_hash), DiffCheck::Full | DiffCheck::Partial) {
                        old_rule.push(format!(
                            "{name} (time {:x}{})",
                            b.time,
                            if blk.flags & flags::MAIN != 0 { format!(", main block {}", blk.height) } else { String::new() }
                        ));
                        continue;
                    }
                }
                settle(&mut f, &blk.hashlow, check);
            }
            Some(_) if opts.randomx_limit == 0 => skipped_rx += 1,
            Some(seed) => {
                deferred.push(Reverse(Deferred {
                    time: b.time,
                    hashlow: blk.hashlow,
                    block: b,
                    recorded: blk.difficulty,
                    recorded_link: blk.max_diff_link,
                    seed,
                }));
                if deferred.len() > opts.randomx_limit {
                    deferred.pop();
                    skipped_rx += 1;
                }
            }
        }
    }
    drop(r);

    // RandomX candidates, oldest first so that each seed is set up once
    let mut candidates: Vec<Deferred> = deferred.into_iter().map(|d| d.0).collect();
    candidates.sort_unstable();
    let (rx_checked, mut rx_seeds, mut last_seed) = (candidates.len(), 0u64, None);
    for (i, c) in candidates.into_iter().enumerate() {
        if last_seed != Some(c.seed.height) {
            last_seed = Some(c.seed.height);
            rx_seeds += 1;
        }
        if i % 1000 == 999 {
            tracing::info!("RandomX: {} of {rx_checked} candidates checked", i + 1);
        }
        let schedule = RxSchedule { fork_epoch: Some(0), seeds: vec![Seed { switch_epoch: 0, ..c.seed }] };
        let own = own_difficulty(&c.block, p, &schedule, pow);
        settle(&mut f, &c.hashlow, check_difficulty(&t, &c.block, c.recorded, c.recorded_link, own));
    }

    out.push(format!(
        "blocks with data: {with_data} parsed and hashed{}, {} wrong",
        if transport > 0 { format!(" ({transport} stored with a transport header)") } else { String::new() },
        f.count("block")
    ));
    f.list("block", &mut out);
    if !foreign_keys.is_empty() {
        out.push(format!("  {} blocks kept as key material do not hash to the block they stand for:", foreign_keys.len()));
        out.extend(foreign_keys.iter().take(SHOWN).map(|l| format!("    {l}")));
        if foreign_keys.len() > SHOWN {
            out.push(format!("    ... and {} more", foreign_keys.len() - SHOWN));
        }
    }
    out.push(format!(
        "  signatures: {self_signed} of the {keyed} blocks that name public keys are signed by one of them; {account_txs} account transactions, {} problems",
        f.count("signature")
    ));
    f.list("signature", &mut out);
    out.push(format!(
        "  difficulty: {full} recomputed from all their links, {partial} from their max-difficulty link, {} wrong; not checkable: {unrecorded} (none recorded), {unverifiable} (linked blocks not in the file), {unknown_pow} (seed blocks not in the file)",
        f.count("difficulty")
    ));
    f.list("difficulty", &mut out);
    if !old_rule.is_empty() {
        out.push(format!(
            "  {} transactions carry the difficulty of their hash, as xdagj scored them before it scored transactions as 1:",
            old_rule.len()
        ));
        out.extend(old_rule.iter().take(SHOWN).map(|l| format!("    {l}")));
        if old_rule.len() > SHOWN {
            out.push(format!("    ... and {} more", old_rule.len() - SHOWN));
        }
    }
    if history.forked {
        out.push(format!(
            "  of these, {rx_checked} main-block candidates were hashed with RandomX under {rx_seeds} seeds{}",
            if skipped_rx > 0 { format!(" ({skipped_rx} older candidates skipped)") } else { String::new() }
        ));
    }
    Ok(VerifyReport { lines: out, failures: f.failures })
}
