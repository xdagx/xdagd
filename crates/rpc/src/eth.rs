//! Ethereum JSON-RPC subset. An XDAG main block at height `h` is presented as
//! Ethereum block number `h`; its transactions are the EVM transactions that
//! main block executed.

use serde_json::{json, Value};
use xdag_chain::query;
use xdag_types::nova::NovaTx;
use xdag_types::{Address, BlockHash, HashLow};

use crate::{hex0x, param, param_str, qty, Backend, RpcError, RpcResult};

const EMPTY_UNCLES: &str = "0x1dcc4de8dec75d7aab85b567b6ccd41ad312451b948a7413f0a142fd40d49347";
const EMPTY_TRIE: &str = "0x56e81f171bcc55a6ff8345e692c0f86e5b48e01b996cadc001622fb5e363b421";

fn zero_bloom() -> String {
    format!("0x{}", "0".repeat(512))
}

fn parse_u64(v: &Value) -> Option<u64> {
    match v {
        Value::String(s) if s.starts_with("0x") => u64::from_str_radix(&s[2..], 16).ok(),
        Value::String(s) => s.parse().ok(),
        Value::Number(n) => n.as_u64(),
        _ => None,
    }
}

fn parse_u128(v: &Value) -> Option<u128> {
    match v {
        Value::String(s) if s.starts_with("0x") => u128::from_str_radix(&s[2..], 16).ok(),
        Value::String(s) => s.parse().ok(),
        Value::Number(n) => n.as_u64().map(|x| x as u128),
        _ => None,
    }
}

fn parse_addr(v: Option<&Value>) -> Result<Option<Address>, RpcError> {
    match v {
        None | Some(Value::Null) => Ok(None),
        Some(Value::String(s)) => Address::parse(s).map(Some).map_err(|_| RpcError::invalid("bad address")),
        _ => Err(RpcError::invalid("bad address")),
    }
}

fn parse_data(v: Option<&Value>) -> Result<Vec<u8>, RpcError> {
    match v {
        None | Some(Value::Null) => Ok(vec![]),
        Some(Value::String(s)) => hex::decode(s.trim_start_matches("0x")).map_err(|_| RpcError::invalid("bad hex data")),
        _ => Err(RpcError::invalid("bad data")),
    }
}

fn block_number(b: &dyn Backend, v: Option<&Value>) -> Result<u64, RpcError> {
    let latest = b.status().nmain;
    match v {
        None | Some(Value::Null) => Ok(latest),
        Some(Value::String(s)) if matches!(s.as_str(), "latest" | "pending" | "safe" | "finalized") => Ok(latest),
        Some(Value::String(s)) if s == "earliest" => Ok(0),
        Some(v) => parse_u64(v).ok_or_else(|| RpcError::invalid("bad block number")),
    }
}

fn tx_object(b: &dyn Backend, hash: &[u8; 32]) -> Result<Option<Value>, RpcError> {
    let Some(loc) = query::tx_location(b.db(), hash)? else { return Ok(None) };
    let Some(NovaTx::Evm(raw)) = query::payload_tx(b.db(), &loc.block, loc.index)? else { return Ok(None) };
    let tx = xdag_evm::tx::decode(&raw).map_err(RpcError::internal)?;
    let (block_hash, idx) = eth_block_position(b, loc.main_height, hash)?;
    Ok(Some(json!({
        "hash": hex0x(hash),
        "nonce": qty(tx.nonce),
        "blockHash": block_hash,
        "blockNumber": qty(loc.main_height),
        "transactionIndex": qty(idx as u64),
        "from": tx.sender.to_hex(),
        "to": tx.to.map(|a| a.to_hex()),
        "value": qty_bytes(&tx.value),
        "gas": qty(tx.gas_limit),
        "gasPrice": qty(tx.effective_gas_price(0)),
        "maxFeePerGas": tx.max_priority_fee_per_gas.map(|_| qty(tx.max_fee_per_gas)),
        "maxPriorityFeePerGas": tx.max_priority_fee_per_gas.map(qty),
        "input": hex0x(&tx.data),
        "type": qty(tx.tx_type as u64),
        "chainId": qty(tx.chain_id),
        "v": "0x0", "r": "0x0", "s": "0x0",
    })))
}

