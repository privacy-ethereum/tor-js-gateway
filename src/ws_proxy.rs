//! WebSocket-to-TCP relay.
//!
//! Accepts WebSocket upgrades at `/socket/{ip}:{port}` and proxies
//! bidirectionally to the target TCP address.  This enables browser
//! clients (which cannot open raw TCP sockets) to reach Tor relays.
//!
//! Only targets present in the current consensus relay allowlist are
//! permitted.  Local/private IPs are also rejected as a defence-in-depth
//! measure.
//!
//! Connections are subject to configurable limits: max total connections,
//! per-IP cap, idle timeout, and max lifetime.

use std::collections::{HashMap, HashSet};
use std::net::{IpAddr, SocketAddr};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, RwLock};
use std::time::Duration;

use axum::extract::Path;
use axum::extract::ws::{Message, WebSocket, WebSocketUpgrade};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;
use tracing::{debug, info, warn};

/// Shared set of relay `SocketAddr`s from the current consensus.
pub type RelayAllowlist = Arc<RwLock<HashSet<SocketAddr>>>;

/// Configuration for WS relay limits.
#[derive(Clone, Debug)]
pub struct WsLimits {
    pub max_connections: usize,
    pub per_ip_limit: usize,
    pub idle_timeout: Duration,
    pub max_lifetime: Duration,
}

impl Default for WsLimits {
    fn default() -> Self {
        Self {
            max_connections: 8192,
            per_ip_limit: 16,
            idle_timeout: Duration::from_secs(300),
            max_lifetime: Duration::from_secs(3600),
        }
    }
}

/// Tracks active WS connections globally and per-IP.
#[derive(Clone)]
pub struct ConnectionTracker {
    total: Arc<AtomicUsize>,
    per_ip: Arc<Mutex<HashMap<IpAddr, usize>>>,
}

impl ConnectionTracker {
    pub fn new() -> Self {
        Self {
            total: Arc::new(AtomicUsize::new(0)),
            per_ip: Arc::new(Mutex::new(HashMap::new())),
        }
    }

    /// Try to acquire a slot. Returns false if a limit would be exceeded.
    fn acquire(&self, ip: IpAddr, limits: &WsLimits) -> bool {
        let current = self.total.load(Ordering::Relaxed);
        if current >= limits.max_connections {
            return false;
        }

        let mut map = self.per_ip.lock().unwrap_or_else(|e| e.into_inner());
        let count = map.entry(ip).or_insert(0);
        if *count >= limits.per_ip_limit {
            return false;
        }

        self.total.fetch_add(1, Ordering::Relaxed);
        *count += 1;
        true
    }

    fn release(&self, ip: IpAddr) {
        self.total.fetch_sub(1, Ordering::Relaxed);
        let mut map = self.per_ip.lock().unwrap_or_else(|e| e.into_inner());
        if let Some(count) = map.get_mut(&ip) {
            *count -= 1;
            if *count == 0 {
                map.remove(&ip);
            }
        }
    }
}

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
    req: axum::extract::ConnectInfo<SocketAddr>,
    Path(target): Path<String>,
    ws: WebSocketUpgrade,
) -> Response {
    let peer_ip = req.0.ip();
    let allowlist = &state.relay_allowlist;
    let tracker = &state.connection_tracker;
    let limits = &state.ws_limits;

    let addr: SocketAddr = match target.parse() {
        Ok(a) => a,
        Err(_) => {
            warn!("bad target '{}'", target);
            return (StatusCode::BAD_REQUEST, "invalid target address").into_response();
        }
    };

    if is_local(addr.ip()) {
        warn!("rejected local target {}", addr);
        return (StatusCode::FORBIDDEN, "connections to local addresses are forbidden")
            .into_response();
    }

    let allowed = allowlist
        .read()
        .unwrap_or_else(|e| e.into_inner())
        .contains(&addr);
    if !allowed {
        warn!("rejected non-relay target {}", addr);
        return (StatusCode::FORBIDDEN, "target is not an advertised Tor relay").into_response();
    }

    if !tracker.acquire(peer_ip, limits) {
        warn!("connection limit reached for {}", peer_ip);
        return (StatusCode::SERVICE_UNAVAILABLE, "connection limit reached").into_response();
    }

    let tracker = tracker.clone();
    let idle_timeout = limits.idle_timeout;
    let max_lifetime = limits.max_lifetime;
    ws.on_upgrade(move |socket| async move {
        relay(socket, addr, peer_ip, tracker, idle_timeout, max_lifetime).await;
    })
}

async fn relay(
    ws: WebSocket,
    target: SocketAddr,
    peer_ip: IpAddr,
    tracker: ConnectionTracker,
    idle_timeout: Duration,
    max_lifetime: Duration,
) {
    if let Err(e) = relay_inner(ws, target, idle_timeout, max_lifetime).await {
        debug!("ws relay to {}: {}", target, e);
    }
    tracker.release(peer_ip);
}

async fn relay_inner(
    ws: WebSocket,
    addr: SocketAddr,
    idle_timeout: Duration,
    max_lifetime: Duration,
) -> Result<(), Box<dyn std::error::Error>> {
    let tcp = TcpStream::connect(addr).await?;
    info!("ws relay connected to {}", addr);

    let (mut tcp_read, mut tcp_write) = tcp.into_split();
    let (mut ws_write, mut ws_read) = ws.split();

    use futures::SinkExt;
    use futures::StreamExt;

    let deadline = tokio::time::Instant::now() + max_lifetime;

    // WS -> TCP
    let ws_to_tcp = async {
        loop {
            let msg = tokio::time::timeout(idle_timeout, ws_read.next()).await;
            match msg {
                Err(_) => {
                    debug!("ws relay to {}: idle timeout", addr);
                    break;
                }
                Ok(None) => break,
                Ok(Some(Ok(Message::Binary(data)))) => {
                    if tcp_write.write_all(&data).await.is_err() {
                        break;
                    }
                }
                Ok(Some(Ok(Message::Close(_)))) | Ok(Some(Err(_))) => break,
                Ok(Some(Ok(_))) => {}
            }
        }
        let _ = tcp_write.shutdown().await;
    };

    // TCP -> WS
    let tcp_to_ws = async {
        let mut buf = vec![0u8; 16384];
        loop {
            let read = tokio::time::timeout(idle_timeout, tcp_read.read(&mut buf)).await;
            match read {
                Err(_) => {
                    debug!("ws relay to {}: idle timeout", addr);
                    break;
                }
                Ok(Ok(0)) | Ok(Err(_)) => break,
                Ok(Ok(n)) => {
                    if ws_write
                        .send(Message::Binary(buf[..n].to_vec().into()))
                        .await
                        .is_err()
                    {
                        break;
                    }
                }
            }
        }
        let _ = ws_write.send(Message::Close(None)).await;
    };

    tokio::select! {
        _ = ws_to_tcp => {}
        _ = tcp_to_ws => {}
        _ = tokio::time::sleep_until(deadline) => {
            debug!("ws relay to {}: max lifetime reached", addr);
        }
    }

    debug!("ws relay to {} done", addr);
    Ok(())
}
