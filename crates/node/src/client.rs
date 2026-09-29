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
