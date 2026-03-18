//! WebSocket-to-TCP relay.
//!
//! Accepts WebSocket upgrades at `/socket/{ip}:{port}` and proxies
//! bidirectionally to the target TCP address.  This enables browser
//! clients (which cannot open raw TCP sockets) to reach Tor relays.
//!
//! Only targets present in the current consensus relay allowlist are
//! permitted.  Local/private IPs are also rejected as a defence-in-depth
//! measure.

use std::collections::HashSet;
use std::net::{IpAddr, SocketAddr};
use std::sync::{Arc, RwLock};

use axum::extract::Path;
use axum::extract::ws::{Message, WebSocket, WebSocketUpgrade};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;
use tracing::{debug, info, warn};

/// Shared set of relay `SocketAddr`s from the current consensus.
pub type RelayAllowlist = Arc<RwLock<HashSet<SocketAddr>>>;

/// Returns true if the IP is non-routable (loopback, private, link-local, etc.).
fn is_local(ip: IpAddr) -> bool {
    match ip {
        IpAddr::V4(v4) => {
            v4.is_loopback()
                || v4.is_private()
                || v4.is_link_local()
                || v4.is_broadcast()
                || v4.is_unspecified()
        }
        IpAddr::V6(v6) => {
            v6.is_loopback()
                || v6.is_unspecified()
                // ::ffff:127.0.0.1 etc.
                || v6.to_ipv4_mapped().is_some_and(|v4| {
                    v4.is_loopback() || v4.is_private() || v4.is_link_local()
                })
        }
    }
}

/// Handler for `GET /socket/:target` — upgrades to WebSocket, then relays.
pub async fn handle_socket(
    axum::extract::State(state): axum::extract::State<crate::server::AppState>,
    Path(target): Path<String>,
    ws: WebSocketUpgrade,
) -> Response {
    let allowlist = &state.relay_allowlist;
    let addr: SocketAddr = match target.parse() {
        Ok(a) => a,
        Err(_) => {
            warn!("bad target '{}'", target);
            return (StatusCode::BAD_REQUEST, "invalid target address").into_response();
        }
    };

    if is_local(addr.ip()) {
        warn!("rejected local target {}", addr);
        return (StatusCode::FORBIDDEN, "connections to local addresses are forbidden").into_response();
    }

    let allowed = allowlist.read().unwrap_or_else(|e| e.into_inner()).contains(&addr);
    if !allowed {
        warn!("rejected non-relay target {}", addr);
        return (StatusCode::FORBIDDEN, "target is not an advertised Tor relay").into_response();
    }

    ws.on_upgrade(move |socket| relay(socket, addr))
}

async fn relay(ws: WebSocket, target: SocketAddr) {
    if let Err(e) = relay_inner(ws, target).await {
        debug!("ws relay to {}: {}", target, e);
    }
}

async fn relay_inner(ws: WebSocket, addr: SocketAddr) -> Result<(), Box<dyn std::error::Error>> {
    let tcp = TcpStream::connect(addr).await?;
    info!("ws relay connected to {}", addr);

    let (mut tcp_read, mut tcp_write) = tcp.into_split();
    let (mut ws_write, mut ws_read) = ws.split();

    use futures::SinkExt;
    use futures::StreamExt;

    // WS -> TCP
    let ws_to_tcp = async {
        while let Some(msg) = ws_read.next().await {
            match msg {
                Ok(Message::Binary(data)) => {
                    if tcp_write.write_all(&data).await.is_err() {
                        break;
                    }
                }
                Ok(Message::Close(_)) | Err(_) => break,
                _ => {}
            }
        }
        let _ = tcp_write.shutdown().await;
    };

    // TCP -> WS
    let tcp_to_ws = async {
        let mut buf = vec![0u8; 16384];
        loop {
            let n = match tcp_read.read(&mut buf).await {
                Ok(0) | Err(_) => break,
                Ok(n) => n,
            };
            if ws_write
                .send(Message::Binary(buf[..n].to_vec().into()))
                .await
                .is_err()
            {
                break;
            }
        }
        let _ = ws_write.send(Message::Close(None)).await;
    };

    tokio::select! {
        _ = ws_to_tcp => {}
        _ = tcp_to_ws => {}
    }

    debug!("ws relay to {} done", addr);
    Ok(())
}
