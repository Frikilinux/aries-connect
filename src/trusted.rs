//! Persistent trust store: paired devices and their per-device configuration.
//!
//! KDE Connect pins the peer certificate at pairing time; reconnecting devices
//! must present the same certificate. Every paired device lives in its own
//! directory under `<data_dir>/devices/<deviceId>/` so state remains isolated
//! per device. Legacy monolithic `trusted_devices.json` files are migrated on
//! load and removed afterwards.

use std::collections::HashMap;
use std::io;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use tokio::sync::RwLock;

/// Subdirectory holding one directory per paired device.
const DEVICES_DIR: &str = "devices";
/// Configuration file inside each device directory.
const DEVICE_FILE: &str = "device.json";
/// Legacy monolithic store migrated by [`TrustedStore::load`].
const LEGACY_FILE: &str = "trusted_devices.json";

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TrustedDevice {
    pub id: String,
    pub name: String,
    /// SHA-256 (hex) of the peer certificate seen during pairing.
    #[serde(default)]
    pub cert_sha256: Option<String>,
    /// Last IPv4 address the device was heard from; it seeds unicast discovery
    /// after a restart.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_ip: Option<String>,
    /// Per-plugin overrides. Only toggled plugins are listed; plugins absent
    /// from the map are enabled.
    #[serde(default, skip_serializing_if = "HashMap::is_empty")]
    pub plugins: HashMap<String, bool>,
}

impl TrustedDevice {
    /// Whether `plugin` is enabled; anything not present counts as enabled.
    pub fn plugin_enabled(&self, plugin: &str) -> bool {
        self.plugins.get(plugin).copied().unwrap_or(true)
    }
}

/// Hex-encoded SHA-256 fingerprint of a DER certificate.
pub fn fingerprint(cert_der: &[u8]) -> String {
    let mut hasher = Sha256::new();
    hasher.update(cert_der);
    hex::encode(hasher.finalize())
}

/// Device ids become directory names; restrict them to a safe subset.
fn is_safe_component(id: &str) -> bool {
    !id.is_empty()
        && id.len() <= 64
        && id != "."
        && id != ".."
        && id
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')
}

pub struct TrustedStore {
    /// `devices/` directory, or `None` for the in-memory store used in tests.
    devices_dir: Option<PathBuf>,
    devices: RwLock<HashMap<String, TrustedDevice>>,
}

impl TrustedStore {
    /// In-memory store (tests).
    pub fn in_memory() -> Self {
        Self {
            devices_dir: None,
            devices: RwLock::new(HashMap::new()),
        }
    }

    /// Loads every `devices/<id>/device.json` from the data directory and
    /// migrates a legacy `trusted_devices.json` if one is still present.
    pub fn load(data_dir: &Path) -> Self {
        let devices_dir = data_dir.join(DEVICES_DIR);
        let mut devices = HashMap::new();

        if let Ok(entries) = std::fs::read_dir(&devices_dir) {
            for entry in entries.flatten() {
                let path = entry.path().join(DEVICE_FILE);
                let device = std::fs::read(&path)
                    .ok()
                    .and_then(|bytes| serde_json::from_slice::<TrustedDevice>(&bytes).ok());
                match device {
                    Some(device) if is_safe_component(&device.id) => {
                        devices.insert(device.id.clone(), device);
                    }
                    Some(_) => tracing::warn!(
                        path = %path.display(),
                        "device id is not a safe directory name"
                    ),
                    None => tracing::warn!(
                        path = %path.display(),
                        "ignoring unreadable device file"
                    ),
                }
            }
        }

        Self::migrate_legacy(data_dir, &mut devices);

        Self {
            devices_dir: Some(devices_dir),
            devices: RwLock::new(devices),
        }
    }

