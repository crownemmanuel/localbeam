use anyhow::Result;
use serde::{Deserialize, Serialize};
use std::{fs, path::PathBuf};

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AllowMode {
    All,
    Contacts,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Settings {
    pub device_name: String,
    pub device_avatar: String, // single emoji
    pub save_dir: String,
    pub allow_mode: AllowMode,
    /// Require explicit per-transfer accept by the user (default true).
    pub require_accept: bool,
    /// Run the HTTPS/HTTP QR-upload server.
    pub enable_qr_server: bool,
    /// Persistent ports (so QR url and discovery stay stable across restarts).
    pub transfer_port: u16,
    pub http_port: u16,
}

impl Settings {
    pub fn default_with(name: String, save_dir: String) -> Self {
        Self {
            device_name: name,
            device_avatar: "💻".into(),
            save_dir,
            allow_mode: AllowMode::All,
            require_accept: true,
            enable_qr_server: true,
            transfer_port: pick_port(0xB1),
            http_port: pick_port(0xB2),
        }
    }

    pub fn load_or_create(data_dir: &PathBuf) -> Result<Self> {
        let path = data_dir.join("settings.json");
        if path.exists() {
            let txt = fs::read_to_string(&path)?;
            if let Ok(s) = serde_json::from_str::<Settings>(&txt) {
                return Ok(s);
            }
        }
        let default_name = hostname::get()
            .ok()
            .and_then(|s| s.into_string().ok())
            .unwrap_or_else(|| "My Computer".to_string());
        let downloads = dirs::document_dir()
            .or_else(dirs::home_dir)
            .unwrap_or_else(|| PathBuf::from("."));
        let save_dir = downloads.join("LocalBeam");
        fs::create_dir_all(&save_dir).ok();
        let s = Settings::default_with(default_name, save_dir.to_string_lossy().into());
        s.save(data_dir).ok();
        Ok(s)
    }

    pub fn save(&self, data_dir: &PathBuf) -> Result<()> {
        fs::create_dir_all(data_dir).ok();
        let path = data_dir.join("settings.json");
        let txt = serde_json::to_string_pretty(self)?;
        fs::write(&path, txt)?;
        Ok(())
    }
}

// Deterministic-ish port from a tag, in 49152..=65535 range using random offset on first run.
fn pick_port(tag: u8) -> u16 {
    use std::time::{SystemTime, UNIX_EPOCH};
    let t = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_nanos() as u64)
        .unwrap_or(0);
    let base = 49152u16;
    let span = 65535u16 - base;
    let off = ((t as u32).wrapping_add(tag as u32)) % (span as u32);
    base + off as u16
}
