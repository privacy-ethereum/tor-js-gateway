//! WebRTC data channel relay.
//!
//! Provides the same TCP relay functionality as the WebSocket proxy, but over
//! WebRTC data channels.  A single UDP socket multiplexes all peer connections.
//!
//! Signaling: browser POSTs an SDP offer to `/rtc/connect`, gets back an SDP answer.
//! Then opens data channels labeled `"ip:port"` to proxy TCP connections to Tor relays.

use std::collections::HashMap;
use std::net::{IpAddr, SocketAddr};
use std::time::{Duration, Instant};

use axum::extract::State;
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use serde::Deserialize;
use str0m::change::SdpOffer;
use str0m::channel::ChannelId;
use str0m::net::{Protocol, Receive};
use str0m::{Event, IceConnectionState, Input, Output, Rtc};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpStream, UdpSocket};
use tokio::sync::mpsc;
use tracing::{debug, info, warn};

use crate::ws_proxy::{ConnectionTracker, RelayAllowlist, WsLimits, is_local};

/// A newly negotiated peer, sent from the HTTP signaling handler to the UDP loop.
pub struct NewPeer {
    pub rtc: Rtc,
    pub peer_ip: IpAddr,
}

/// Messages from TCP bridge tasks back to the UDP event loop.
enum TcpMsg {
    /// Data read from TCP, to be written to the data channel.
    Data(ChannelId, Vec<u8>),
    /// TCP connection closed or errored.
    Closed(ChannelId),
}

/// Per-peer state in the UDP event loop.
struct Peer {
    rtc: Rtc,
    peer_ip: IpAddr,
    /// Data channel ID -> (label, sender for writing data channel bytes to TCP).
    channels: HashMap<ChannelId, (String, mpsc::Sender<Vec<u8>>)>,
    /// The `_signal` control channel, if open.
    signal_cid: Option<ChannelId>,
    created_at: Instant,
    last_activity: Instant,
}

/// HTTP handler for `POST /rtc/connect` — SDP offer/answer signaling.
pub async fn handle_rtc_connect(
    State(state): State<crate::server::AppState>,
    req: axum::extract::ConnectInfo<SocketAddr>,
    body: String,
) -> Response {
    let peer_ip = req.0.ip();

    let webrtc = match &state.webrtc_tx {
        Some(tx) => tx,
        None => {
            return (StatusCode::NOT_FOUND, "WebRTC not enabled").into_response();
        }
    };

    // Check connection limits before doing any work.
    if !state.connection_tracker.acquire(peer_ip, &state.ws_limits) {
        warn!("rtc: connection limit reached for {}", peer_ip);
        return (StatusCode::SERVICE_UNAVAILABLE, "connection limit reached").into_response();
    }

    // Browser sends {"type":"offer","sdp":"v=0\r\n..."} — extract the raw SDP string.
    #[derive(Deserialize)]
    struct BrowserOffer {
        sdp: String,
    }
    let browser_offer: BrowserOffer = match serde_json::from_str(&body) {
        Ok(o) => o,
        Err(e) => {
            state.connection_tracker.release(peer_ip);
            warn!("rtc: bad SDP offer from {}: {}", peer_ip, e);
            return (StatusCode::BAD_REQUEST, "invalid SDP offer").into_response();
        }
    };
    let offer = match SdpOffer::from_sdp_string(&browser_offer.sdp) {
        Ok(o) => o,
        Err(e) => {
            state.connection_tracker.release(peer_ip);
            warn!("rtc: bad SDP from {}: {}", peer_ip, e);
            return (StatusCode::BAD_REQUEST, "invalid SDP").into_response();
        }
    };

    let mut rtc = Rtc::new(Instant::now());

    // Add host ICE candidates for all network interfaces so the browser
    // can reach us via whichever path works (loopback, LAN, public, tunneled).
    if let Some(local_addr) = state.webrtc_local_addr {
        let port = local_addr.port();
        if local_addr.ip().is_unspecified() {
            // Bound to 0.0.0.0 — advertise all interface IPs.
            // Always include the peer's IP (the address they used to reach us).
            let mut added = std::collections::HashSet::new();
            for ip in gather_local_ips() {
                if added.insert(ip) {
                    if let Ok(c) = str0m::Candidate::host(SocketAddr::new(ip, port), "udp") {
                        rtc.add_local_candidate(c);
                    }
                }
            }
            // Also add the peer's connecting IP in case it's not in our interface list
            // (e.g. port-forwarded).
            if added.insert(peer_ip) {
                if let Ok(c) = str0m::Candidate::host(SocketAddr::new(peer_ip, port), "udp") {
                    rtc.add_local_candidate(c);
                }
            }
        } else {
            if let Ok(c) = str0m::Candidate::host(local_addr, "udp") {
                rtc.add_local_candidate(c);
            }
        }
    }

    let answer = match rtc.sdp_api().accept_offer(offer) {
        Ok(answer) => answer,
        Err(e) => {
            state.connection_tracker.release(peer_ip);
            warn!("rtc: failed to accept offer from {}: {}", peer_ip, e);
            return (StatusCode::BAD_REQUEST, "failed to process SDP offer").into_response();
        }
    };

    // Browser expects {"type":"answer","sdp":"v=0\r\n..."} format.
    let answer_json = serde_json::json!({
        "type": "answer",
        "sdp": answer.to_sdp_string(),
    })
    .to_string();

    // Hand off the Rtc instance to the UDP event loop.
    if webrtc.send(NewPeer { rtc, peer_ip }).await.is_err() {
        state.connection_tracker.release(peer_ip);
        return StatusCode::INTERNAL_SERVER_ERROR.into_response();
    }

    (
        StatusCode::OK,
        [("content-type", "application/json")],
        answer_json,
    )
        .into_response()
}