/// Big-endian bytes as an Ethereum quantity (no leading zeros).
fn qty_bytes(b: &[u8]) -> String {
    let h = hex::encode(b);
    let t = h.trim_start_matches('0');
    if t.is_empty() {
        "0x0".into()
    } else {
        format!("0x{t}")
    }
}

fn eth_block_position(b: &dyn Backend, height: u64, hash: &[u8; 32]) -> Result<(String, usize), RpcError> {
    let block_hash = main_hash(b, height)?.unwrap_or_default();
    let txs = query::evm_txs_at(b.db(), height)?;
    Ok((block_hash, txs.iter().position(|h| h == hash).unwrap_or(0)))
}

fn main_hash(b: &dyn Backend, height: u64) -> Result<Option<String>, RpcError> {
    let Some(h) = query::main_hashlow(b.db(), height)? else { return Ok(None) };
    Ok(query::block_view(b.db(), &h)?.map(|v| format!("0x{}", v.info.hash.to_xdagj_hex())))
}

fn receipt_json(b: &dyn Backend, hash: &[u8; 32]) -> RpcResult {
    let Some(r) = query::receipt(b.db(), hash)? else { return Ok(Value::Null) };
    let loc = query::tx_location(b.db(), hash)?.ok_or_else(|| RpcError::internal("receipt without location"))?;
    let tx = match query::payload_tx(b.db(), &loc.block, loc.index)? {
        Some(NovaTx::Evm(raw)) => xdag_evm::tx::decode(&raw).ok(),
        _ => None,
    };
    let (block_hash, idx) = eth_block_position(b, r.height, hash)?;
    let logs: Vec<Value> = r
        .logs
        .iter()
        .enumerate()
        .map(|(i, l)| {
            json!({
                "address": l.address.to_hex(),
                "topics": l.topics.iter().map(|t| hex0x(t)).collect::<Vec<_>>(),
                "data": hex0x(&l.data),
                "blockNumber": qty(r.height),
                "blockHash": block_hash,
                "transactionHash": hex0x(hash),
                "transactionIndex": qty(idx as u64),
                "logIndex": qty(i as u64),
                "removed": false,
            })
        })
        .collect();
    let price = if r.gas_used > 0 { r.fee / r.gas_used as u128 } else { 0 };
    Ok(json!({
        "transactionHash": hex0x(hash),
        "transactionIndex": qty(idx as u64),
        "blockHash": block_hash,
        "blockNumber": qty(r.height),
        "from": loc.sender.to_hex(),
        "to": tx.as_ref().and_then(|t| t.to).map(|a| a.to_hex()),
        "cumulativeGasUsed": qty(r.gas_used),
        "gasUsed": qty(r.gas_used),
        "effectiveGasPrice": qty(price),
        "contractAddress": r.contract_address.map(|a| a.to_hex()),
        "logs": logs,
        "logsBloom": zero_bloom(),
        "status": if r.success { "0x1" } else { "0x0" },
        "type": qty(tx.map(|t| t.tx_type as u64).unwrap_or(0)),
    }))
}