    /// Moves entries from the legacy monolithic file into per-device
    /// directories. Device files loaded beforehand win. The legacy file is
    /// deleted only when every entry is represented on disk.
    fn migrate_legacy(data_dir: &Path, devices: &mut HashMap<String, TrustedDevice>) {
        let legacy_path = data_dir.join(LEGACY_FILE);
        let bytes = match std::fs::read(&legacy_path) {
            Ok(bytes) => bytes,
            Err(_) => return,
        };
        let list: Vec<TrustedDevice> = match serde_json::from_slice(&bytes) {
            Ok(list) => list,
            Err(e) => {
                tracing::warn!(
                    error = %e,
                    path = %legacy_path.display(),
                    "ignoring unreadable legacy trust store"
                );
                return;
            }
        };

        let mut all_persisted = true;
        for device in list {
            if !is_safe_component(&device.id) {
                tracing::warn!(device = %device.id, "legacy device id is not a safe directory name");
                all_persisted = false;
                continue;
            }
            if devices.contains_key(&device.id) {
                continue;
            }
            let dir = data_dir.join(DEVICES_DIR).join(&device.id);
            match std::fs::create_dir_all(&dir)
                .and_then(|()| write_atomic(&dir.join(DEVICE_FILE), &device))
            {
                Ok(()) => {
                    devices.insert(device.id.clone(), device);
                }
                Err(e) => {
                    tracing::warn!(error = %e, device = %device.id, "cannot migrate legacy device");
                    all_persisted = false;
                }
            }
        }

        if all_persisted {
            match std::fs::remove_file(&legacy_path) {
                Ok(()) => tracing::info!("migrated trust store to per-device directories"),
                Err(e) => tracing::warn!(
                    error = %e,
                    path = %legacy_path.display(),
                    "cannot remove legacy trust store"
                ),
            }
        }
    }

    pub async fn all(&self) -> Vec<TrustedDevice> {
        self.devices.read().await.values().cloned().collect()
    }

    pub async fn get(&self, id: &str) -> Option<TrustedDevice> {
        self.devices.read().await.get(id).cloned()
    }

    /// Adds or updates a trusted device and persists its file. Device ids that
    /// cannot form a safe directory name stay in memory only.
    pub async fn insert(&self, device: TrustedDevice) {
        let dir = self.device_dir(&device.id);
        if self.devices_dir.is_some() && dir.is_none() {
            tracing::warn!(
                device = %device.id,
                "device id is not a safe directory name; keeping it in memory only"
            );
        }
        let device_to_store = device.clone();
        self.devices.write().await.insert(device.id.clone(), device);
        if let Some(dir) = dir {
            write_device_file(&dir, &device_to_store).await;
        }
    }

    pub async fn remove(&self, id: &str) {
        self.devices.write().await.remove(id);
        let Some(dir) = self.device_dir(id) else {
            return;
        };
        match tokio::fs::remove_dir_all(&dir).await {
            Ok(()) => {}
            Err(e) if e.kind() == io::ErrorKind::NotFound => {}
            Err(e) => tracing::warn!(
                error = %e,
                path = %dir.display(),
                "cannot remove device directory"
            ),
        }
    }

    /// Enables or disables `plugin` for a device. Plugins absent from the map
    /// are enabled, so enabling removes the override. Returns whether the
    /// state changed and was persisted.
    pub async fn set_plugin_enabled(&self, id: &str, plugin: &str, enabled: bool) -> bool {
        {
            let mut devices = self.devices.write().await;
            let Some(device) = devices.get_mut(id) else {
                tracing::debug!(device = %id, "plugin state change for an unknown device");
                return false;
            };
            if device.plugin_enabled(plugin) == enabled {
                return false;
            }
            if enabled {
                device.plugins.remove(plugin);
            } else {
                device.plugins.insert(plugin.to_owned(), false);
            }
        }
        self.persist_device(id).await;
        true
    }

    /// Whether the stored device enables `plugin`; unknown devices count as
    /// disabled so callers never act on devices they do not know.
    pub async fn plugin_enabled(&self, id: &str, plugin: &str) -> bool {
        self.devices
            .read()
            .await
            .get(id)
            .is_some_and(|device| device.plugin_enabled(plugin))
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
        self.persist_device(id).await;
        true
    }

    /// Serializes the in-memory device into `<devices_dir>/<id>/device.json`
    /// through a temporary file and a rename.
    async fn persist_device(&self, id: &str) {
        let Some(dir) = self.device_dir(id) else {
            return;
        };
        let device = match self.devices.read().await.get(id) {
            Some(device) => device.clone(),
            None => return,
        };
        write_device_file(&dir, &device).await;
    }