/// Run the UDP event loop that drives all WebRTC peers.
pub async fn run_udp_loop(
    udp: UdpSocket,
    local_addr: SocketAddr,
    mut new_peers_rx: mpsc::Receiver<NewPeer>,
    relay_allowlist: RelayAllowlist,
    connection_tracker: ConnectionTracker,
    ws_limits: WsLimits,
    has_ipv6: bool,
) {
    let mut peers: Vec<Peer> = Vec::new();
    let (tcp_tx, mut tcp_rx) = mpsc::channel::<TcpMsg>(1024);
    let mut buf = vec![0u8; 65536];

    info!("WebRTC UDP loop listening on {}", local_addr);

    loop {
        // Accept new peers.
        while let Ok(new_peer) = new_peers_rx.try_recv() {
            let now = Instant::now();
            peers.push(Peer {
                rtc: new_peer.rtc,
                peer_ip: new_peer.peer_ip,
                channels: HashMap::new(),
                signal_cid: None,
                created_at: now,
                last_activity: now,
            });
            debug!("rtc: new peer from {}, total={}", new_peer.peer_ip, peers.len());
        }

        // Poll all peers for output.
        let mut earliest_timeout = Instant::now() + Duration::from_millis(100);

        for peer in peers.iter_mut() {
            loop {
                match peer.rtc.poll_output() {
                    Ok(Output::Transmit(t)) => {
                        let _ = udp.send_to(&t.contents, t.destination).await;
                    }
                    Ok(Output::Event(event)) => {
                        handle_peer_event(
                            peer,
                            event,
                            &relay_allowlist,
                            &connection_tracker,
                            &ws_limits,
                            &tcp_tx,
                            has_ipv6,
                        );
                    }
                    Ok(Output::Timeout(t)) => {
                        if t < earliest_timeout {
                            earliest_timeout = t;
                        }
                        break;
                    }
                    Err(e) => {
                        debug!("rtc: poll error for {}: {}", peer.peer_ip, e);
                        break;
                    }
                }
            }
        }

        // Drain TCP -> data channel messages.
        while let Ok(msg) = tcp_rx.try_recv() {
            match msg {
                TcpMsg::Data(cid, data) => {
                    for peer in peers.iter_mut() {
                        if peer.channels.get(&cid).is_some() {
                            if let Some(mut ch) = peer.rtc.channel(cid) {
                                let _ = ch.write(true, &data);
                            }
                            peer.last_activity = Instant::now();
                            break;
                        }
                    }
                }
                TcpMsg::Closed(cid) => {
                    for peer in peers.iter_mut() {
                        if let Some((label, _)) = peer.channels.remove(&cid) {
                            let sctp_id = peer.rtc.direct_api()
                                .sctp_stream_id_by_channel_id(cid)
                                .unwrap_or(0);
                            peer.rtc.direct_api().close_data_channel(cid);
                            // Notify client via signal channel so it can close locally.
                            if let Some(sig_cid) = peer.signal_cid {
                                if let Some(mut ch) = peer.rtc.channel(sig_cid) {
                                    let msg = serde_json::json!({
                                        "type": "closed",
                                        "channel": label,
                                        "sctp_id": sctp_id,
                                    });
                                    let _ = ch.write(false, msg.to_string().as_bytes());
                                }
                            }
                            connection_tracker.release(peer.peer_ip);
                            debug!("rtc: TCP closed for channel {:?} ({}), peer {}", cid, label, peer.peer_ip);
                            break;
                        }
                    }
                }
            }
        }

        // Enforce idle timeout and max lifetime.
        let now = Instant::now();
        peers.retain_mut(|peer| {
            let idle = now.duration_since(peer.last_activity) > ws_limits.idle_timeout;
            let expired = now.duration_since(peer.created_at) > ws_limits.max_lifetime;
            if idle || expired {
                if idle {
                    debug!("rtc: idle timeout for {}", peer.peer_ip);
                } else {
                    debug!("rtc: max lifetime for {}", peer.peer_ip);
                }
                let n = peer.channels.len();
                peer.channels.clear();
                for _ in 0..n {
                    connection_tracker.release(peer.peer_ip);
                }
                // Release the initial connection slot from signaling.
                connection_tracker.release(peer.peer_ip);
                return false;
            }
            true
        });

        // Remove disconnected peers.
        peers.retain_mut(|peer| {
            if !peer.rtc.is_alive() {
                debug!("rtc: peer {} disconnected", peer.peer_ip);
                let n = peer.channels.len();
                peer.channels.clear();
                for _ in 0..n {
                    connection_tracker.release(peer.peer_ip);
                }
                connection_tracker.release(peer.peer_ip);
                return false;
            }
            true
        });

        // Wait for UDP packet or timeout.
        let wait = earliest_timeout.saturating_duration_since(Instant::now());
        let wait = wait.max(Duration::from_millis(1));

        tokio::select! {
            result = udp.recv_from(&mut buf) => {
                match result {
                    Ok((n, source)) => {
                        let port = local_addr.port();

                        // When bound to 0.0.0.0, we don't know which local IP the
                        // packet was addressed to. Try each candidate IP we registered
                        // until a peer accepts.
                        let candidate_ips: Vec<IpAddr> = if local_addr.ip().is_unspecified() {
                            gather_local_ips()
                        } else {
                            vec![local_addr.ip()]
                        };

                        let mut handled = false;
                        for dest_ip in &candidate_ips {
                            let dest = SocketAddr::new(*dest_ip, port);
                            let Ok(receive) = Receive::new(
                                Protocol::Udp,
                                source,
                                dest,
                                &buf[..n],
                            ) else {
                                continue;
                            };
                            let input = Input::Receive(Instant::now(), receive);

                            if let Some(peer) = peers.iter_mut().find(|p| p.rtc.accepts(&input)) {
                                if let Err(e) = peer.rtc.handle_input(input) {
                                    debug!("rtc: input error for {}: {}", peer.peer_ip, e);
                                }
                                peer.last_activity = Instant::now();
                                handled = true;
                                break;
                            }
                        }
                        if !handled {
                            debug!("rtc: no peer accepted packet from {}", source);
                        }
                    }
                    Err(e) => {
                        warn!("rtc: UDP recv error: {}", e);
                    }
                }
            }
            _ = tokio::time::sleep(wait) => {
                // Drive timeouts for all peers.
                let now = Instant::now();
                for peer in peers.iter_mut() {
                    let _ = peer.rtc.handle_input(Input::Timeout(now));
                }
            }
        }
    }
}

