//! xdagj-compatible `xdag_*` methods (response shapes follow xdagj 0.8.4).

use serde_json::{json, Value};
use xdag_chain::query::{self, BlockView};
use xdag_chain::records::{flags, HistoryEntry};
use xdag_types::{Address, FieldType, HashLow, Nano};

use crate::{param, param_str, Backend, RpcError, RpcResult};

const DEFAULT_PAGE_SIZE: usize = 100;
const MAX_PAGE_SIZE: usize = 500;

fn xdag_amount(n: Nano) -> String {
    n.to_xdag_string()
}

fn signed_amount(v: i64) -> String {
    if v < 0 {
        format!("-{}", Nano(v.unsigned_abs()).to_xdag_string())
    } else {
        Nano(v as u64).to_xdag_string()
    }
}

/// xdagj `Commands.getStateByFlags`.
pub fn state_by_flags(f: u8) -> &'static str {
    let f = f & !(flags::OURS | flags::REMARK);
    if f == flags::REF | flags::MAIN_REF | flags::APPLIED | flags::MAIN | flags::MAIN_CHAIN {
        "Main"
    } else if f == flags::REF | flags::MAIN_REF | flags::APPLIED {
        "Accepted"
    } else if f == flags::REF | flags::MAIN_REF {
        "Rejected"
    } else {
        "Pending"
    }
}

/// Accepts a Base58 address, a 0x address, a 32-char legacy block address or
/// a 64-char block hash (xdagj forms).
pub enum Target {
    Account(Address),
    Block(HashLow),
}

pub fn parse_target(s: &str) -> Result<Target, RpcError> {
    let s = s.trim();
    if let Ok(a) = Address::from_base58(s) {
        return Ok(Target::Account(a));
    }
    if s.len() == 42 && s.starts_with("0x") {
        return Address::from_hex(s).map(Target::Account).map_err(|_| RpcError::invalid("bad address"));
    }
    if s.len() == 32 {
        return HashLow::from_legacy_address(s).map(Target::Block).map_err(|_| RpcError::invalid("bad block address"));
    }
    HashLow::from_xdagj_hex(s).map(Target::Block).map_err(|_| RpcError::invalid("bad hash or address"))
}

fn block_type(v: &BlockView) -> &'static str {
    if v.info.snapshot.is_some() {
        return "Snapshot";
    }
    if state_by_flags(v.flags()) == "Main" {
        return "Main";
    }
    match &v.block {
        Some(b) if b.insigs.is_empty() && b.inputs.is_empty() && b.outputs.is_empty() => "Wallet",
        _ => "Transaction",
    }
}

fn history_json(e: &HistoryEntry) -> Value {
    let address = match e.counterparty.as_deref() {
        Some(cp) if cp.len() == 20 => Address::from_slice(cp).unwrap().to_base58(),
        Some(cp) if cp.len() == 24 => HashLow::from_slice(cp).unwrap().to_legacy_address(),
        _ => String::new(),
    };
    let (hashlow, tx_address) = if e.tx.len() == 24 {
        let h = HashLow::from_slice(&e.tx).unwrap();
        (h.to_xdagj_hex(), h.to_legacy_address())
    } else {
        (format!("0x{}", hex::encode(&e.tx)), format!("0x{}", hex::encode(&e.tx)))
    };
    json!({
        "direction": e.direction as u8,
        "hashlow": hashlow,
        "address": tx_address,
        "counterparty": address,
        "amount": xdag_amount(e.amount),
        "time": xdag_types::time::xdag_to_ms(e.time),
        "remark": String::from_utf8_lossy(&e.remark).trim_matches(char::from(0)).to_string(),
        "height": e.main_height,
        "status": e.status.name(),
    })
}

