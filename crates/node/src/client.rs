//! `xdagd tx …` — sign transactions locally and submit them over JSON-RPC.

use std::io::{Read, Write};
use std::net::TcpStream;
use std::path::Path;
use std::time::Duration;

use anyhow::{anyhow, bail, Context, Result};
use serde_json::{json, Value};
use xdag_types::nova::NativeTransfer;
use xdag_types::{Address, KeyPair, Nano};

/// Minimal blocking JSON-RPC client (plain HTTP).
pub fn rpc(url: &str, method: &str, params: Value) -> Result<Value> {
    let rest = url.strip_prefix("http://").ok_or_else(|| anyhow!("only http:// RPC URLs are supported"))?;
    let (host, path) = match rest.find('/') {
        Some(i) => (&rest[..i], &rest[i..]),
        None => (rest, "/"),
    };
    let body = json!({"jsonrpc": "2.0", "id": 1, "method": method, "params": params}).to_string();
    let mut s = TcpStream::connect(host).with_context(|| format!("connect {host}"))?;
    s.set_read_timeout(Some(Duration::from_secs(60)))?;
    write!(
        s,
        "POST {path} HTTP/1.1\r\nHost: {host}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
        body.len()
    )?;
    let mut resp = String::new();
    s.read_to_string(&mut resp)?;
    let (head, payload) = resp.split_once("\r\n\r\n").ok_or_else(|| anyhow!("bad HTTP response"))?;
    let payload = if head.to_ascii_lowercase().contains("transfer-encoding: chunked") { dechunk(payload) } else { payload.to_string() };
    let v: Value = serde_json::from_str(&payload).with_context(|| format!("bad JSON: {payload}"))?;
    if let Some(e) = v.get("error") {
        bail!("rpc error: {e}");
    }
    Ok(v["result"].clone())
}

fn dechunk(s: &str) -> String {
    let mut out = String::new();
    let mut rest = s;
    while let Some((len, tail)) = rest.split_once("\r\n") {
        let n = usize::from_str_radix(len.trim(), 16).unwrap_or(0);
        if n == 0 || tail.len() < n {
            break;
        }
        out.push_str(&tail[..n]);
        rest = tail[n..].trim_start_matches("\r\n");
    }
    out
}

pub fn load_key(spec: &str) -> Result<KeyPair> {
    let text = if Path::new(spec).exists() { std::fs::read_to_string(spec)? } else { spec.to_string() };
    let b = hex::decode(text.trim().trim_start_matches("0x")).context("key must be hex or a file containing hex")?;
    KeyPair::from_secret_bytes(&b).map_err(|e| anyhow!("{e}"))
}

/// What a transaction in xdagj's block format needs to know of the node it
/// is sent to: the network's parameters and a block time inside the epoch.
fn legacy_template(url: &str, remark: &str) -> Result<(xdag_types::NetworkParams, xdag_types::BlockTemplate)> {
    use xdag_types::{BlockTemplate, Network, NetworkParams};
    anyhow::ensure!(remark.len() <= 32, "remark longer than 32 bytes");
    let network = match rpc(url, "xdag_netType", json!([]))?.as_str() {
        Some("mainnet") => Network::Mainnet,
        Some("testnet") => Network::Testnet,
        Some("devnet") => Network::Devnet,
        other => bail!("unknown network type {other:?}"),
    };
    let params = NetworkParams::for_network(network);
    let now = xdag_types::time::now_xdag();
    let time = if params.epochs().is_end_of_epoch(now) { now - 1 } else { now };
    let mut t = BlockTemplate::new(params.header_field(), time);
    if !remark.is_empty() {
        let mut r = [0u8; 32];
        r[..remark.len()].copy_from_slice(remark.as_bytes());
        t.remark = Some(r);
    }
    Ok((params, t))
}

/// A legacy account transaction in xdagj's block format, for networks still
/// on xdagj's rules (any xdagj or xdagd node accepts it). The receiver gets
/// `amount`; the sender also pays the 0.1 XDAG output fee. No balance check
/// is made here: the network decides.
pub fn send_legacy(url: &str, key: &KeyPair, to: &str, amount: &str, nonce: Option<u64>, remark: &str) -> Result<String> {
    use xdag_types::{FieldType, LinkTarget};
    let to = Address::parse(to).map_err(|_| anyhow!("bad destination"))?;
    let amount = Nano::parse_xdag(amount).map_err(|e| anyhow!("amount: {e}"))?;
    let (params, mut t) = legacy_template(url, remark)?;
    let nonce = match nonce {
        Some(n) => n,
        None => rpc(url, "xdag_getTransactionNonce", json!([key.address().to_base58()]))?
            .as_str()
            .and_then(|s| s.parse().ok())
            .ok_or_else(|| anyhow!("bad nonce"))?,
    };
    let total = amount.checked_add(params.min_gas).ok_or_else(|| anyhow!("amount overflow"))?.to_camount_legacy();
    t.tx_nonce = Some(nonce);
    t.links.push((FieldType::Input, LinkTarget::Address(key.address()), total));
    t.links.push((FieldType::Output, LinkTarget::Address(to), total));
    t.sign_out = Some(key.clone());
    t.include_out_pubkey = true;
    let block = t.build().map_err(|e| anyhow!("{e}"))?;
    let r = rpc(url, "xdag_sendRawTransaction", json!([hex::encode(block.raw())]))?;
    Ok(format!("{}  (block {}, nonce {nonce})", r.as_str().unwrap_or_default(), block.hashlow().to_legacy_address()))
}

