use crate::{
    contacts::Contact,
    settings::Settings,
    state::{AppState, PeerInfo, TransferProgress},
    transfer,
};
use serde::Serialize;
use std::{path::PathBuf, sync::Arc};
use tauri::State;

#[derive(Serialize)]
pub struct MeInfo {
    pub id: String,
    pub name: String,
    pub avatar: String,
    pub host: Option<String>,
    pub transfer_port: u16,
    pub http_port: u16,
    pub save_dir: String,
    pub allow_mode: String,
    pub require_accept: bool,
    pub enable_qr_server: bool,
    pub qr_url: Option<String>,
}

#[tauri::command]
pub fn get_me(state: State<'_, Arc<AppState>>) -> MeInfo {
    let s = state.settings.read().clone();
    let host = local_ip();
    let qr_url = host
        .as_ref()
        .map(|ip| format!("http://{}:{}/", ip, s.http_port));
    MeInfo {
        id: state.identity.fingerprint.clone(),
        name: s.device_name.clone(),
        avatar: s.device_avatar.clone(),
        host,
        transfer_port: s.transfer_port,
        http_port: s.http_port,
        save_dir: s.save_dir.clone(),
        allow_mode: match s.allow_mode {
            crate::settings::AllowMode::All => "all".into(),
            crate::settings::AllowMode::Contacts => "contacts".into(),
        },
        require_accept: s.require_accept,
        enable_qr_server: s.enable_qr_server,
        qr_url,
    }
}

#[tauri::command]
pub fn list_peers(state: State<'_, Arc<AppState>>) -> Vec<PeerInfo> {
    state.peers.read().values().cloned().collect()
}

#[tauri::command]
pub fn list_contacts(state: State<'_, Arc<AppState>>) -> Vec<Contact> {
    state.contacts.read().contacts.clone()
}

#[tauri::command]
pub fn list_pending_contact_requests(
    state: State<'_, Arc<AppState>>,
) -> Vec<crate::contacts::ContactRequest> {
    state.contacts.read().pending.clone()
}

#[tauri::command]
pub fn list_transfers(state: State<'_, Arc<AppState>>) -> Vec<TransferProgress> {
    state.active_transfers.read().values().cloned().collect()
}

#[derive(serde::Deserialize)]
pub struct SettingsPatch {
    pub device_name: Option<String>,
    pub device_avatar: Option<String>,
    pub save_dir: Option<String>,
    pub allow_mode: Option<String>,
    pub require_accept: Option<bool>,
    pub enable_qr_server: Option<bool>,
}

#[tauri::command]
pub fn update_settings(
    state: State<'_, Arc<AppState>>,
    patch: SettingsPatch,
) -> Result<Settings, String> {
    let mut s = state.settings.write();
    if let Some(n) = patch.device_name {
        if !n.trim().is_empty() {
            s.device_name = n.trim().to_string();
        }
    }
    if let Some(a) = patch.device_avatar {
        if !a.is_empty() {
            s.device_avatar = a;
        }
    }
    if let Some(d) = patch.save_dir {
        s.save_dir = d;
    }
    if let Some(m) = patch.allow_mode {
        s.allow_mode = if m == "contacts" {
            crate::settings::AllowMode::Contacts
        } else {
            crate::settings::AllowMode::All
        };
    }
    if let Some(r) = patch.require_accept {
        s.require_accept = r;
    }
    if let Some(q) = patch.enable_qr_server {
        s.enable_qr_server = q;
    }
    s.save(&state.data_dir).map_err(|e| e.to_string())?;
    Ok(s.clone())
}

#[tauri::command]
pub async fn send_files(
    state: State<'_, Arc<AppState>>,
    peer_id: String,
    paths: Vec<String>,
) -> Result<String, String> {
    let st = state.inner().clone();
    let pbs: Vec<PathBuf> = paths.iter().map(PathBuf::from).collect();
    transfer::send_files(st, peer_id, pbs)
        .await
        .map_err(|e| e.to_string())
}

#[tauri::command]
pub async fn send_contact_request(
    state: State<'_, Arc<AppState>>,
    peer_id: String,
    message: Option<String>,
) -> Result<(), String> {
    let st = state.inner().clone();
    transfer::send_contact_request(st, peer_id, message)
        .await
        .map_err(|e| e.to_string())
}

#[tauri::command]
pub fn decide_incoming(
    state: State<'_, Arc<AppState>>,
    transfer_id: String,
    accept: bool,
) -> Result<(), String> {
    let st = state.inner().clone();
    transfer::decide_pending(&st, &transfer_id, accept);
    Ok(())
}

#[tauri::command]
pub fn accept_contact_request(
    state: State<'_, Arc<AppState>>,
    id: String,
) -> Result<Option<Contact>, String> {
    let mut book = state.contacts.write();
    let c = book.accept_pending(&id);
    book.save(&state.data_dir).map_err(|e| e.to_string())?;
    Ok(c)
}

#[tauri::command]
pub fn reject_contact_request(
    state: State<'_, Arc<AppState>>,
    id: String,
) -> Result<(), String> {
    let mut book = state.contacts.write();
    book.reject_pending(&id);
    book.save(&state.data_dir).map_err(|e| e.to_string())?;
    Ok(())
}

#[tauri::command]
pub fn remove_contact(
    state: State<'_, Arc<AppState>>,
    id: String,
) -> Result<(), String> {
    let mut book = state.contacts.write();
    book.remove_contact(&id);
    book.save(&state.data_dir).map_err(|e| e.to_string())?;
    Ok(())
}

#[tauri::command]
pub fn clear_completed_transfers(state: State<'_, Arc<AppState>>) {
    let mut m = state.active_transfers.write();
    m.retain(|_, v| {
        !matches!(
            v.status,
            crate::state::TransferStatus::Completed
                | crate::state::TransferStatus::Rejected
                | crate::state::TransferStatus::Failed
                | crate::state::TransferStatus::Cancelled
        )
    });
}

fn local_ip() -> Option<String> {
    use std::net::IpAddr;
    if let Ok(list) = local_ip_address::list_afinet_netifas() {
        for (_n, ip) in list {
            if ip.is_loopback() {
                continue;
            }
            if let IpAddr::V4(v4) = ip {
                if !v4.is_link_local() {
                    return Some(v4.to_string());
                }
            }
        }
    }
    local_ip_address::local_ip().ok().map(|ip| ip.to_string())
}
