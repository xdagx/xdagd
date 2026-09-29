//! WebSocket endpoint for external pools (xdagj `pool.ws.port`).
//!
//! This is the node↔pool interface of *one* node operator, so an allow list of
//! pool IPs is a sensible local default (loopback); it has nothing to do with
//! which nodes may join the network.

use std::net::{IpAddr, SocketAddr};
use std::sync::Arc;

use axum::extract::ws::{Message as WsMessage, WebSocket, WebSocketUpgrade};
use axum::extract::{ConnectInfo, State};
use axum::response::IntoResponse;
use axum::routing::get;
use axum::Router;
use futures::{SinkExt, StreamExt};

use crate::Coordinator;

#[derive(Clone)]
struct Ctx {
    coord: Arc<Coordinator>,
    allowed: Arc<Vec<IpAddr>>,
}

pub async fn serve(addr: SocketAddr, coord: Arc<Coordinator>, allowed: Vec<IpAddr>) -> std::io::Result<()> {
    let ctx = Ctx { coord, allowed: Arc::new(allowed) };
    let app = Router::new().route("/", get(upgrade)).route("/websocket", get(upgrade)).with_state(ctx);
    let listener = tokio::net::TcpListener::bind(addr).await?;
    tracing::info!(%addr, "pool WebSocket listening");
    axum::serve(listener, app.into_make_service_with_connect_info::<SocketAddr>()).await
}

async fn upgrade(State(ctx): State<Ctx>, ConnectInfo(peer): ConnectInfo<SocketAddr>, ws: WebSocketUpgrade) -> impl IntoResponse {
    let any = ctx.allowed.iter().any(|ip| ip.is_unspecified());
    if !any && !ctx.allowed.contains(&peer.ip()) {
        return axum::http::StatusCode::FORBIDDEN.into_response();
    }
    ws.on_upgrade(move |socket| session(ctx, socket, peer)).into_response()
}

async fn session(ctx: Ctx, socket: WebSocket, peer: SocketAddr) {
    tracing::info!(%peer, "pool connected");
    let (mut tx, mut rx) = socket.split();
    let mut tasks = ctx.coord.tasks.subscribe();
    if let Some(t) = ctx.coord.current_task() {
        let _ = tx.send(WsMessage::Text(t.to_json().into())).await;
    }
    loop {
        tokio::select! {
            t = tasks.recv() => match t {
                Ok(json) => {
                    if tx.send(WsMessage::Text(json.into())).await.is_err() {
                        break;
                    }
                }
                Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => continue,
                Err(_) => break,
            },
            m = rx.next() => match m {
                Some(Ok(WsMessage::Text(text))) => handle_share(&ctx.coord, text.as_str()),
                Some(Ok(WsMessage::Ping(p))) => { let _ = tx.send(WsMessage::Pong(p)).await; }
                Some(Ok(WsMessage::Close(_))) | None | Some(Err(_)) => break,
                _ => {}
            }
        }
    }
    tracing::info!(%peer, "pool disconnected");
}

fn handle_share(coord: &Coordinator, text: &str) {
    let Ok(v) = serde_json::from_str::<serde_json::Value>(text) else { return };
    if v.get("msgType").and_then(|x| x.as_i64()) != Some(2) {
        return;
    }
    let c = &v["msgContent"];
    let (Some(share), Some(idx)) = (c["share"].as_str(), c["taskIndex"].as_u64()) else { return };
    let Ok(bytes) = hex::decode(share.trim_start_matches("0x")) else { return };
    let Ok(nonce) = <[u8; 32]>::try_from(bytes.as_slice()) else { return };
    if coord.submit_share(idx, c["hash"].as_str(), nonce) {
        tracing::debug!(task = idx, "pool share improved the candidate");
    }
}