fn block_json(b: &dyn Backend, height: u64, full: bool) -> RpcResult {
    let Some(h) = query::main_hashlow(b.db(), height)? else { return Ok(Value::Null) };
    let Some(v) = query::block_view(b.db(), &h)? else { return Ok(Value::Null) };
    let parent = if height > 1 { main_hash(b, height - 1)? } else { None };
    let txs = query::evm_txs_at(b.db(), height)?;
    let mut gas_used = 0u64;
    for t in &txs {
        if let Some(r) = query::receipt(b.db(), t)? {
            gas_used += r.gas_used;
        }
    }
    let tx_list: Vec<Value> =
        if full { txs.iter().filter_map(|t| tx_object(b, t).ok().flatten()).collect() } else { txs.iter().map(|t| json!(hex0x(t))).collect() };
    let coinbase = v.block.as_ref().and_then(|bl| bl.coinbase).unwrap_or(Address::ZERO);
    let nova = b.params().nova.as_ref();
    Ok(json!({
        "number": qty(height),
        "hash": format!("0x{}", v.info.hash.to_xdagj_hex()),
        "parentHash": parent.unwrap_or_else(|| format!("0x{}", "0".repeat(64))),
        "nonce": "0x0000000000000000",
        "sha3Uncles": EMPTY_UNCLES,
        "logsBloom": zero_bloom(),
        "transactionsRoot": EMPTY_TRIE,
        "stateRoot": format!("0x{}", "0".repeat(64)),
        "receiptsRoot": EMPTY_TRIE,
        "miner": coinbase.to_hex(),
        "difficulty": "0x0",
        "totalDifficulty": xdag_types::difficulty::to_quantity_hex(v.info.difficulty),
        "extraData": "0x",
        "size": qty(512u64),
        "gasLimit": qty(nova.map(|n| n.main_gas_limit).unwrap_or(0)),
        "gasUsed": qty(gas_used),
        "timestamp": qty(xdag_types::time::xdag_to_ms(v.info.time) / 1000),
        "transactions": tx_list,
        "uncles": [],
        "baseFeePerGas": "0x0",
        "mixHash": format!("0x{}", hex::encode(v.info.hash.0)),
    }))
}

fn call_args(b: &dyn Backend, params: &Value) -> Result<(Address, Option<Address>, Vec<u8>, u128, Option<u64>), RpcError> {
    let o = param(params, 0).ok_or_else(|| RpcError::invalid("missing call object"))?;
    let from = parse_addr(o.get("from"))?.unwrap_or(Address::ZERO);
    let to = parse_addr(o.get("to"))?;
    let data = match o.get("input").or_else(|| o.get("data")) {
        Some(v) => parse_data(Some(v))?,
        None => vec![],
    };
    let value = o.get("value").and_then(parse_u128).unwrap_or(0);
    let gas = o.get("gas").and_then(parse_u64);
    let _ = b;
    Ok((from, to, data, value, gas))
}

fn revert_error(out: &[u8]) -> RpcError {
    RpcError { code: 3, message: format!("execution reverted: 0x{}", hex::encode(out)) }
}

