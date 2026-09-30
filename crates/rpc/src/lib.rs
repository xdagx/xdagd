//! JSON-RPC 2.0 over HTTP.
//!
//! * `xdag_*` — the xdagj API (same method names, parameters and response
//!   shapes) so wallets, explorers and pools keep working, plus new methods
//!   for full history and Nova transactions;
//! * `eth_*` / `net_*` / `web3_*` — the Ethereum API subset used by MetaMask,
//!   Hardhat, Foundry and ethers.js, mapping XDAG main blocks to Ethereum blocks.

pub mod eth;
pub mod xdag;

use std::net::SocketAddr;
use std::sync::Arc;

use axum::extract::State;
use axum::http::{HeaderValue, Method};
use axum::response::IntoResponse;
use axum::routing::post;
use axum::{Json, Router};
use serde_json::{json, Value};

use xdag_chain::evm_api::EvmExecResult;
use xdag_net::PeerInfo;
use xdag_storage::Db;
use xdag_types::nova::NovaTx;
use xdag_types::{Address, HashLow, NetworkParams};

/// Node status used by several methods.
#[derive(Clone, Debug, Default)]
pub struct Status {
    pub nmain: u64,
    pub nblocks: u64,
    pub top: Option<HashLow>,
    pub top_diff: xdag_types::Difficulty,
    pub synced: bool,
    pub extra: usize,
    pub pool_txs: usize,
    pub now: u64,
}

/// Everything the RPC layer needs from the node.
pub trait Backend: Send + Sync + 'static {
    fn params(&self) -> &NetworkParams;
    fn db(&self) -> &Db;
    fn status(&self) -> Status;
    /// Import and broadcast a raw 512-byte block (legacy transaction).
    fn submit_block(&self, raw: Vec<u8>) -> Result<HashLow, String>;
    /// Admit a Nova transaction to the pool and gossip it.
    fn submit_nova_tx(&self, tx: NovaTx) -> Result<[u8; 32], String>;
    /// Read-only EVM call against the latest state.
    fn evm_call(&self, from: Address, to: Option<Address>, data: Vec<u8>, value: u128, gas: Option<u64>) -> Result<EvmExecResult, String>;
    fn peers(&self) -> Vec<PeerInfo>;
    fn coinbase(&self) -> Option<Address>;
    /// Next usable nonce of a sender (executed + queued).
    fn next_nonce(&self, a: &Address, evm: bool) -> u64;
    /// Transfer from the node wallet (xdag_personal_sendTransaction).
    fn wallet_transfer(
        &self,
        from: Option<Address>,
        to: Address,
        amount: xdag_types::Nano,
        remark: &str,
        password: &str,
    ) -> Result<Vec<String>, String>;
    fn client_version(&self) -> String;
    /// (hash, sender, nonce, in flight) of pooled Nova transactions.
    fn pending(&self) -> Vec<([u8; 32], Address, u64, bool)>;
    /// A block by hash. Nodes also answer for the main-block candidates they
    /// still hold only in memory (as xdagj does).
    fn block_view(&self, h: &HashLow) -> Result<Option<xdag_chain::query::BlockView>, String> {
        xdag_chain::query::block_view(self.db(), h).map_err(|e| e.to_string())
    }
}

#[derive(Debug)]
pub struct RpcError {
    pub code: i64,
    pub message: String,
}

impl RpcError {
    pub fn invalid(m: impl Into<String>) -> Self {
        RpcError { code: -32602, message: m.into() }
    }
    pub fn internal(m: impl Into<String>) -> Self {
        RpcError { code: -32603, message: m.into() }
    }
    pub fn not_found(method: &str) -> Self {
        RpcError { code: -32601, message: format!("method {method} not found") }
    }
    pub fn exec(m: impl Into<String>) -> Self {
        RpcError { code: -32000, message: m.into() }
    }
}

impl From<xdag_chain::ChainError> for RpcError {
    fn from(e: xdag_chain::ChainError) -> Self {
        RpcError::internal(e.to_string())
    }
}

