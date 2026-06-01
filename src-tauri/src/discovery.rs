use crate::state::{AppState, PeerInfo};
use anyhow::Result;
use mdns_sd::{ServiceDaemon, ServiceEvent, ServiceInfo};
use std::{collections::HashMap, net::IpAddr, sync::Arc};

const SERVICE_TYPE: &str = "_localbeam._tcp.local.";

pub fn start(state: Arc<AppState>) -> Result<ServiceDaemon> {
    let mdns = ServiceDaemon::new()?;
    publish(&mdns, &state)?;

    let receiver = mdns.browse(SERVICE_TYPE)?;
    let st = state.clone();
    std::thread::spawn(move || {
        while let Ok(event) = receiver.recv() {
            match event {
                ServiceEvent::ServiceResolved(info) => {
                    if let Some(peer) = peer_from_info(&info, &st.identity.fingerprint) {
                        st.peers.write().insert(peer.id.clone(), peer);
                        broadcast(&st);
                    }
                }
                ServiceEvent::ServiceRemoved(_, fullname) => {
                    let mut map = st.peers.write();
                    map.retain(|_, p| !fullname.starts_with(&format!("{}.", p.id)));
                    drop(map);
                    broadcast(&st);
                }
                _ => {}
            }
        }
    });

    Ok(mdns)
}

pub fn republish(mdns: &ServiceDaemon, state: &Arc<AppState>) -> Result<()> {
    // Unregister current and re-register with updated metadata.
    let id = state.identity.fingerprint.clone();
    let full = format!("{}.{}", id, SERVICE_TYPE);
    let _ = mdns.unregister(&full);
    publish(mdns, state)
}

fn publish(mdns: &ServiceDaemon, state: &Arc<AppState>) -> Result<()> {
    let s = state.settings.read().clone();
    let id = state.identity.fingerprint.clone();
    let host_name = format!("{}.local.", short_id(&id));
    let ips = local_ips();
    let mut props: HashMap<String, String> = HashMap::new();
    props.insert("id".into(), id.clone());
    props.insert("name".into(), s.device_name.clone());
    props.insert("avatar".into(), s.device_avatar.clone());
    props.insert("http".into(), s.http_port.to_string());

    let info = ServiceInfo::new(
        SERVICE_TYPE,
        &id,
        &host_name,
        &ips[..],
        s.transfer_port,
        Some(props),
    )?;
    mdns.register(info)?;
    Ok(())
}

fn short_id(id: &str) -> String {
    id.chars().take(12).collect()
}

fn local_ips() -> Vec<IpAddr> {
    let mut out = Vec::new();
    if let Ok(list) = local_ip_address::list_afinet_netifas() {
        for (_name, ip) in list {
            if ip.is_loopback() {
                continue;
            }
            if let IpAddr::V4(v4) = ip {
                if !v4.is_link_local() {
                    out.push(IpAddr::V4(v4));
                }
            }
        }
    }
    if out.is_empty() {
        if let Ok(ip) = local_ip_address::local_ip() {
            out.push(ip);
        }
    }
    out
}

fn peer_from_info(info: &ServiceInfo, self_id: &str) -> Option<PeerInfo> {
    let props = info.get_properties();
    let id = props
        .get_property_val_str("id")
        .map(|s| s.to_string())
        .unwrap_or_else(|| info.get_fullname().to_string());
    if id == self_id {
        return None;
    }
    let name = props
        .get_property_val_str("name")
        .map(|s| s.to_string())
        .unwrap_or_else(|| "Unknown".to_string());
    let avatar = props
        .get_property_val_str("avatar")
        .map(|s| s.to_string())
        .unwrap_or_else(|| "💻".into());
    let http_port = props
        .get_property_val_str("http")
        .and_then(|s| s.parse::<u16>().ok())
        .unwrap_or(0);
    let host = info
        .get_addresses()
        .iter()
        .filter_map(|ip| match ip {
            IpAddr::V4(v4) if !v4.is_loopback() => Some(v4.to_string()),
            _ => None,
        })
        .next()?;
    Some(PeerInfo {
        id,
        name,
        avatar,
        host,
        transfer_port: info.get_port(),
        http_port,
        last_seen: crate::contacts::now_secs(),
    })
}

fn broadcast(state: &Arc<AppState>) {
    let peers: Vec<PeerInfo> = state.peers.read().values().cloned().collect();
    state.emit("peers", peers);
}
