//! Daemon-wide configuration persisted as `config.json`.
//!
//! Only the display name announced to other devices is configurable for now.
//! When no override is stored, the daemon falls back to the host name.

use std::io;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};
use tokio::sync::RwLock;

/// Configuration file inside the data directory.
const CONFIG_FILE: &str = "config.json";
/// Maximum accepted length for a device name, in characters.
pub const MAX_DEVICE_NAME_CHARS: usize = 128;

/// On-disk shape. Missing keys keep their defaults so old files load.
#[derive(Debug, Default, Serialize, Deserialize)]
struct ConfigFile {
    #[serde(rename = "deviceName", default)]
    device_name: Option<String>,
}

/// Persistent configuration of the daemon.
pub struct ConfigStore {
    /// `config.json` path, or `None` for the in-memory store used in tests.
    path: Option<PathBuf>,
    /// Name used when no override is configured (the host name).
    default_name: String,
    /// Configured display name; `None` means "use `default_name`".
    device_name: RwLock<Option<String>>,
}

impl ConfigStore {
    /// In-memory store (tests), with `default_name` as the fallback.
    pub fn in_memory(default_name: String) -> Self {
        Self {
            path: None,
            default_name,
            device_name: RwLock::new(None),
        }
    }

    /// Loads `<data_dir>/config.json`. A missing, unreadable or malformed file
    /// leaves the defaults in place instead of failing startup.
    pub fn load(data_dir: &Path, default_name: String) -> Self {
        let path = data_dir.join(CONFIG_FILE);
        let device_name = match std::fs::read(&path) {
            Ok(bytes) => match serde_json::from_slice::<ConfigFile>(&bytes) {
                Ok(config) => match normalize(config.device_name) {
                    Some(name) if name.chars().count() > MAX_DEVICE_NAME_CHARS => {
                        tracing::warn!(
                            path = %path.display(),
                            max = MAX_DEVICE_NAME_CHARS,
                            "ignoring configured device name that is too long"
                        );
                        None
                    }
                    name => name,
                },
                Err(e) => {
                    tracing::warn!(
                        path = %path.display(),
                        error = %e,
                        "ignoring malformed config file"
                    );
                    None
                }
            },
            Err(e) if e.kind() == io::ErrorKind::NotFound => None,
            Err(e) => {
                tracing::warn!(
                    path = %path.display(),
                    error = %e,
                    "cannot read config file"
                );
                None
            }
        };

        Self {
            path: Some(path),
            default_name,
            device_name: RwLock::new(device_name),
        }
    }

    /// Name the daemon announces: the configured override, or the fallback.
    pub async fn effective_name(&self) -> String {
        self.device_name
            .read()
            .await
            .clone()
            .unwrap_or_else(|| self.default_name.clone())
    }

    /// Configured override, if any.
    pub async fn configured_name(&self) -> Option<String> {
        self.device_name.read().await.clone()
    }

    /// Fallback used when the override is cleared.
    pub fn default_name(&self) -> &str {
        &self.default_name
    }

    /// Stores (or with `None` clears) the display name and persists the file.
    /// An in-memory store only updates the value.
    pub async fn set_device_name(&self, name: Option<String>) -> io::Result<()> {
        let name = normalize(name);
        if let Some(path) = &self.path {
            write_atomic(
                path,
                &ConfigFile {
                    device_name: name.clone(),
                },
            )
            .await?;
        }
        *self.device_name.write().await = name;
        Ok(())
    }
}

/// Trims a name and treats a blank value as "no override".
fn normalize(name: Option<String>) -> Option<String> {
    name.map(|name| name.trim().to_owned())
        .filter(|name| !name.is_empty())
}

/// Writes the configuration atomically: `config.json.tmp` followed by a
/// rename, so readers never observe a partially written file.
async fn write_atomic(path: &Path, config: &ConfigFile) -> io::Result<()> {
    let json = serde_json::to_vec_pretty(config).map_err(io::Error::other)?;
    let tmp = path.with_extension("json.tmp");
    if let Some(parent) = path.parent() {
        tokio::fs::create_dir_all(parent).await?;
    }
    tokio::fs::write(&tmp, json).await?;
    tokio::fs::rename(&tmp, path).await
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn missing_file_uses_the_default_name() {
        let dir = tempfile::tempdir().expect("temp dir");
        let store = ConfigStore::load(dir.path(), "fallback".to_owned());

        assert_eq!(store.effective_name().await, "fallback");
        assert_eq!(store.configured_name().await, None);
    }

    #[tokio::test]
    async fn loads_a_configured_name() {
        let dir = tempfile::tempdir().expect("temp dir");
        std::fs::write(
            dir.path().join(CONFIG_FILE),
            r#"{"deviceName": "  Zotta-PC  "}"#,
        )
        .expect("config file");
        let store = ConfigStore::load(dir.path(), "fallback".to_owned());

        assert_eq!(store.effective_name().await, "Zotta-PC");
    }

    #[tokio::test]
    async fn blank_or_malformed_config_falls_back_to_the_default() {
        let dir = tempfile::tempdir().expect("temp dir");
        std::fs::write(dir.path().join(CONFIG_FILE), r#"{"deviceName": "   "}"#)
            .expect("config file");
        let blank = ConfigStore::load(dir.path(), "fallback".to_owned());
        assert_eq!(blank.effective_name().await, "fallback");

        std::fs::write(dir.path().join(CONFIG_FILE), "{not json").expect("config file");
        let malformed = ConfigStore::load(dir.path(), "fallback".to_owned());
        assert_eq!(malformed.effective_name().await, "fallback");
    }

    #[tokio::test]
    async fn ignores_an_overlong_configured_name() {
        let dir = tempfile::tempdir().expect("temp dir");
        let long = "x".repeat(MAX_DEVICE_NAME_CHARS + 1);
        std::fs::write(
            dir.path().join(CONFIG_FILE),
            serde_json::json!({ "deviceName": long }).to_string(),
        )
        .expect("config file");
        let store = ConfigStore::load(dir.path(), "fallback".to_owned());

        assert_eq!(store.effective_name().await, "fallback");
    }

    #[tokio::test]
    async fn persists_and_clears_the_override() {
        let dir = tempfile::tempdir().expect("temp dir");
        let store = ConfigStore::load(dir.path(), "fallback".to_owned());

        store
            .set_device_name(Some("  New Name ".to_owned()))
            .await
            .expect("persist name");
        assert_eq!(store.effective_name().await, "New Name");

        let reloaded = ConfigStore::load(dir.path(), "fallback".to_owned());
        assert_eq!(reloaded.effective_name().await, "New Name");

        store.set_device_name(None).await.expect("clear name");
        assert_eq!(store.effective_name().await, "fallback");
        assert_eq!(store.configured_name().await, None);

        let reloaded = ConfigStore::load(dir.path(), "fallback".to_owned());
        assert_eq!(reloaded.effective_name().await, "fallback");
        assert!(
            !dir.path().join("config.json.tmp").exists(),
            "atomic write must not leave temporary files"
        );
    }

    #[tokio::test]
    async fn in_memory_store_does_not_touch_the_filesystem() {
        let store = ConfigStore::in_memory("fallback".to_owned());
        store
            .set_device_name(Some("Memory".to_owned()))
            .await
            .expect("set name");

        assert_eq!(store.effective_name().await, "Memory");
        assert_eq!(store.default_name(), "fallback");
    }
}