pub fn handle(b: &dyn Backend, method: &str, params: &Value) -> RpcResult {
    let p = b.params();
    let nova = p.nova.as_ref();
    let chain_id = nova.map(|n| n.chain_id).unwrap_or(0);
    let min_price = nova.map(|n| n.min_gas_price).unwrap_or(0);
    match method {
        "web3_clientVersion" => Ok(json!(b.client_version())),
        "web3_sha3" => {
            let d = parse_data(param(params, 0))?;
            Ok(json!(hex0x(&xdag_types::hash::keccak256(&d))))
        }
        "net_version" => Ok(json!(chain_id.to_string())),
        "net_listening" => Ok(json!(true)),
        "net_peerCount" => Ok(json!(qty(b.peers().len() as u64))),
        "eth_chainId" => Ok(json!(qty(chain_id))),
        "eth_blockNumber" => Ok(json!(qty(b.status().nmain))),
        "eth_syncing" => {
            let s = b.status();
            if s.synced {
                Ok(json!(false))
            } else {
                Ok(json!({"startingBlock": "0x0", "currentBlock": qty(s.nmain), "highestBlock": qty(s.nmain)}))
            }
        }
        "eth_accounts" => Ok(json!([])),
        "eth_gasPrice" | "eth_maxPriorityFeePerGas" => Ok(json!(qty(min_price))),
        "eth_feeHistory" => {
            let n = param(params, 0).and_then(parse_u64).unwrap_or(1).clamp(1, 1024);
            let newest = block_number(b, param(params, 1))?;
            let pct = param(params, 2).and_then(|v| v.as_array().map(|a| a.len())).unwrap_or(0);
            let oldest = newest.saturating_sub(n - 1);
            let count = (newest - oldest + 1) as usize;
            Ok(json!({
                "oldestBlock": qty(oldest),
                "baseFeePerGas": vec!["0x0"; count + 1],
                "gasUsedRatio": vec![0.0; count],
                "reward": vec![vec![qty(min_price); pct]; count],
            }))
        }
        "eth_getBalance" => {
            let a = parse_addr(param(params, 0))?.ok_or_else(|| RpcError::invalid("missing address"))?;
            Ok(json!(qty(query::balance_wei(b.db(), &a)?)))
        }
        "eth_getTransactionCount" => {
            let a = parse_addr(param(params, 0))?.ok_or_else(|| RpcError::invalid("missing address"))?;
            let pending = matches!(param(params, 1), Some(Value::String(s)) if s == "pending");
            let n = if pending { b.next_nonce(&a, true) } else { query::account(b.db(), &a)?.map(|r| r.nonce).unwrap_or(0) };
            Ok(json!(qty(n)))
        }
        "eth_getCode" => {
            let a = parse_addr(param(params, 0))?.ok_or_else(|| RpcError::invalid("missing address"))?;
            let code = match query::account(b.db(), &a)?.and_then(|r| r.code_hash) {
                Some(h) => query::code(b.db(), &h)?,
                None => vec![],
            };
            Ok(json!(hex0x(&code)))
        }
        "eth_getStorageAt" => {
            let a = parse_addr(param(params, 0))?.ok_or_else(|| RpcError::invalid("missing address"))?;
            let s = param_str(params, 1)?;
            let v = u128::from_str_radix(s.trim_start_matches("0x"), 16).ok();
            let mut slot = [0u8; 32];
            match v {
                Some(n) => slot[16..].copy_from_slice(&n.to_be_bytes()),
                None => {
                    let d = hex::decode(format!("{:0>64}", s.trim_start_matches("0x"))).map_err(|_| RpcError::invalid("bad slot"))?;
                    slot.copy_from_slice(&d[d.len() - 32..]);
                }
            }
            Ok(json!(hex0x(&query::storage_at(b.db(), &a, &slot)?)))
        }
        "eth_call" => {
            let (from, to, data, value, gas) = call_args(b, params)?;
            let r = b.evm_call(from, to, data, value, gas).map_err(RpcError::exec)?;
            if r.success {
                Ok(json!(hex0x(&r.output)))
            } else {
                Err(revert_error(&r.output))
            }
        }
        "eth_estimateGas" => {
            let (from, to, data, value, gas) = call_args(b, params)?;
            let cap = gas.unwrap_or(nova.map(|n| n.batch_gas_limit).unwrap_or(30_000_000));
            let r = b.evm_call(from, to, data.clone(), value, Some(cap)).map_err(RpcError::exec)?;
            if !r.success {
                return Err(revert_error(&r.output));
            }
            // binary search the smallest gas limit that still succeeds
            let (mut lo, mut hi) = (r.gas_used.saturating_sub(1), cap);
            while lo + 1 < hi && hi - lo > hi / 64 {
                let mid = lo + (hi - lo) / 2;
                match b.evm_call(from, to, data.clone(), value, Some(mid)) {
                    Ok(x) if x.success => hi = mid,
                    _ => lo = mid,
                }
            }
            Ok(json!(qty(hi)))
        }
        "eth_sendRawTransaction" => {
            let raw = parse_data(param(params, 0))?;
            let h = b.submit_nova_tx(NovaTx::Evm(raw)).map_err(RpcError::exec)?;
            Ok(json!(hex0x(&h)))
        }
        "eth_getTransactionByHash" => {
            let d = parse_data(param(params, 0))?;
            let h: [u8; 32] = d.try_into().map_err(|_| RpcError::invalid("bad hash"))?;
            Ok(tx_object(b, &h)?.unwrap_or(Value::Null))
        }
        "eth_getTransactionReceipt" => {
            let d = parse_data(param(params, 0))?;
            let h: [u8; 32] = d.try_into().map_err(|_| RpcError::invalid("bad hash"))?;
            receipt_json(b, &h)
        }
        "eth_getBlockByNumber" => {
            let n = block_number(b, param(params, 0))?;
            let full = param(params, 1).and_then(|v| v.as_bool()).unwrap_or(false);
            block_json(b, n, full)
        }
        "eth_getBlockByHash" => {
            let s = param_str(params, 0)?;
            let h = BlockHash::from_xdagj_hex(&s).map_err(|_| RpcError::invalid("bad hash"))?;
            let full = param(params, 1).and_then(|v| v.as_bool()).unwrap_or(false);
            match query::block_view(b.db(), &h.hashlow())? {
                Some(v) if v.state.height > 0 => block_json(b, v.state.height, full),
                _ => Ok(Value::Null),
            }
        }
        "eth_getBlockTransactionCountByNumber" => {
            let n = block_number(b, param(params, 0))?;
            Ok(json!(qty(query::evm_txs_at(b.db(), n)?.len() as u64)))
        }
        "eth_getLogs" => {
            let f = param(params, 0).cloned().unwrap_or(json!({}));
            let latest = b.status().nmain;
            let from = block_number(b, f.get("fromBlock"))?.min(latest);
            let to = block_number(b, f.get("toBlock"))?.min(latest);
            if to >= from && to - from > 10_000 {
                return Err(RpcError::invalid("block range too large (max 10000)"));
            }
            let addrs: Vec<Address> = match f.get("address") {
                Some(Value::String(s)) => vec![Address::parse(s).map_err(|_| RpcError::invalid("bad address"))?],
                Some(Value::Array(a)) => a.iter().filter_map(|v| v.as_str()).filter_map(|s| Address::parse(s).ok()).collect(),
                _ => vec![],
            };
            let topics: Vec<Option<Vec<[u8; 32]>>> = match f.get("topics") {
                Some(Value::Array(t)) => t
                    .iter()
                    .map(|x| match x {
                        Value::String(s) => hex::decode(s.trim_start_matches("0x")).ok().and_then(|d| d.try_into().ok()).map(|a| vec![a]),
                        Value::Array(opts) => Some(
                            opts.iter()
                                .filter_map(|o| o.as_str())
                                .filter_map(|s| hex::decode(s.trim_start_matches("0x")).ok().and_then(|d| d.try_into().ok()))
                                .collect(),
                        ),
                        _ => None,
                    })
                    .collect(),
                _ => vec![],
            };
            let mut out = vec![];
            for h in from..=to.max(from) {
                if h > to {
                    break;
                }
                for t in query::evm_txs_at(b.db(), h)? {
                    let r = receipt_json(b, &t)?;
                    if let Some(logs) = r.get("logs").and_then(|l| l.as_array()) {
                        for l in logs {
                            let la = l["address"].as_str().and_then(|s| Address::parse(s).ok());
                            if !addrs.is_empty() && !la.map(|a| addrs.contains(&a)).unwrap_or(false) {
                                continue;
                            }
                            let lt: Vec<[u8; 32]> = l["topics"]
                                .as_array()
                                .map(|a| {
                                    a.iter()
                                        .filter_map(|x| x.as_str())
                                        .filter_map(|s| hex::decode(&s[2..]).ok())
                                        .filter_map(|d| d.try_into().ok())
                                        .collect()
                                })
                                .unwrap_or_default();
                            let ok = topics.iter().enumerate().all(|(i, want)| match want {
                                None => true,
                                Some(opts) => lt.get(i).map(|x| opts.contains(x)).unwrap_or(false),
                            });
                            if ok {
                                out.push(l.clone());
                            }
                        }
                    }
                }
            }
            Ok(json!(out))
        }
        _ => {
            let _ = HashLow::ZERO;
            Err(RpcError::not_found(method))
        }
    }
}