/// Handle an event from a peer's Rtc instance.
fn handle_peer_event(
    peer: &mut Peer,
    event: Event,
    relay_allowlist: &RelayAllowlist,
    connection_tracker: &ConnectionTracker,
    ws_limits: &WsLimits,
    tcp_tx: &mpsc::Sender<TcpMsg>,
    has_ipv6: bool,
) {
    /// Send a JSON message on the signal channel.
    fn signal_send(peer: &mut Peer, msg: &serde_json::Value) {
        if let Some(cid) = peer.signal_cid {
            if let Some(mut ch) = peer.rtc.channel(cid) {
                let _ = ch.write(false, msg.to_string().as_bytes());
            }
        }
    }

    match event {
        Event::ChannelOpen(cid, label) => {
            // --- Signal channel ---
            if label == "_signal" {
                info!("rtc: signal channel open from {}", peer.peer_ip);
                peer.signal_cid = Some(cid);
                signal_send(peer, &serde_json::json!({
                    "type": "hello",
                    "server": "tor-js-gateway",
                    "ipv6": has_ipv6,
                }));
                return;
            }

            // --- Init channel (ignored) ---
            if label == "_init" {
                peer.rtc.direct_api().close_data_channel(cid);
                return;
            }

            let sctp_id = peer.rtc.direct_api()
                .sctp_stream_id_by_channel_id(cid)
                .unwrap_or(0);
            info!("rtc: channel open {:?} sctp={} label='{}' from {}", cid, sctp_id, label, peer.peer_ip);

            // Parse label as target address.
            let addr: SocketAddr = match label.parse() {
                Ok(a) => a,
                Err(_) => {
                    warn!("rtc: bad channel label '{}'", label);
                    signal_send(peer, &serde_json::json!({
                        "type": "rejected",
                        "channel": label,
                        "sctp_id": sctp_id,
                        "reason": "invalid target address",
                    }));
                    peer.rtc.direct_api().close_data_channel(cid);
                    return;
                }
            };

            // Security checks (same as WS proxy).
            let rejection = if addr.is_ipv6() && !has_ipv6 {
                Some("IPv6 not supported on this server")
            } else if is_local(addr.ip()) {
                Some("local addresses forbidden")
            } else if !relay_allowlist
                .read()
                .unwrap_or_else(|e| e.into_inner())
                .contains(&addr)
            {
                Some("not an advertised relay")
            } else if !connection_tracker.acquire(peer.peer_ip, ws_limits) {
                Some("connection limit reached")
            } else {
                None
            };

            if let Some(reason) = rejection {
                warn!("rtc: rejected {} — {}", addr, reason);
                signal_send(peer, &serde_json::json!({
                    "type": "rejected",
                    "channel": label,
                    "sctp_id": sctp_id,
                    "reason": reason,
                }));
                peer.rtc.direct_api().close_data_channel(cid);
                return;
            }

            // Spawn TCP bridge task.
            let (dc_to_tcp_tx, dc_to_tcp_rx) = mpsc::channel::<Vec<u8>>(64);
            let tcp_tx = tcp_tx.clone();
            tokio::spawn(tcp_bridge_task(addr, cid, dc_to_tcp_rx, tcp_tx));

            peer.channels.insert(cid, (label.to_string(), dc_to_tcp_tx));
            peer.last_activity = Instant::now();
        }
        Event::ChannelData(data) => {
            peer.last_activity = Instant::now();

            // Handle signal channel messages.
            if peer.signal_cid == Some(data.id) {
                if let Ok(text) = std::str::from_utf8(&data.data) {
                    if let Ok(msg) = serde_json::from_str::<serde_json::Value>(text) {
                        match msg.get("type").and_then(|t| t.as_str()) {
                            Some("ping") => {
                                signal_send(peer, &serde_json::json!({
                                    "type": "pong",
                                    "ts": msg.get("ts"),
                                }));
                            }
                            _ => {}
                        }
                    }
                }
                return;
            }

            if let Some((_, tx)) = peer.channels.get(&data.id) {
                let _ = tx.try_send(data.data.to_vec());
            }
        }
        Event::ChannelClose(cid) => {
            if peer.signal_cid == Some(cid) {
                peer.signal_cid = None;
            } else if peer.channels.remove(&cid).is_some() { // (label, tx) dropped
                connection_tracker.release(peer.peer_ip);
                debug!("rtc: channel {:?} closed by remote", cid);
            }
        }
        Event::IceConnectionStateChange(IceConnectionState::Disconnected) => {
            debug!("rtc: ICE disconnected for {}", peer.peer_ip);
        }
        Event::Connected => {
            info!("rtc: peer {} connected", peer.peer_ip);
        }
        _ => {}
    }
}