fn paged_history(b: &dyn Backend, t: &Target, page: usize, page_size: usize, range: Option<(u64, u64)>) -> Result<(Vec<Value>, usize), RpcError> {
    let all = match t {
        Target::Account(a) => query::address_history(b.db(), a, None, 100_000)?,
        Target::Block(h) => query::block_history(b.db(), h, None, 100_000)?,
    };
    let filtered: Vec<&HistoryEntry> = all
        .iter()
        .filter(|e| match range {
            Some((s, end)) => {
                let ms = xdag_types::time::xdag_to_ms(e.time);
                ms >= s && ms <= end
            }
            None => true,
        })
        .collect();
    let total_pages = filtered.len().div_ceil(page_size).max(1);
    let start = (page.max(1) - 1) * page_size;
    Ok((filtered.iter().skip(start).take(page_size).map(|e| history_json(e)).collect(), total_pages))
}

fn links_json(b: &dyn Backend, v: &BlockView) -> Vec<Value> {
    let mut links = vec![];
    let fee_amount = if v.state.ref_.is_some() { xdag_amount(v.state.fee) } else { "0.000000000".into() };
    links.push(json!({
        "direction": 2,
        "address": v.state.ref_.map(|r| r.to_legacy_address()).unwrap_or_else(|| "A".repeat(32)),
        "hashlow": v.state.ref_.map(|r| r.to_xdagj_hex()).unwrap_or_else(|| "A".repeat(32)),
        "amount": fee_amount,
    }));
    let Some(blk) = &v.block else { return links };
    let limit = if blk.is_tx() { xdag_chain::fees::output_limit(blk, b.params()).unwrap_or(Nano::ZERO) } else { Nano::ZERO };
    for l in blk.inputs.iter().chain(blk.outputs.iter()) {
        if l.kind == FieldType::Coinbase {
            continue;
        }
        let input = matches!(l.kind, FieldType::In | FieldType::Input);
        let mut amount = l.amount.to_nano_legacy().unwrap_or(Nano::ZERO);
        if !input && !blk.inputs.is_empty() {
            amount = amount.saturating_sub(limit);
        }
        let (address, hashlow) = match l.target {
            xdag_types::LinkTarget::Address(a) => (a.to_base58(), a.to_hex()),
            xdag_types::LinkTarget::Block(h) => (h.to_legacy_address(), h.to_xdagj_hex()),
        };
        links.push(json!({"direction": if input { 0 } else { 1 }, "address": address, "hashlow": hashlow, "amount": xdag_amount(amount)}));
    }
    links
}

fn block_json(b: &dyn Backend, v: &BlockView, page: usize, page_size: usize, range: Option<(u64, u64)>, brief: bool) -> Result<Value, RpcError> {
    let remark = v.info.remark.map(|r| String::from_utf8_lossy(&r).trim_matches(char::from(0)).trim().to_string()).unwrap_or_default();
    let mut o = json!({
        "height": v.state.height,
        "balance": signed_amount(v.state.amount),
        "blockTime": xdag_types::time::xdag_to_ms(v.info.time),
        "timeStamp": v.info.time,
        "state": state_by_flags(v.flags()),
        "hash": v.info.hash.to_xdagj_hex(),
        "address": v.hashlow.to_legacy_address(),
        "remark": remark,
        "diff": xdag_types::difficulty::to_quantity_hex(v.info.difficulty),
        "type": block_type(v),
        "flags": format!("{:x}", v.flags()),
    });
    if let (Some(pl), Some(n)) = (&v.payload, (v.info.payload_txs > 0).then_some(v.info.payload_txs)) {
        o["novaTxs"] = json!(n);
        o["payloadBytes"] = json!(pl.len());
    }
    if brief {
        return Ok(o);
    }
    o["refs"] = json!(links_json(b, v));
    if page != 0 {
        let (txs, total) = paged_history(b, &Target::Block(v.hashlow), page, page_size, range)?;
        let mut txs = txs;
        if state_by_flags(v.flags()) == "Main" {
            let reward = xdag_chain::fees::reward(v.state.height, b.params());
            txs.insert(
                0,
                json!({"direction": 2, "hashlow": v.hashlow.to_xdagj_hex(), "address": v.hashlow.to_legacy_address(),
                       "amount": xdag_amount(Nano(reward.0 + v.state.fee.0)), "time": xdag_types::time::xdag_to_ms(v.info.time), "remark": remark}),
            );
        }
        o["transactions"] = json!(txs);
        o["totalPage"] = json!(total);
    }
    Ok(o)
}