/// Spend the balance of a block — the reward of a block this key mined, or a
/// balance from before account addresses existed — in xdagj's block format
/// (what xdagj's `xfertonew` sends). `amount` leaves the block; the receiver
/// gets it less the 0.1 XDAG output fee. Only the key that signed the block
/// can spend it, and no check is made here: the network decides.
pub fn send_from_block(url: &str, key: &KeyPair, block: &str, to: &str, amount: &str, remark: &str) -> Result<String> {
    use xdag_types::{FieldType, HashLow, LinkTarget};
    let from = HashLow::from_legacy_address(block).map_err(|_| anyhow!("bad block address"))?;
    let to = Address::parse(to).map_err(|_| anyhow!("bad destination"))?;
    let amount = Nano::parse_xdag(amount).map_err(|e| anyhow!("amount: {e}"))?.to_camount_legacy();
    let (_, mut t) = legacy_template(url, remark)?;
    t.links.push((FieldType::In, LinkTarget::Block(from), amount));
    t.links.push((FieldType::Output, LinkTarget::Address(to), amount));
    t.sign_out = Some(key.clone());
    t.include_out_pubkey = true;
    let tx = t.build().map_err(|e| anyhow!("{e}"))?;
    let r = rpc(url, "xdag_sendRawTransaction", json!([hex::encode(tx.raw())]))?;
    Ok(format!("{}  (block {})", r.as_str().unwrap_or_default(), tx.hashlow().to_legacy_address()))
}

fn chain_info(url: &str) -> Result<(u64, Nano)> {
    let info = rpc(url, "xdag_getChainInfo", json!([]))?;
    let chain_id = info["chainId"].as_u64().ok_or_else(|| anyhow!("this network has no Nova parameters"))?;
    let fee = info["minNativeFee"].as_str().and_then(|s| Nano::parse_xdag(s).ok()).unwrap_or(Nano::from_milli(100));
    Ok((chain_id, fee))
}

pub fn send_native(url: &str, key: &KeyPair, to: &str, amount: &str, fee: Option<&str>, remark: &str) -> Result<String> {
    let to = Address::parse(to).map_err(|_| anyhow!("bad destination"))?;
    let amount = Nano::parse_xdag(amount).map_err(|e| anyhow!("amount: {e}"))?;
    let (chain_id, default_fee) = chain_info(url)?;
    let fee = match fee {
        Some(f) => Nano::parse_xdag(f).map_err(|e| anyhow!("fee: {e}"))?,
        None => default_fee,
    };
    let nonce: u64 = rpc(url, "xdag_getTransactionNonce", json!([key.address().to_base58()]))?
        .as_str()
        .and_then(|s| s.parse().ok())
        .ok_or_else(|| anyhow!("bad nonce"))?;
    let tx = NativeTransfer::new_signed(key, chain_id, nonce, to, amount, fee, remark.as_bytes()).map_err(|e| anyhow!("{e}"))?;
    let h = rpc(url, "xdag_sendNovaTransaction", json!([hex::encode(tx.encode())]))?;
    Ok(h.as_str().unwrap_or_default().to_string())
}

pub fn send_evm(url: &str, key: &KeyPair, to: Option<&str>, value_wei: u128, data: &str, gas: u64) -> Result<String> {
    let (chain_id, _) = chain_info(url)?;
    let to = to.map(|t| Address::parse(t).map_err(|_| anyhow!("bad destination"))).transpose()?;
    let data = hex::decode(data.trim_start_matches("0x")).context("data must be hex")?;
    let nonce_hex = rpc(url, "eth_getTransactionCount", json!([key.evm_address().to_hex(), "pending"]))?;
    let nonce = u64::from_str_radix(nonce_hex.as_str().unwrap_or("0x0").trim_start_matches("0x"), 16)?;
    let price_hex = rpc(url, "eth_gasPrice", json!([]))?;
    let price = u128::from_str_radix(price_hex.as_str().unwrap_or("0x0").trim_start_matches("0x"), 16)?;
    let raw = xdag_evm::tx::sign_eip1559(key, chain_id, nonce, price, price * 2, gas, to, value_wei, &data);
    let h = rpc(url, "eth_sendRawTransaction", json!([format!("0x{}", hex::encode(raw))]))?;
    Ok(h.as_str().unwrap_or_default().to_string())
}