pub type RpcResult = Result<Value, RpcError>;

pub(crate) fn param(params: &Value, i: usize) -> Option<&Value> {
    match params {
        Value::Array(a) => a.get(i),
        _ => None,
    }
}

pub(crate) fn param_str(params: &Value, i: usize) -> Result<String, RpcError> {
    match param(params, i) {
        Some(Value::String(s)) => Ok(s.clone()),
        Some(Value::Number(n)) => Ok(n.to_string()),
        _ => Err(RpcError::invalid(format!("missing string parameter {i}"))),
    }
}

pub fn dispatch(b: &dyn Backend, method: &str, params: &Value) -> RpcResult {
    if method.starts_with("xdag_") {
        xdag::handle(b, method, params)
    } else if method.starts_with("eth_") || method.starts_with("net_") || method.starts_with("web3_") {
        eth::handle(b, method, params)
    } else {
        Err(RpcError::not_found(method))
    }
}

fn respond(b: &dyn Backend, req: &Value) -> Value {
    let id = req.get("id").cloned().unwrap_or(Value::Null);
    let Some(method) = req.get("method").and_then(|m| m.as_str()) else {
        return json!({"jsonrpc": "2.0", "id": id, "error": {"code": -32600, "message": "invalid request"}});
    };
    let params = req.get("params").cloned().unwrap_or(Value::Array(vec![]));
    match dispatch(b, method, &params) {
        Ok(v) => json!({"jsonrpc": "2.0", "id": id, "result": v}),
        Err(e) => json!({"jsonrpc": "2.0", "id": id, "error": {"code": e.code, "message": e.message}}),
    }
}

async fn handler(State(b): State<Arc<dyn Backend>>, Json(body): Json<Value>) -> impl IntoResponse {
    let b2 = b.clone();
    let out = tokio::task::spawn_blocking(move || match &body {
        Value::Array(reqs) => {
            if reqs.len() > 1000 {
                return json!({"jsonrpc": "2.0", "id": null, "error": {"code": -32600, "message": "batch too large"}});
            }
            Value::Array(reqs.iter().map(|r| respond(&*b2, r)).collect())
        }
        other => respond(&*b2, other),
    })
    .await
    .unwrap_or_else(|_| json!({"jsonrpc": "2.0", "id": null, "error": {"code": -32603, "message": "internal error"}}));
    let _ = b;
    Json(out)
}

pub fn router(b: Arc<dyn Backend>, cors_origin: Option<String>) -> Router {
    let mut r = Router::new().route("/", post(handler)).with_state(b);
    if let Some(origin) = cors_origin {
        let v = HeaderValue::from_str(&origin).unwrap_or(HeaderValue::from_static("*"));
        r = r.layer(axum::middleware::from_fn(move |req: axum::extract::Request, next: axum::middleware::Next| {
            let v = v.clone();
            async move {
                if req.method() == Method::OPTIONS {
                    let mut res = axum::http::Response::new(axum::body::Body::empty());
                    add_cors(res.headers_mut(), &v);
                    return res;
                }
                let mut res = next.run(req).await;
                add_cors(res.headers_mut(), &v);
                res
            }
        }));
    }
    r
}

fn add_cors(h: &mut axum::http::HeaderMap, origin: &HeaderValue) {
    h.insert("access-control-allow-origin", origin.clone());
    h.insert("access-control-allow-methods", HeaderValue::from_static("POST, OPTIONS"));
    h.insert("access-control-allow-headers", HeaderValue::from_static("content-type"));
}

pub async fn serve(addr: SocketAddr, b: Arc<dyn Backend>, cors_origin: Option<String>) -> std::io::Result<()> {
    let listener = tokio::net::TcpListener::bind(addr).await?;
    tracing::info!(%addr, "JSON-RPC listening");
    axum::serve(listener, router(b, cors_origin)).await
}

pub(crate) fn hex0x(b: &[u8]) -> String {
    format!("0x{}", hex::encode(b))
}

pub(crate) fn qty(v: impl Into<u128>) -> String {
    format!("{:#x}", v.into())
}