fn account_json(b: &dyn Backend, a: &Address, page: usize, page_size: usize, range: Option<(u64, u64)>) -> Result<Value, RpcError> {
    let bal = query::balance(b.db(), a)?;
    let mut o = json!({
        "address": a.to_base58(),
        "hash": Value::Null,
        "balance": xdag_amount(bal),
        "type": "Wallet",
        "state": "Accepted",
        "blockTime": 0,
        "timeStamp": 0,
    });
    if page != 0 {
        let (txs, total) = paged_history(b, &Target::Account(*a), page, page_size, range)?;
        o["transactions"] = json!(txs);
        o["totalPage"] = json!(total);
    }
    Ok(o)
}

fn parse_page_args(params: &Value) -> (usize, usize, Option<(u64, u64)>) {
    let num = |i: usize| param(params, i).and_then(|v| v.as_str().map(|s| s.to_string()).or_else(|| v.as_u64().map(|n| n.to_string())));
    let page = num(1).and_then(|s| s.parse().ok()).unwrap_or(1usize);
    let n = match params {
        Value::Array(a) => a.len(),
        _ => 0,
    };
    let (size, range) = match n {
        3 => (num(2).and_then(|s| s.parse().ok()).unwrap_or(DEFAULT_PAGE_SIZE), None),
        4 | 5 => {
            let s = num(2).and_then(|s| s.parse().ok()).unwrap_or(0);
            let e = num(3).and_then(|s| s.parse().ok()).unwrap_or(u64::MAX);
            let size = if n == 5 { num(4).and_then(|s| s.parse().ok()).unwrap_or(DEFAULT_PAGE_SIZE) } else { DEFAULT_PAGE_SIZE };
            (size, Some((s, e)))
        }
        _ => (DEFAULT_PAGE_SIZE, None),
    };
    (page, size.clamp(1, MAX_PAGE_SIZE), range)
}

fn get_by_hash(b: &dyn Backend, params: &Value) -> RpcResult {
    let (page, size, range) = parse_page_args(params);
    match parse_target(&param_str(params, 0)?)? {
        Target::Account(a) => account_json(b, &a, page, size, range),
        Target::Block(h) => match query::block_view(b.db(), &h)? {
            Some(v) => block_json(b, &v, page, size, range, false),
            None => Ok(Value::Null),
        },
    }
}