/// Bridge between a data channel and a TCP connection to a Tor relay.
async fn tcp_bridge_task(
    target: SocketAddr,
    cid: ChannelId,
    mut dc_to_tcp_rx: mpsc::Receiver<Vec<u8>>,
    tcp_tx: mpsc::Sender<TcpMsg>,
) {
    let tcp = match tokio::time::timeout(
        Duration::from_secs(10),
        TcpStream::connect(target),
    )
    .await
    {
        Ok(Ok(s)) => s,
        Ok(Err(e)) => {
            debug!("rtc: TCP connect to {} failed: {}", target, e);
            let _ = tcp_tx.send(TcpMsg::Closed(cid)).await;
            return;
        }
        Err(_) => {
            debug!("rtc: TCP connect to {} timed out", target);
            let _ = tcp_tx.send(TcpMsg::Closed(cid)).await;
            return;
        }
    };
    info!("rtc: TCP connected to {} for channel {:?}", target, cid);

    let (mut tcp_read, mut tcp_write) = tcp.into_split();

    // DC -> TCP
    let dc_to_tcp = async {
        while let Some(data) = dc_to_tcp_rx.recv().await {
            if tcp_write.write_all(&data).await.is_err() {
                break;
            }
        }
        let _ = tcp_write.shutdown().await;
    };

    // TCP -> DC
    let tcp_to_dc = async {
        let mut buf = vec![0u8; 16384];
        loop {
            match tcp_read.read(&mut buf).await {
                Ok(0) | Err(_) => break,
                Ok(n) => {
                    if tcp_tx
                        .send(TcpMsg::Data(cid, buf[..n].to_vec()))
                        .await
                        .is_err()
                    {
                        break;
                    }
                }
            }
        }
    };

    tokio::select! {
        _ = dc_to_tcp => {}
        _ = tcp_to_dc => {}
    }

    let _ = tcp_tx.send(TcpMsg::Closed(cid)).await;
    debug!("rtc: TCP bridge for {:?} to {} done", cid, target);
}

