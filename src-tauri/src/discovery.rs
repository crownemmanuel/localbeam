use crate::state::{AppState, PeerInfo};
use once_cell::sync::Lazy;
use serde::{Deserialize, Serialize};
use std::{
    collections::HashSet,
    net::{IpAddr, Ipv4Addr, SocketAddr},
    sync::Arc,
    time::Duration,
};
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
    #[serde(default = "default_mobile_web_available")]
    mobile_web_available: bool,
}

fn default_mobile_web_available() -> bool {
    // Builds before mobile peer switching did not announce this field. Treat
    // them as available so newer builds still discover and can route to them.
    true
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

    let mut sent = 0usize;
    for target in discovery_targets() {
        match socket.send_to(&probe, target).await {
            Ok(_) => sent += 1,
            Err(e) => tracing::warn!(?e, %target, "discovery scan broadcast failed"),
        }
    }

    if sent == 0 {
        expire_peers(state);
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
                    manual: false,
                    last_seen: now,
                },
            );
        }
        map.retain(|_, p| p.manual || now.saturating_sub(p.last_seen) < PEER_TTL_SECS);
    }

    broadcast(state);
}

fn expire_peers(state: &Arc<AppState>) {
    let now = crate::contacts::now_secs();
    let mut map = state.peers.write();
    let before = map.len();
    map.retain(|_, p| p.manual || now.saturating_sub(p.last_seen) < PEER_TTL_SECS);
    if map.len() != before {
        drop(map);
        broadcast(state);
    }
}

fn build_peer_snapshot(state: &AppState, fallback_ip: String) -> DiscoveryPeer {
    let s = state.settings.read().clone();
    let host = fallback_ip
        .parse::<IpAddr>()
        .ok()
        .and_then(best_local_host_for_remote)
        .map(|ip| ip.to_string())
        .or_else(|| local_ip_address::local_ip().ok().map(|ip| ip.to_string()))
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

#[derive(Debug, Clone, Copy)]
struct LocalIpv4Interface {
    ip: Ipv4Addr,
    netmask: Ipv4Addr,
    broadcast: Option<Ipv4Addr>,
}

fn discovery_targets() -> Vec<SocketAddr> {
    let mut seen = HashSet::new();
    let mut targets = Vec::new();

    add_target(&mut targets, &mut seen, Ipv4Addr::new(255, 255, 255, 255));

    for iface in local_ipv4_interfaces() {
        if let Some(broadcast) = iface.broadcast {
            add_target(&mut targets, &mut seen, broadcast);
        }
        if let Some(broadcast) = directed_broadcast(&iface) {
            add_target(&mut targets, &mut seen, broadcast);
        }
    }

    targets
}

fn add_target(targets: &mut Vec<SocketAddr>, seen: &mut HashSet<SocketAddr>, ip: Ipv4Addr) {
    if !is_usable_broadcast(ip) {
        return;
    }

    let target = SocketAddr::from((ip, DISCOVERY_PORT));
    if seen.insert(target) {
        targets.push(target);
    }
}

fn local_ipv4_interfaces() -> Vec<LocalIpv4Interface> {
    match if_addrs::get_if_addrs() {
        Ok(ifaces) => ifaces
            .into_iter()
            .filter_map(|iface| match iface.addr {
                if_addrs::IfAddr::V4(v4) if is_usable_local_ipv4(v4.ip) => {
                    Some(LocalIpv4Interface {
                        ip: v4.ip,
                        netmask: v4.netmask,
                        broadcast: v4.broadcast,
                    })
                }
                _ => None,
            })
            .collect(),
        Err(e) => {
            tracing::warn!(?e, "discovery failed to list local interfaces");
            Vec::new()
        }
    }
}

fn best_local_host_for_remote(remote: IpAddr) -> Option<Ipv4Addr> {
    let remote = match remote {
        IpAddr::V4(ip) => ip,
        IpAddr::V6(_) => return None,
    };

    local_ipv4_interfaces()
        .into_iter()
        .find(|iface| same_subnet(iface.ip, remote, iface.netmask))
        .map(|iface| iface.ip)
}

fn directed_broadcast(iface: &LocalIpv4Interface) -> Option<Ipv4Addr> {
    let mask = u32::from(iface.netmask);
    if mask == 0 || mask == u32::MAX {
        return None;
    }
    Some(Ipv4Addr::from(u32::from(iface.ip) | !mask))
}

fn same_subnet(a: Ipv4Addr, b: Ipv4Addr, netmask: Ipv4Addr) -> bool {
    let mask = u32::from(netmask);
    mask != 0 && (u32::from(a) & mask) == (u32::from(b) & mask)
}

fn is_usable_local_ipv4(ip: Ipv4Addr) -> bool {
    !ip.is_loopback() && !ip.is_link_local() && !ip.is_unspecified()
}

fn is_usable_broadcast(ip: Ipv4Addr) -> bool {
    !ip.is_loopback() && !ip.is_link_local() && !ip.is_unspecified()
}

fn broadcast(state: &Arc<AppState>) {
    let peers: Vec<PeerInfo> = state.peers.read().values().cloned().collect();
    state.emit("peers", peers);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn discovery_peer_accepts_pre_mobile_web_payloads() {
        let payload = r#"{
            "protocol": "localbeam-discovery-v1",
            "instanceId": "old-instance",
            "id": "peer-id",
            "name": "Older LocalBeam",
            "avatar": "desktop",
            "host": "192.168.1.40",
            "transferPort": 45454,
            "httpPort": 45455
        }"#;

        let peer: DiscoveryPeer = serde_json::from_str(payload).unwrap();
        assert!(peer.mobile_web_available);
    }

    #[test]
    fn directed_broadcast_uses_interface_netmask() {
        let iface = LocalIpv4Interface {
            ip: Ipv4Addr::new(192, 168, 12, 34),
            netmask: Ipv4Addr::new(255, 255, 255, 0),
            broadcast: None,
        };

        assert_eq!(
            directed_broadcast(&iface),
            Some(Ipv4Addr::new(192, 168, 12, 255))
        );
    }
}