pub fn handle(b: &dyn Backend, method: &str, params: &Value) -> RpcResult {
    let p = b.params();
    match method {
        "xdag_blockNumber" => Ok(json!(b.status().nmain.to_string())),
        "xdag_protocolVersion" => Ok(json!(env!("CARGO_PKG_VERSION"))),
        "xdag_netType" => Ok(json!(p.network.name())),
        "xdag_coinbase" => Ok(json!(b.coinbase().map(|a| a.to_base58()))),
        "xdag_getBalance" => match parse_target(&param_str(params, 0)?)? {
            Target::Account(a) => Ok(json!(xdag_amount(query::balance(b.db(), &a)?))),
            Target::Block(h) => {
                let v = query::block_view(b.db(), &h)?.ok_or_else(|| RpcError::exec("block not found"))?;
                Ok(json!(signed_amount(v.state.amount)))
            }
        },
        "xdag_getTotalBalance" => {
            let total = b.coinbase().map(|a| query::balance(b.db(), &a)).transpose()?.unwrap_or(Nano::ZERO);
            Ok(json!(xdag_amount(total)))
        }
        "xdag_getTransactionNonce" => {
            let a = Address::parse(&param_str(params, 0)?).map_err(|_| RpcError::invalid("bad address"))?;
            Ok(json!(b.next_nonce(&a, false).to_string()))
        }
        "xdag_getRewardByNumber" => {
            let n: u64 = param_str(params, 0)?.parse().map_err(|_| RpcError::invalid("bad number"))?;
            Ok(json!(xdag_amount(xdag_chain::fees::reward(n, p))))
        }
        "xdag_getBalanceByNumber" => {
            let n: u64 = param_str(params, 0)?.parse().map_err(|_| RpcError::invalid("bad number"))?;
            let Some(h) = query::main_hashlow(b.db(), n)? else { return Ok(Value::Null) };
            let v = query::block_view(b.db(), &h)?.ok_or_else(|| RpcError::internal("main block missing"))?;
            Ok(json!(signed_amount(v.state.amount)))
        }
        "xdag_getStatus" => {
            let s = b.status();
            let supply = xdag_chain::fees::supply(s.nmain, p);
            Ok(json!({
                "nblock": s.nblocks.to_string(),
                "totalNblocks": s.nblocks.to_string(),
                "nmain": s.nmain.to_string(),
                "totalNmain": s.nmain.to_string(),
                "curDiff": xdag_types::difficulty::to_quantity_hex(s.top_diff),
                "netDiff": xdag_types::difficulty::to_quantity_hex(s.top_diff),
                "hashRateOurs": "0.0",
                "hashRateTotal": "0.0",
                "ourSupply": xdag_amount(supply),
                "netSupply": xdag_amount(supply),
                "synced": s.synced,
                "extraBlocks": s.extra,
                "pendingTxs": s.pool_txs,
            }))
        }
        "xdag_getBlockByHash" | "xdag_getTransactionByHash" => get_by_hash(b, params),
        "xdag_getBlockByNumber" => {
            let n: u64 = param_str(params, 0)?.parse().map_err(|_| RpcError::invalid("bad number"))?;
            let (page, size, range) = parse_page_args(params);
            let Some(h) = query::main_hashlow(b.db(), n)? else { return Ok(Value::Null) };
            match query::block_view(b.db(), &h)? {
                Some(v) => block_json(b, &v, page, size, range, false),
                None => Ok(Value::Null),
            }
        }
        "xdag_getBlocksByNumber" => {
            let n: usize = param(params, 0).and_then(|v| v.as_str().and_then(|s| s.parse().ok()).or(v.as_u64().map(|x| x as usize))).unwrap_or(20);
            let mut out = vec![];
            for (_, h) in query::main_blocks(b.db(), n.min(1000))? {
                if let Some(v) = query::block_view(b.db(), &h)? {
                    out.push(block_json(b, &v, 0, 0, None, true)?);
                }
            }
            Ok(json!(out))
        }
        "xdag_sendRawTransaction" => {
            let hexs = param_str(params, 0)?;
            let raw = hex::decode(hexs.trim_start_matches("0x")).map_err(|_| RpcError::invalid("bad hex"))?;
            // xdagj answers with strings, not JSON-RPC errors
            Ok(match b.submit_block(raw) {
                Ok(h) => json!(h.to_legacy_address()),
                Err(e) => json!(format!("INVALID_BLOCK {e}")),
            })
        }
        "xdag_sendNovaTransaction" => {
            let hexs = param_str(params, 0)?;
            let raw = hex::decode(hexs.trim_start_matches("0x")).map_err(|_| RpcError::invalid("bad hex"))?;
            let tx = xdag_types::nova::NativeTransfer::decode(&raw).map_err(|e| RpcError::invalid(e.to_string()))?;
            let h = b.submit_nova_tx(xdag_types::nova::NovaTx::Native(tx)).map_err(RpcError::exec)?;
            Ok(json!(crate::hex0x(&h)))
        }
        "xdag_getNovaTransaction" => {
            let hexs = param_str(params, 0)?;
            let h = hex::decode(hexs.trim_start_matches("0x")).map_err(|_| RpcError::invalid("bad hash"))?;
            match query::tx_location(b.db(), &h)? {
                Some(l) => Ok(json!({
                    "block": l.block.to_xdagj_hex(),
                    "index": l.index,
                    "height": l.main_height,
                    "status": l.status.name(),
                    "sender": l.sender.to_base58(),
                    "fee": xdag_amount(l.fee),
                    "gasUsed": l.gas_used,
                })),
                None => Ok(Value::Null),
            }
        }
        "xdag_getHistory" => {
            let t = parse_target(&param_str(params, 0)?)?;
            // cursor: "height.seq" from a previous page's last entry, or a bare height
            let before = match param(params, 1) {
                None | Some(Value::Null) => None,
                Some(v) => Some(
                    v.as_u64()
                        .map(|height| query::HistoryCursor { height, seq: 0 })
                        .or_else(|| v.as_str().and_then(|s| s.parse().ok()))
                        .ok_or_else(|| RpcError::invalid("bad cursor"))?,
                ),
            };
            let limit = param(params, 2).and_then(|v| v.as_u64()).unwrap_or(100).clamp(1, 1000) as usize;
            let entries = match t {
                Target::Account(a) => query::address_history_page(b.db(), &a, before, limit)?,
                Target::Block(h) => query::block_history_page(b.db(), &h, before, limit)?,
            };
            Ok(json!(entries
                .iter()
                .map(|(c, e)| {
                    let mut j = history_json(e);
                    j["cursor"] = json!(c.to_string());
                    j
                })
                .collect::<Vec<_>>()))
        }
        "xdag_getPendingTransactions" => Ok(json!(b
            .pending()
            .iter()
            .map(|(h, s, n, inflight)| json!({"hash": crate::hex0x(h), "sender": s.to_base58(), "nonce": n, "inFlight": inflight}))
            .collect::<Vec<_>>())),
        "xdag_getAverageFee" => Ok(json!(format!("{:.2}", p.min_gas.0 as f64 / 1e9))),
        "xdag_syncing" => {
            let s = b.status();
            Ok(json!({"currentBlock": s.nmain.to_string(), "highestBlock": s.nmain.to_string(), "isSyncDone": s.synced}))
        }
        "xdag_netConnectionList" | "xdag_getPeers" => {
            let peers = b.peers();
            Ok(json!(peers
                .iter()
                .map(|p| json!({"nodeAddress": p.addr, "peerId": p.peer_id, "connectTime": p.connected_secs,
                    "inBound": if p.inbound { 1 } else { 0 }, "outBound": if p.inbound { 0 } else { 1 },
                    "client": p.client_id, "nova": p.nova, "score": p.score}))
                .collect::<Vec<_>>()))
        }
        "xdag_getChainInfo" => Ok(json!({
            "network": p.network.name(),
            "novaActivationEpoch": p.nova.as_ref().map(|n| n.activation_epoch),
            "chainId": p.nova.as_ref().map(|n| n.chain_id),
            "epochSeconds": (1u64 << p.epoch_bits) / 1024,
            "minGas": xdag_amount(p.min_gas),
            "minNativeFee": p.nova.as_ref().map(|n| xdag_amount(n.min_native_fee)),
            "minGasPrice": p.nova.as_ref().map(|n| n.min_gas_price.to_string()),
            "client": b.client_version(),
        })),
        "xdag_personal_sendTransaction" | "xdag_personal_sendSafeTransaction" => {
            let req = param(params, 0).cloned().unwrap_or(Value::Null);
            let pass = param_str(params, 1).unwrap_or_default();
            let to = req.get("to").and_then(|v| v.as_str()).ok_or_else(|| RpcError::invalid("missing to"))?;
            let value = req.get("value").and_then(|v| v.as_str()).ok_or_else(|| RpcError::invalid("missing value"))?;
            let remark = req.get("remark").and_then(|v| v.as_str()).unwrap_or("");
            let from = req.get("from").and_then(|v| v.as_str()).filter(|s| !s.is_empty());
            let to = Address::parse(to).map_err(|_| RpcError::invalid("To address is illegal"))?;
            // exact decimal parsing (xdagj rounded amounts to 0.01 XDAG through a double)
            let amount = Nano::parse_xdag(value).map_err(|_| RpcError::invalid("The transfer amount is invalid"))?;
            let from = from.map(Address::parse).transpose().map_err(|_| RpcError::invalid("From address is illegal"))?;
            Ok(match b.wallet_transfer(from, to, amount, remark, &pass) {
                Ok(res) => json!({"code": 0, "result": res, "errMsg": Value::Null}),
                Err(e) => json!({"code": -10200, "result": Value::Null, "errMsg": e}),
            })
        }
        _ => Err(RpcError::not_found(method)),
    }
}
