use crate::state::{AppState, PeerInfo};
use once_cell::sync::Lazy;
use serde::{Deserialize, Serialize};
use std::{net::SocketAddr, sync::Arc, time::Duration};
use tokio::net::UdpSocket;
use uuid::Uuid;

const DISCOVERY_PORT: u16 = 9987;
const DISCOVERY_PROTOCOL: &str = "localbeam-discovery-v1";
const SCAN_TIMEOUT_MS: u64 = 1400;
const SCAN_INTERVAL_SECS: u64 = 5;
// Expire peers not heard from in this many seconds (must be > scan interval).
const PEER_TTL_SECS: u64 = 30;

static INSTANCE_ID: Lazy<String> = Lazy::new(|| Uuid::new_v4().to_string());

#[derive(Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
struct DiscoveryProbe {
    protocol: String,
    instance_id: String,
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
struct DiscoveryPeer {
    protocol: String,
    instance_id: String,
    id: String,
    name: String,
    avatar: String,
    host: String,
    transfer_port: u16,
    http_port: u16,
    mobile_web_available: bool,
}

/// Start the always-on UDP responder and the periodic background scanner.
pub async fn start(state: Arc<AppState>) {
    let st_resp = state.clone();
    tokio::spawn(async move {
        run_responder(st_resp).await;
    });

    tokio::spawn(async move {
        loop {
            scan_once(&state).await;
            tokio::time::sleep(Duration::from_secs(SCAN_INTERVAL_SECS)).await;
        }
    });
}

/// Trigger an immediate scan (called by the "Rescan" button).
pub async fn scan(state: &Arc<AppState>) {
    scan_once(state).await;
}

// ---------------------------------------------------------------------------
// Responder — listens on :9987 and replies unicast to any valid probe
// ---------------------------------------------------------------------------

async fn run_responder(state: Arc<AppState>) {
    let socket = match UdpSocket::bind(format!("0.0.0.0:{}", DISCOVERY_PORT)).await {
        Ok(s) => s,
        Err(e) => {
            tracing::error!(?e, "discovery responder failed to bind :9987");
            return;
        }
    };
    if let Err(e) = socket.set_broadcast(true) {
        tracing::warn!(?e, "discovery responder set_broadcast failed");
    }

    let mut buf = [0u8; 4096];
    loop {
        let (len, sender_addr) = match socket.recv_from(&mut buf).await {
            Ok(v) => v,
            Err(e) => {
                tracing::warn!(?e, "discovery responder recv error");
                continue;
            }
        };
        let Ok(probe) = serde_json::from_slice::<DiscoveryProbe>(&buf[..len]) else {
            continue;
        };
        if probe.protocol != DISCOVERY_PROTOCOL || probe.instance_id == *INSTANCE_ID {
            continue;
        }
        let peer = build_peer_snapshot(&state, sender_addr.ip().to_string());
        if let Ok(payload) = serde_json::to_vec(&peer) {
            let _ = socket.send_to(&payload, sender_addr).await;
        }
    }
}

// ---------------------------------------------------------------------------
// Scanner — broadcasts a probe and collects unicast replies until timeout
// ---------------------------------------------------------------------------

async fn scan_once(state: &Arc<AppState>) {
    let socket = match UdpSocket::bind("0.0.0.0:0").await {
        Ok(s) => s,
        Err(e) => {
            tracing::warn!(?e, "discovery scan bind failed");
            return;
        }
    };
    if let Err(e) = socket.set_broadcast(true) {
        tracing::warn!(?e, "discovery scan set_broadcast failed");
        return;
    }

    let probe = match serde_json::to_vec(&DiscoveryProbe {
        protocol: DISCOVERY_PROTOCOL.to_string(),
        instance_id: INSTANCE_ID.clone(),
    }) {
        Ok(v) => v,
        Err(e) => {
            tracing::warn!(?e, "discovery probe serialize failed");
            return;
        }
    };

    let broadcast_addr: SocketAddr = format!("255.255.255.255:{}", DISCOVERY_PORT)
        .parse()
        .unwrap();
    if let Err(e) = socket.send_to(&probe, broadcast_addr).await {
        tracing::warn!(?e, "discovery scan broadcast failed");
        return;
    }

    let mut found: Vec<DiscoveryPeer> = Vec::new();
    let deadline = tokio::time::Instant::now() + Duration::from_millis(SCAN_TIMEOUT_MS);
    let mut buf = [0u8; 4096];

    loop {
        let remaining = match deadline.checked_duration_since(tokio::time::Instant::now()) {
            Some(d) => d,
            None => break,
        };
        match tokio::time::timeout(remaining, socket.recv_from(&mut buf)).await {
            Ok(Ok((len, sender_addr))) => {
                let Ok(mut peer) = serde_json::from_slice::<DiscoveryPeer>(&buf[..len]) else {
                    continue;
                };
                if peer.protocol != DISCOVERY_PROTOCOL || peer.instance_id == *INSTANCE_ID {
                    continue;
                }
                // If the responder reported a loopback or empty host, use sender's IP.
                if peer.host.is_empty() || peer.host == "127.0.0.1" {
                    peer.host = sender_addr.ip().to_string();
                }
                found.push(peer);
            }
            _ => break,
        }
    }

    if found.is_empty() {
        // Still run expiry even when no new peers arrived.
        expire_peers(state);
        return;
    }

    let self_id = state.identity.fingerprint.clone();
    let now = crate::contacts::now_secs();

    {
        let mut map = state.peers.write();
        for peer in found {
            if peer.id == self_id {
                continue;
            }
            map.insert(
                peer.id.clone(),
                PeerInfo {
                    id: peer.id,
                    name: peer.name,
                    avatar: peer.avatar,
                    host: peer.host,
                    transfer_port: peer.transfer_port,
                    http_port: peer.http_port,
                    mobile_web_available: peer.mobile_web_available,
                    last_seen: now,
                },
            );
        }
        map.retain(|_, p| now.saturating_sub(p.last_seen) < PEER_TTL_SECS);
    }

    broadcast(state);
}

fn expire_peers(state: &Arc<AppState>) {
    let now = crate::contacts::now_secs();
    let mut map = state.peers.write();
    let before = map.len();
    map.retain(|_, p| now.saturating_sub(p.last_seen) < PEER_TTL_SECS);
    if map.len() != before {
        drop(map);
        broadcast(state);
    }
}

fn build_peer_snapshot(state: &AppState, fallback_ip: String) -> DiscoveryPeer {
    let s = state.settings.read().clone();
    let host = local_ip_address::local_ip()
        .map(|ip| ip.to_string())
        .unwrap_or(fallback_ip);
    DiscoveryPeer {
        protocol: DISCOVERY_PROTOCOL.to_string(),
        instance_id: INSTANCE_ID.clone(),
        id: state.identity.fingerprint.clone(),
        name: s.device_name,
        avatar: s.device_avatar,
        host,
        transfer_port: s.transfer_port,
        http_port: s.http_port,
        mobile_web_available: s.enable_qr_server,
    }
}

fn broadcast(state: &Arc<AppState>) {
    let peers: Vec<PeerInfo> = state.peers.read().values().cloned().collect();
    state.emit("peers", peers);
}
