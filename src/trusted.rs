//! Persistent trust store: paired devices and their certificate fingerprints.
//!
//! KDE Connect pins the peer certificate at pairing time; reconnecting devices
//! must present the same certificate. Persisting the list lets the daemon
//! remember pairings across restarts (the phone does the same on its side).

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use tokio::sync::RwLock;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TrustedDevice {
    pub id: String,
    pub name: String,
    /// SHA-256 (hex) of the peer certificate seen during pairing.
    #[serde(default)]
    pub cert_sha256: Option<String>,
    /// Last IPv4 address the device was heard from. Only paired devices are
    /// stored, and the value is used to announce unicast after a restart.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_ip: Option<String>,
}

/// Hex-encoded SHA-256 fingerprint of a DER certificate.
pub fn fingerprint(cert_der: &[u8]) -> String {
    let mut hasher = Sha256::new();
    hasher.update(cert_der);
    hex::encode(hasher.finalize())
}

pub struct TrustedStore {
    path: Option<PathBuf>,
    devices: RwLock<HashMap<String, TrustedDevice>>,
}

impl TrustedStore {
    /// In-memory store (tests).
    pub fn in_memory() -> Self {
        Self {
            path: None,
            devices: RwLock::new(HashMap::new()),
        }
    }

    /// Loads `trusted_devices.json` from the data directory, if present.
    pub fn load(data_dir: &Path) -> Self {
        let path = data_dir.join("trusted_devices.json");
        let devices = match std::fs::read(&path) {
            Ok(bytes) => match serde_json::from_slice::<Vec<TrustedDevice>>(&bytes) {
                Ok(list) => list.into_iter().map(|d| (d.id.clone(), d)).collect(),
                Err(e) => {
                    tracing::warn!(error = %e, path = %path.display(), "ignoring unreadable trust store");
                    HashMap::new()
                }
            },
            Err(_) => HashMap::new(),
        };
        Self {
            path: Some(path),
            devices: RwLock::new(devices),
        }
    }

    pub async fn all(&self) -> Vec<TrustedDevice> {
        self.devices.read().await.values().cloned().collect()
    }

    pub async fn get(&self, id: &str) -> Option<TrustedDevice> {
        self.devices.read().await.get(id).cloned()
    }

    /// Adds or updates a trusted device and persists the store.
    pub async fn insert(&self, device: TrustedDevice) {
        self.devices.write().await.insert(device.id.clone(), device);
        self.persist().await;
    }

    pub async fn remove(&self, id: &str) {
        self.devices.write().await.remove(id);
        self.persist().await;
    }

    /// Updates the last known address of a trusted device. Only the in-memory
    /// entry and the persisted file are touched when the address changed.
    /// Returns whether it changed.
    pub async fn update_last_ip(&self, id: &str, ip: &str) -> bool {
        {
            let mut devices = self.devices.write().await;
            let Some(device) = devices.get_mut(id) else {
                return false;
            };
            if device.last_ip.as_deref() == Some(ip) {
                return false;
            }
            device.last_ip = Some(ip.to_owned());
        }
        self.persist().await;
        true
    }

    async fn persist(&self) {
        let Some(path) = &self.path else {
            return;
        };
        let mut list: Vec<TrustedDevice> = self.devices.read().await.values().cloned().collect();
        list.sort_by(|a, b| a.id.cmp(&b.id));
        let json = match serde_json::to_vec_pretty(&list) {
            Ok(j) => j,
            Err(e) => {
                tracing::warn!(error = %e, "cannot serialize trust store");
                return;
            }
        };
        if let Err(e) = tokio::fs::write(path, json).await {
            tracing::warn!(error = %e, path = %path.display(), "cannot write trust store");
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn survives_reload() {
        let dir = tempfile::tempdir().expect("tempdir");
        let store = TrustedStore::load(dir.path());
        store
            .insert(TrustedDevice {
                id: "a".repeat(32),
                name: "Phone".to_owned(),
                cert_sha256: Some("deadbeef".to_owned()),
                last_ip: Some("192.168.1.20".to_owned()),
            })
            .await;

        let reloaded = TrustedStore::load(dir.path());
        let all = reloaded.all().await;
        assert_eq!(all.len(), 1);
        assert_eq!(all[0].name, "Phone");
        assert_eq!(all[0].cert_sha256.as_deref(), Some("deadbeef"));
        assert_eq!(all[0].last_ip.as_deref(), Some("192.168.1.20"));
    }

    #[tokio::test]
    async fn updates_last_ip_only_for_known_devices() {
        let dir = tempfile::tempdir().expect("tempdir");
        let store = TrustedStore::load(dir.path());
        store
            .insert(TrustedDevice {
                id: "b".repeat(32),
                name: "Phone".to_owned(),
                cert_sha256: None,
                last_ip: None,
            })
            .await;

        assert!(!store.update_last_ip(&"c".repeat(32), "192.168.1.30").await);
        assert!(store.update_last_ip(&"b".repeat(32), "192.168.1.30").await);
        assert!(!store.update_last_ip(&"b".repeat(32), "192.168.1.30").await);

        let reloaded = TrustedStore::load(dir.path());
        let device = reloaded.get(&"b".repeat(32)).await.expect("device");
        assert_eq!(device.last_ip.as_deref(), Some("192.168.1.30"));
    }

    #[test]
    fn fingerprints_are_stable() {
        assert_eq!(fingerprint(b"abc"), fingerprint(b"abc"));
        assert_ne!(fingerprint(b"abc"), fingerprint(b"abd"));
    }
}
