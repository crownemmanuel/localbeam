use crate::{contacts::ContactBook, identity::Identity, settings::Settings};
use parking_lot::RwLock;
use serde::{Deserialize, Serialize};
use std::{collections::HashMap, path::PathBuf, sync::Arc};
use tauri::{AppHandle, Emitter};
use tokio::sync::oneshot;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PeerInfo {
    pub id: String,
    pub name: String,
    pub avatar: String,
    pub host: String, // ipv4 string
    pub transfer_port: u16,
    pub http_port: u16,
    pub mobile_web_available: bool,
    #[serde(default)]
    pub manual: bool,
    pub last_seen: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct IncomingPrompt {
    pub transfer_id: String,
    pub from_id: String,
    pub from_name: String,
    pub from_avatar: String,
    pub files: Vec<TransferFileMeta>,
    pub total_bytes: u64,
    pub source: TransferSource,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TransferSource {
    Peer,
    QrUpload,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TransferFileMeta {
    pub name: String,
    pub size: u64,
    pub mime: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TransferProgress {
    pub transfer_id: String,
    pub created_at: u64,
    pub direction: TransferDirection,
    pub peer_id: Option<String>,
    pub peer_name: String,
    pub files: Vec<TransferFileMeta>,
    pub current_file_index: usize,
    pub bytes_sent: u64,
    pub total_bytes: u64,
    pub status: TransferStatus,
    pub error: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum TransferDirection {
    Outgoing,
    Incoming,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum TransferStatus {
    Pending,
    Active,
    Completed,
    Failed,
    Rejected,
    Cancelled,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ContactRequestInbound {
    pub id: String,
    pub name: String,
    pub avatar: String,
    pub message: Option<String>,
}

pub struct AppState {
    pub data_dir: PathBuf,
    pub identity: Identity,
    pub settings: RwLock<Settings>,
    pub contacts: RwLock<ContactBook>,
    pub peers: RwLock<HashMap<String, PeerInfo>>,
    pub pending_decisions: RwLock<HashMap<String, oneshot::Sender<bool>>>,
    pub active_transfers: RwLock<HashMap<String, TransferProgress>>,
    pub app: RwLock<Option<AppHandle>>,
    /// When false, the main window won't auto-hide on blur (e.g. while a native
    /// dialog is open, or while a modal is up).
    pub auto_hide: RwLock<bool>,
}

impl AppState {
    pub fn new(
        data_dir: PathBuf,
        identity: Identity,
        settings: Settings,
        contacts: ContactBook,
    ) -> Arc<Self> {
        let now = crate::contacts::now_secs();
        let peers = settings
            .manual_peers
            .iter()
            .map(|peer| {
                (
                    peer.id.clone(),
                    PeerInfo {
                        id: peer.id.clone(),
                        name: peer.name.clone(),
                        avatar: "💻".into(),
                        host: peer.host.clone(),
                        transfer_port: peer.transfer_port,
                        http_port: peer.http_port,
                        mobile_web_available: true,
                        manual: true,
                        last_seen: now,
                    },
                )
            })
            .collect();

        Arc::new(Self {
            data_dir,
            identity,
            settings: RwLock::new(settings),
            contacts: RwLock::new(contacts),
            peers: RwLock::new(peers),
            pending_decisions: RwLock::new(HashMap::new()),
            active_transfers: RwLock::new(HashMap::new()),
            app: RwLock::new(None),
            auto_hide: RwLock::new(true),
        })
    }

    pub fn set_app_handle(&self, h: AppHandle) {
        *self.app.write() = Some(h);
    }

    pub fn emit<S: Serialize + Clone>(&self, event: &str, payload: S) {
        if let Some(app) = self.app.read().clone() {
            let _ = app.emit(event, payload);
        }
    }

    pub fn update_transfer(&self, t: TransferProgress) {
        self.active_transfers
            .write()
            .insert(t.transfer_id.clone(), t.clone());
        self.emit("transfer-progress", t);
    }
}