    /// Directory holding the on-disk state of one device, or `None` when the
    /// store is in-memory or the id is unsuitable for a directory name.
    fn device_dir(&self, id: &str) -> Option<PathBuf> {
        if !is_safe_component(id) {
            return None;
        }
        self.devices_dir.as_ref().map(|dir| dir.join(id))
    }
}

/// Persists one device file atomically: `device.json.tmp` followed by a
/// rename, so readers never observe a partially written file.
async fn write_device_file(dir: &Path, device: &TrustedDevice) {
    let json = match serde_json::to_vec_pretty(device) {
        Ok(json) => json,
        Err(e) => {
            tracing::warn!(error = %e, "cannot serialize device file");
            return;
        }
    };
    let path = dir.join(DEVICE_FILE);
    let tmp = dir.join(format!("{DEVICE_FILE}.tmp"));
    let result = async {
        tokio::fs::create_dir_all(dir).await?;
        tokio::fs::write(&tmp, json).await?;
        tokio::fs::rename(&tmp, &path).await
    }
    .await;
    if let Err(e) = result {
        tracing::warn!(error = %e, path = %path.display(), "cannot write device file");
    }
}

/// Writes `value` to `path` through a `path.tmp` file and a rename, so readers
/// never observe a partially written file. Used by the synchronous migration
/// path; `spawn_device_write` is its asynchronous counterpart.
fn write_atomic<T: serde::Serialize>(path: &Path, value: &T) -> io::Result<()> {
    let json = serde_json::to_vec_pretty(value).map_err(io::Error::other)?;
    let tmp = path.with_extension("json.tmp");
    std::fs::write(&tmp, json)?;
    std::fs::rename(&tmp, path)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn phone(id: &str, plugins: HashMap<String, bool>) -> TrustedDevice {
        TrustedDevice {
            id: id.to_owned(),
            name: "Phone".to_owned(),
            cert_sha256: Some("deadbeef".to_owned()),
            last_ip: Some("192.168.1.20".to_owned()),
            plugins,
        }
    }

    #[tokio::test]
    async fn survives_reload_in_device_directory() {
        let dir = tempfile::tempdir().expect("tempdir");
        let store = TrustedStore::load(dir.path());
        let id = "a".repeat(32);
        store.insert(phone(&id, HashMap::new())).await;

        let file = dir.path().join("devices").join(&id).join("device.json");
        assert!(file.is_file(), "device.json must exist");
        assert!(
            !dir.path()
                .join("devices")
                .join(&id)
                .join("device.json.tmp")
                .exists(),
            "atomic write must not leave temporary files"
        );

        let reloaded = TrustedStore::load(dir.path());
        let all = reloaded.all().await;
        assert_eq!(all.len(), 1);
        assert_eq!(all[0].id, id);
        assert_eq!(all[0].name, "Phone");
        assert_eq!(all[0].cert_sha256.as_deref(), Some("deadbeef"));
        assert_eq!(all[0].last_ip.as_deref(), Some("192.168.1.20"));
        assert!(all[0].plugin_enabled("kdeconnect.notification"));
    }

    #[tokio::test]
    async fn migrates_legacy_monolithic_store() {
        let dir = tempfile::tempdir().expect("tempdir");
        let id = "b".repeat(32);
        let legacy = serde_json::to_vec_pretty(&vec![phone(&id, HashMap::new())]).expect("legacy");
        std::fs::write(dir.path().join(LEGACY_FILE), legacy).expect("legacy store");

        let store = TrustedStore::load(dir.path());
        assert_eq!(store.all().await.len(), 1);
        assert!(
            dir.path()
                .join("devices")
                .join(&id)
                .join("device.json")
                .is_file(),
            "legacy entry becomes a device directory"
        );
        assert!(
            !dir.path().join(LEGACY_FILE).exists(),
            "legacy file is removed after a successful migration"
        );

        let reloaded = TrustedStore::load(dir.path());
        assert_eq!(reloaded.all().await.len(), 1);
    }

    #[tokio::test]
    async fn device_file_wins_over_legacy_entry() {
        let dir = tempfile::tempdir().expect("tempdir");
        let id = "c".repeat(32);
        let mut legacy_device = phone(&id, HashMap::new());
        legacy_device.name = "Legacy name".to_owned();
        std::fs::write(
            dir.path().join(LEGACY_FILE),
            serde_json::to_vec_pretty(&vec![legacy_device]).expect("legacy"),
        )
        .expect("legacy store");

        let devices_dir = dir.path().join("devices").join(&id);
        std::fs::create_dir_all(&devices_dir).expect("devices dir");
        let mut disk_device = phone(&id, HashMap::new());
        disk_device.name = "Fresh name".to_owned();
        write_atomic(&devices_dir.join(DEVICE_FILE), &disk_device).expect("device file");

        let store = TrustedStore::load(dir.path());
        let all = store.all().await;
        assert_eq!(all.len(), 1);
        assert_eq!(all[0].name, "Fresh name");
        assert!(!dir.path().join(LEGACY_FILE).exists());
    }

    #[tokio::test]
    async fn toggles_plugins_and_persists_overrides() {
        let dir = tempfile::tempdir().expect("tempdir");
        let store = TrustedStore::load(dir.path());
        let id = "d".repeat(32);
        store.insert(phone(&id, HashMap::new())).await;

        // Unknown plugins are enabled by default and toggling persists.
        assert!(store.plugin_enabled(&id, "kdeconnect.notification").await);
        assert!(
            store
                .set_plugin_enabled(&id, "kdeconnect.notification", false)
                .await
        );
        assert!(
            !store
                .set_plugin_enabled(&id, "kdeconnect.notification", false)
                .await
        );
        assert!(!store.plugin_enabled(&id, "kdeconnect.notification").await);
        // Reenabling removes the override instead of storing `true`.
        assert!(
            store
                .set_plugin_enabled(&id, "kdeconnect.notification", true)
                .await
        );
        let reloaded = TrustedStore::load(dir.path());
        let device = reloaded.get(&id).await.expect("device");
        assert!(
            device.plugins.is_empty(),
            "enabled plugins must not be stored"
        );

        // Unknown devices change nothing.
        assert!(
            !store
                .set_plugin_enabled(&"e".repeat(32), "kdeconnect.notification", false)
                .await
        );
        assert!(
            !store
                .plugin_enabled(&"e".repeat(32), "kdeconnect.notification")
                .await
        );
    }

    #[tokio::test]
    async fn removes_device_directory_on_unpair() {
        let dir = tempfile::tempdir().expect("tempdir");
        let store = TrustedStore::load(dir.path());
        let id = "f".repeat(32);
        store.insert(phone(&id, HashMap::new())).await;
        assert!(dir
            .path()
            .join("devices")
            .join(&id)
            .join("device.json")
            .is_file());

        store.remove(&id).await;
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        assert!(
            !dir.path().join("devices").join(&id).exists(),
            "unpair removes the whole device directory"
        );
        assert!(store.get(&id).await.is_none());
    }

    #[tokio::test]
    async fn ignores_unsafe_device_ids() {
        let dir = tempfile::tempdir().expect("tempdir");
        let store = TrustedStore::load(dir.path());
        store.insert(phone("../evil", HashMap::new())).await;

        assert!(
            !dir.path()
                .join("devices")
                .join(".._evil")
                .join("device.json")
                .exists()
                && !dir.path().parent().expect("parent").join("evil").exists(),
            "unsafe ids must not escape the devices directory"
        );
        assert!(store.get("../evil").await.is_some(), "kept in memory only");
    }

    #[tokio::test]
    async fn updates_last_ip_only_for_known_devices() {
        let dir = tempfile::tempdir().expect("tempdir");
        let store = TrustedStore::load(dir.path());
        let id = "b".repeat(32);
        let mut device = phone(&id, HashMap::new());
        device.last_ip = None;
        store.insert(device).await;

        assert!(!store.update_last_ip(&"c".repeat(32), "192.168.1.30").await);
        assert!(store.update_last_ip(&id, "192.168.1.30").await);
        assert!(!store.update_last_ip(&id, "192.168.1.30").await);

        let reloaded = TrustedStore::load(dir.path());
        let device = reloaded.get(&id).await.expect("device");
        assert_eq!(device.last_ip.as_deref(), Some("192.168.1.30"));
    }

    #[test]
    fn fingerprints_are_stable() {
        assert_eq!(fingerprint(b"abc"), fingerprint(b"abc"));
        assert_ne!(fingerprint(b"abc"), fingerprint(b"abd"));
    }
}