/// Gather all non-unspecified IP addresses from local network interfaces.
fn gather_local_ips() -> Vec<IpAddr> {
    let mut ips = Vec::new();

    // Always include loopback.
    ips.push(IpAddr::V4(std::net::Ipv4Addr::LOCALHOST));

    // Use getifaddrs via a UDP connect trick to discover interface IPs.
    // Connect to a remote address (doesn't actually send anything) to find
    // the default outgoing IP.
    if let Ok(sock) = std::net::UdpSocket::bind("0.0.0.0:0") {
        // Try a public IP to get the default route IP.
        if sock.connect("8.8.8.8:80").is_ok() {
            if let Ok(addr) = sock.local_addr() {
                if !addr.ip().is_unspecified() && !addr.ip().is_loopback() {
                    ips.push(addr.ip());
                }
            }
        }
    }

    // Parse /proc/net/if_inet6 and /proc/net/fib_trie for more addresses.
    // Simpler: read from ip command output.
    if let Ok(output) = std::process::Command::new("ip")
        .args(["-o", "addr", "show"])
        .output()
    {
        if let Ok(text) = String::from_utf8(output.stdout) {
            for line in text.lines() {
                // Format: "2: eth0    inet 192.168.1.5/24 ..."
                let parts: Vec<&str> = line.split_whitespace().collect();
                if parts.len() >= 4 && (parts[2] == "inet" || parts[2] == "inet6") {
                    if let Some(addr_str) = parts[3].split('/').next() {
                        if let Ok(ip) = addr_str.parse::<IpAddr>() {
                            if !ip.is_unspecified() && !ips.contains(&ip) {
                                ips.push(ip);
                            }
                        }
                    }
                }
            }
        }
    }

    ips
}
