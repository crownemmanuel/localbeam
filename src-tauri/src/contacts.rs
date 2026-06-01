use anyhow::Result;
use serde::{Deserialize, Serialize};
use std::{fs, path::PathBuf};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Contact {
    pub id: String, // fingerprint
    pub name: String,
    pub avatar: String,
    pub added_at: u64, // unix seconds
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ContactRequest {
    pub id: String,
    pub name: String,
    pub avatar: String,
    pub message: Option<String>,
    pub requested_at: u64,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct ContactBook {
    pub contacts: Vec<Contact>,
    pub blocked: Vec<String>,
    pub pending: Vec<ContactRequest>,
}

impl ContactBook {
    pub fn load_or_create(data_dir: &PathBuf) -> Result<Self> {
        let path = data_dir.join("contacts.json");
        if path.exists() {
            let txt = fs::read_to_string(&path)?;
            if let Ok(s) = serde_json::from_str::<ContactBook>(&txt) {
                return Ok(s);
            }
        }
        Ok(Self::default())
    }

    pub fn save(&self, data_dir: &PathBuf) -> Result<()> {
        fs::create_dir_all(data_dir).ok();
        let path = data_dir.join("contacts.json");
        fs::write(&path, serde_json::to_string_pretty(self)?)?;
        Ok(())
    }

    pub fn is_contact(&self, id: &str) -> bool {
        self.contacts.iter().any(|c| c.id == id)
    }

    pub fn is_blocked(&self, id: &str) -> bool {
        self.blocked.iter().any(|b| b == id)
    }

    pub fn upsert_pending(&mut self, req: ContactRequest) {
        if self.contacts.iter().any(|c| c.id == req.id) {
            return;
        }
        if let Some(slot) = self.pending.iter_mut().find(|p| p.id == req.id) {
            *slot = req;
        } else {
            self.pending.push(req);
        }
    }

    pub fn accept_pending(&mut self, id: &str) -> Option<Contact> {
        let idx = self.pending.iter().position(|p| p.id == id)?;
        let p = self.pending.remove(idx);
        let c = Contact {
            id: p.id,
            name: p.name,
            avatar: p.avatar,
            added_at: now_secs(),
        };
        self.contacts.push(c.clone());
        Some(c)
    }

    pub fn reject_pending(&mut self, id: &str) {
        self.pending.retain(|p| p.id != id);
    }

    pub fn remove_contact(&mut self, id: &str) {
        self.contacts.retain(|c| c.id != id);
    }
}

pub fn now_secs() -> u64 {
    use std::time::{SystemTime, UNIX_EPOCH};
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}
