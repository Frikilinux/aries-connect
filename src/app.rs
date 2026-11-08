//! Shared daemon state: device registry, events pushed to D-Bus and pending
//! pairing requests.

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;

use tokio::sync::{mpsc, oneshot, Mutex, Notify, RwLock};

use crate::config::{ConfigStore, MAX_DEVICE_NAME_CHARS};
use crate::identity::{pair_packet, IdentityBody, LocalIdentity, NetworkPacket};
use crate::plugins::battery::{self, BatteryStore};
use crate::plugins::clipboard::{self, ClipboardError};
use crate::plugins::connectivity_report::{self, ConnectivityStore};
use crate::plugins::find_my_phone;
use crate::plugins::notifications::{self, NotificationStore};
use crate::plugins::ping;
use crate::plugins::share;
use crate::tls::IdentityCertificate;
use crate::trusted::{TrustedDevice, TrustedStore};

/// Default cap for incoming shared files, in bytes (500 MiB).
pub const DEFAULT_MAX_INCOMING_SIZE: u64 = 500 * 1024 * 1024;

/// Events pushed toward D-Bus.
#[derive(Debug, Clone)]
pub enum Event {
    /// A device became visible via UDP.
    DeviceSeen { id: String, name: String },
    /// TCP/TLS connection established with a device.
    Connected { id: String },
    /// TCP/TLS connection closed.
    Disconnected { id: String },
    /// The peer requested pairing; must be accepted or rejected.
    PairingRequested { id: String, name: String },
    /// Pairing completed.
    PairingAccepted { id: String },
    /// Pairing rejected (by us, by the peer, or by timeout).
    PairingRejected { id: String, reason: String },
    /// The phone posted a notification.
    NotificationReceived {
        device_id: String,
        id: String,
        app_name: String,
        title: String,
        text: String,
    },
    /// A notification was dismissed, locally or on the phone.
    NotificationDismissed { device_id: String, id: String },
    /// A paired device pinged us, optionally with a message.
    PingReceived { device_id: String, message: String },
    /// The battery status of a device changed.
    BatteryChanged {
        device_id: String,
        level: i32,
        charging: bool,
    },
    /// The connectivity report of a device changed.
    ConnectivityChanged { device_id: String },
    /// The device clipboard changed. `timestamp` is `0` for regular updates
    /// and the connection timestamp for `kdeconnect.clipboard.connect`.
    ClipboardReceived {
        device_id: String,
        content: String,
        timestamp: i64,
    },
    /// A per-device plugin was enabled or disabled.
    PluginStateChanged {
        device_id: String,
        plugin: String,
        enabled: bool,
    },
    /// A device was unpaired (locally or from the phone). It stays known as
    /// an unpaired device, so the device object is kept and only `Paired`
    /// changes.
    PairingRemoved { id: String },
    /// A paired device shared text with us.
    ShareTextReceived { device_id: String, text: String },
    /// A paired device shared a URL with us.
    ShareUrlReceived { device_id: String, url: String },
    /// A shared file finished downloading; `path` is its absolute location.
    ShareFileReceived { device_id: String, path: String },
    /// Progress of an incoming multi-file share (bytes and files so far).
    ShareProgress {
        device_id: String,
        bytes: u64,
        total_bytes: u64,
        files: u32,
        total_files: u32,
    },
    /// The per-device download directory changed.
    DownloadDirChanged { device_id: String },
    /// The name this daemon announces to other devices changed.
    LocalNameChanged { name: String },
}

/// Commands arriving from the D-Bus interface toward the core.
#[derive(Debug)]
pub enum UiCommand {
    AcceptPairing(String),
    RejectPairing(String),
    Unpair(String),
    DismissNotification {
        device_id: String,
        notification_id: String,
        /// Result of the dismissal, sent back to the D-Bus caller.
        reply: oneshot::Sender<Result<(), DismissError>>,
    },
    Ping {
        device_id: String,
        message: String,
        /// Result of the delivery attempt, sent back to the D-Bus caller.
        reply: oneshot::Sender<Result<(), PingError>>,
    },
    FindMyPhone {
        device_id: String,
        /// Result of the delivery attempt, sent back to the D-Bus caller.
        reply: oneshot::Sender<Result<(), FindMyPhoneError>>,
    },
    SendClipboard {
        device_id: String,
        content: String,
        /// `None` sends `kdeconnect.clipboard`; `Some(timestamp)` sends
        /// `kdeconnect.clipboard.connect`.
        timestamp: Option<i64>,
        /// Result of the delivery attempt, sent back to the D-Bus caller.
        reply: oneshot::Sender<Result<(), ClipboardError>>,
    },
    SetPluginEnabled {
        device_id: String,
        plugin: String,
        enabled: bool,
        /// Result of the toggle, sent back to the D-Bus caller.
        reply: oneshot::Sender<Result<(), PluginError>>,
    },
    SetDeviceName {
        name: String,
        /// Effective name after the change, sent back to the D-Bus caller.
        reply: oneshot::Sender<Result<(), DeviceNameError>>,
    },
    SendFile {
        device_id: String,
        path: String,
        /// Result of the delivery attempt, sent back to the D-Bus caller.
        reply: oneshot::Sender<Result<(), SendShareError>>,
    },
    SendText {
        device_id: String,
        text: String,
        /// Result of the delivery attempt, sent back to the D-Bus caller.
        reply: oneshot::Sender<Result<(), SendShareError>>,
    },
    SendUrl {
        device_id: String,
        url: String,
        /// Result of the delivery attempt, sent back to the D-Bus caller.
        reply: oneshot::Sender<Result<(), SendShareError>>,
    },
    SetDownloadDir {
        device_id: String,
        dir: String,
        /// Result of the change, sent back to the D-Bus caller.
        reply: oneshot::Sender<Result<(), DownloadDirError>>,
    },
}

/// Decision on a pairing request.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PairDecision {
    Accept,
    Reject,
}

/// Why a ping could not be delivered.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PingError {
    /// The device is unknown or not paired.
    NotPaired,
    /// The `kdeconnect.ping` plugin is disabled for the device.
    PluginDisabled,
    /// The device has no active control channel.
    NotConnected,
}

impl std::fmt::Display for PingError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let message = match self {
            Self::NotPaired => "device is not paired",
            Self::PluginDisabled => "kdeconnect.ping is disabled for this device",
            Self::NotConnected => "device has no active control channel",
        };
        f.write_str(message)
    }
}

impl std::error::Error for PingError {}

/// Why a find-my-phone request could not be delivered.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FindMyPhoneError {
    /// The device is unknown or not paired.
    NotPaired,
    /// The `kdeconnect.findmyphone.request` plugin is disabled for the device.
    PluginDisabled,
    /// The device has no active control channel.
    NotConnected,
}

impl std::fmt::Display for FindMyPhoneError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let message = match self {
            Self::NotPaired => "device is not paired",
            Self::PluginDisabled => "kdeconnect.findmyphone.request is disabled for this device",
            Self::NotConnected => "device has no active control channel",
        };
        f.write_str(message)
    }
}

impl std::error::Error for FindMyPhoneError {}

/// Why a notification dismissal was rejected.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DismissError {
    /// The device is unknown or not paired.
    NotPaired,
    /// The notification is not in the active set.
    UnknownNotification,
    /// The `kdeconnect.notification` plugin is disabled for the device.
    PluginDisabled,
}

impl std::fmt::Display for DismissError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let message = match self {
            Self::NotPaired => "device is not paired",
            Self::UnknownNotification => "notification is not active",
            Self::PluginDisabled => "kdeconnect.notification is disabled for this device",
        };
        f.write_str(message)
    }
}

impl std::error::Error for DismissError {}

/// Why a plugin toggle was rejected.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PluginError {
    /// The device is unknown or not paired.
    UnknownDevice,
    /// The daemon does not implement that plugin.
    UnsupportedPlugin,
}

impl std::fmt::Display for PluginError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let message = match self {
            Self::UnknownDevice => "device is not paired",
            Self::UnsupportedPlugin => "plugin is not implemented by the daemon",
        };
        f.write_str(message)
    }
}

impl std::error::Error for PluginError {}

/// Why a device name was rejected.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DeviceNameError {
    /// The name exceeds [`MAX_DEVICE_NAME_CHARS`].
    TooLong { max: usize, actual: usize },
    /// The new name could not be written to `config.json`.
    Persist(String),
}

impl std::fmt::Display for DeviceNameError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::TooLong { max, actual } => {
                write!(
                    f,
                    "device name is {actual} characters; the maximum is {max}"
                )
            }
            Self::Persist(e) => write!(f, "cannot persist the device name: {e}"),
        }
    }
}

impl std::error::Error for DeviceNameError {}

/// Why a share request could not be delivered.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SendShareError {
    /// The device is unknown or not paired.
    NotPaired,
    /// The `kdeconnect.share.request` plugin is disabled for the device.
    PluginDisabled,
    /// The device has no active control channel.
    NotConnected,
    /// The local file is missing, unreadable or the payload could not be set up.
    FileError,
}

impl std::fmt::Display for SendShareError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let message = match self {
            Self::NotPaired => "device is not paired",
            Self::PluginDisabled => "kdeconnect.share.request is disabled for this device",
            Self::NotConnected => "device has no active control channel",
            Self::FileError => "cannot read the file or set up the transfer",
        };
        f.write_str(message)
    }
}

impl std::error::Error for SendShareError {}

/// Why a download directory change was rejected.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DownloadDirError {
    /// The device is unknown or not paired.
    UnknownDevice,
}

impl std::fmt::Display for DownloadDirError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::UnknownDevice => f.write_str("device is not paired"),
        }
    }
}

impl std::error::Error for DownloadDirError {}

/// Progress of an incoming share batch for one device.
#[derive(Debug, Clone, Copy, Default)]
pub struct ShareBatch {
    /// Bytes of completed files plus the current transfer.
    pub bytes_done: u64,
    /// Files already written to disk.
    pub files_done: u32,
    /// Total bytes announced for the batch.
    pub total_bytes: u64,
    /// Total files announced for the batch.
    pub total_files: u32,
}

/// Per-device progress of incoming share batches.
#[derive(Default)]
pub struct ShareStore {
    inner: RwLock<HashMap<String, ShareBatch>>,
}

impl ShareStore {
    pub fn new() -> Self {
        Self::default()
    }

    /// Current batch state, or a zeroed one when no batch is in progress.
    pub async fn get(&self, device_id: &str) -> ShareBatch {
        self.inner
            .read()
            .await
            .get(device_id)
            .copied()
            .unwrap_or_default()
    }

    /// Sets the announced batch totals, keeping any progress already recorded.
    pub async fn ensure_batch(
        &self,
        device_id: &str,
        total_files: u32,
        total_bytes: u64,
    ) -> ShareBatch {
        let mut map = self.inner.write().await;
        let entry = map.entry(device_id.to_owned()).or_default();
        if total_files > 0 {
            entry.total_files = total_files;
        }
        if total_bytes > 0 {
            entry.total_bytes = total_bytes;
        }
        *entry
    }

    /// Records one finished file and returns the updated batch. The batch is
    /// cleared once every announced file has been received.
    pub async fn file_done(&self, device_id: &str, size: u64) -> ShareBatch {
        let mut map = self.inner.write().await;
        let entry = map.entry(device_id.to_owned()).or_default();
        entry.files_done += 1;
        entry.bytes_done += size;
        let snapshot = *entry;
        if snapshot.total_files > 0 && snapshot.files_done >= snapshot.total_files {
            map.remove(device_id);
        }
        snapshot
    }

    /// Drops the batch state of a device.
    pub async fn clear_device(&self, device_id: &str) {
        self.inner.write().await.remove(device_id);
    }
}

/// Registry of known devices.
pub struct DeviceRegistry {
    devices: RwLock<HashMap<String, Device>>,
}

pub struct Device {
    pub id: String,
    pub name: String,
    pub device_type: String,
    pub protocol_version: i64,
    pub paired: bool,
    pub connected: bool,
}

impl Clone for Device {
    fn clone(&self) -> Self {
        Self {
            id: self.id.clone(),
            name: self.name.clone(),
            device_type: self.device_type.clone(),
            protocol_version: self.protocol_version,
            paired: self.paired,
            connected: self.connected,
        }
    }
}

impl Default for DeviceRegistry {
    fn default() -> Self {
        Self::new()
    }
}

impl DeviceRegistry {
    pub fn new() -> Self {
        Self {
            devices: RwLock::new(HashMap::new()),
        }
    }

    /// Inserts or updates a device from its identity. Returns
    /// `(created, renamed)`.
    pub async fn upsert_identity(&self, body: &IdentityBody) -> (bool, bool) {
        let mut map = self.devices.write().await;
        match map.get_mut(&body.device_id) {
            Some(dev) => {
                let renamed = dev.name != body.device_name;
                dev.name = body.device_name.clone();
                dev.device_type = body.device_type.clone();
                dev.protocol_version = body.protocol_version;
                (false, renamed)
            }
            None => {
                map.insert(
                    body.device_id.clone(),
                    Device {
                        id: body.device_id.clone(),
                        name: body.device_name.clone(),
                        device_type: body.device_type.clone(),
                        protocol_version: body.protocol_version,
                        paired: false,
                        connected: false,
                    },
                );
                (true, false)
            }
        }
    }

    pub async fn set_paired(&self, id: &str, paired: bool) {
        if let Some(d) = self.devices.write().await.get_mut(id) {
            d.paired = paired;
        }
    }

    pub async fn set_connected(&self, id: &str, connected: bool) {
        if let Some(d) = self.devices.write().await.get_mut(id) {
            d.connected = connected;
        }
    }

    pub async fn get(&self, id: &str) -> Option<Device> {
        self.devices.read().await.get(id).cloned()
    }

    /// Ensures a registry entry exists (used to seed persisted pairings).
    pub async fn ensure(&self, id: &str, name: &str, paired: bool) {
        let mut map = self.devices.write().await;
        match map.get_mut(id) {
            Some(dev) => dev.paired = paired,
            None => {
                map.insert(
                    id.to_owned(),
                    Device {
                        id: id.to_owned(),
                        name: name.to_owned(),
                        device_type: "unknown".to_owned(),
                        protocol_version: 0,
                        paired,
                        connected: false,
                    },
                );
            }
        }
    }

    pub async fn list(&self) -> Vec<Device> {
        self.devices.read().await.values().cloned().collect()
    }
}

/// Active control channel for a device. The phone replaces its socket every
/// time it gets a discovery broadcast, so replies must go to the newest link.
struct LinkHandle {
    generation: u64,
    tx: mpsc::UnboundedSender<NetworkPacket>,
    /// SHA-256 of the peer certificate presented on this link.
    cert_sha256: Option<String>,
}

/// Shared daemon state, cloned into every task as an `Arc`.
pub struct App {
    /// Local identity announced over UDP and TLS. Its name is mutable.
    pub local: RwLock<LocalIdentity>,
    pub registry: DeviceRegistry,
    pub trusted: TrustedStore,
    /// Persistent daemon configuration (display name).
    pub config: ConfigStore,
    /// Channel of events toward D-Bus.
    pub events: mpsc::UnboundedSender<Event>,
    /// Active notifications mirrored from paired devices.
    pub notifications: NotificationStore,
    /// Last reported battery status of each device.
    pub battery: BatteryStore,
    /// Last reported connectivity report of each device.
    pub connectivity: ConnectivityStore,
    /// Progress of incoming share batches per device.
    pub share: ShareStore,
    /// TLS identity used for payload transfers; set once at startup.
    certificate: RwLock<Option<Arc<IdentityCertificate>>>,
    /// Pending pairing requests: deviceId -> decision sender.
    pending_pairs: Mutex<HashMap<String, oneshot::Sender<PairDecision>>>,
    /// Latest control channel per device.
    links: Mutex<HashMap<String, LinkHandle>>,
    link_generation: AtomicU64,
    /// Wakes the UDP announce loop after a local name change.
    announce_wakeup: Notify,
}

impl App {
    /// In-memory configuration (tests and embedders that do not persist it).
    pub fn new(
        local: LocalIdentity,
        events: mpsc::UnboundedSender<Event>,
        trusted: TrustedStore,
    ) -> Arc<Self> {
        let config = ConfigStore::in_memory(local.device_name.clone());
        Self::with_config(local, events, trusted, config)
    }

    /// Builds the shared state with an explicit configuration store.
    pub fn with_config(
        local: LocalIdentity,
        events: mpsc::UnboundedSender<Event>,
        trusted: TrustedStore,
        config: ConfigStore,
    ) -> Arc<Self> {
        Arc::new(Self {
            local: RwLock::new(local),
            registry: DeviceRegistry::new(),
            trusted,
            config,
            events,
            notifications: NotificationStore::new(),
            battery: BatteryStore::new(),
            connectivity: ConnectivityStore::new(),
            share: ShareStore::new(),
            certificate: RwLock::new(None),
            pending_pairs: Mutex::new(HashMap::new()),
            links: Mutex::new(HashMap::new()),
            link_generation: AtomicU64::new(0),
            announce_wakeup: Notify::new(),
        })
    }

    /// Stores the TLS identity used for payload transfers. Called once at
    /// startup (and by tests) before any file is shared.
    pub async fn set_certificate(&self, cert: IdentityCertificate) {
        *self.certificate.write().await = Some(Arc::new(cert));
    }

    /// TLS identity used for payload transfers, when configured.
    pub async fn certificate(&self) -> Option<Arc<IdentityCertificate>> {
        self.certificate.read().await.clone()
    }

    /// Clone of the current local identity.
    pub async fn local_identity(&self) -> LocalIdentity {
        self.local.read().await.clone()
    }

    /// Device id of this daemon.
    pub async fn local_id(&self) -> String {
        self.local.read().await.device_id.clone()
    }

    /// Name this daemon announces to other devices.
    pub async fn local_name(&self) -> String {
        self.local.read().await.device_name.clone()
    }

    /// Identity packet with the current name and capabilities.
    pub async fn local_packet(&self) -> NetworkPacket {
        self.local.read().await.packet()
    }

    /// Wakes the UDP announce loop so a new identity is broadcast immediately.
    pub fn request_announce(&self) {
        self.announce_wakeup.notify_one();
    }

    /// Resolves when [`App::request_announce`] is called.
    pub async fn wait_for_announce(&self) {
        self.announce_wakeup.notified().await;
    }

    /// Changes the name announced to other devices and persists it.
    ///
    /// A blank name clears the override and falls back to the host name. The
    /// identity is re-sent over every active control channel and the UDP
    /// announce loop wakes up, so peers see the new name without reconnecting.
    pub async fn set_local_name(&self, name: &str) -> Result<String, DeviceNameError> {
        let trimmed = name.trim();
        let actual = trimmed.chars().count();
        if actual > MAX_DEVICE_NAME_CHARS {
            return Err(DeviceNameError::TooLong {
                max: MAX_DEVICE_NAME_CHARS,
                actual,
            });
        }
        let configured = if trimmed.is_empty() {
            None
        } else {
            Some(trimmed.to_owned())
        };
        self.config
            .set_device_name(configured)
            .await
            .map_err(|e| DeviceNameError::Persist(e.to_string()))?;

        let effective = self.config.effective_name().await;
        let changed = {
            let mut local = self.local.write().await;
            if local.device_name == effective {
                false
            } else {
                local.device_name = effective.clone();
                true
            }
        };
        if changed {
            self.broadcast_identity().await;
            self.request_announce();
            self.emit(Event::LocalNameChanged {
                name: effective.clone(),
            });
        }
        Ok(effective)
    }

    /// Sends the current identity over every active control channel.
    async fn broadcast_identity(&self) {
        let packet = self.local_packet().await;
        let senders: Vec<(String, mpsc::UnboundedSender<NetworkPacket>)> = {
            let links = self.links.lock().await;
            links
                .iter()
                .map(|(id, link)| (id.clone(), link.tx.clone()))
                .collect()
        };
        for (id, tx) in senders {
            if let Err(e) = tx.send(packet.clone()) {
                tracing::debug!(device = %id, error = %e, "cannot send identity update");
            }
        }
    }

    /// Seeds the registry from the persisted trust store.
    pub async fn seed_trusted(&self) {
        for device in self.trusted.all().await {
            self.registry.ensure(&device.id, &device.name, true).await;
        }
    }

    /// Marks a device as paired and persists it together with the certificate
    /// fingerprint of its current link.
    pub async fn mark_paired(&self, id: &str) {
        let name = self
            .registry
            .get(id)
            .await
            .map(|d| d.name)
            .unwrap_or_else(|| id.to_owned());
        let cert_sha256 = {
            let links = self.links.lock().await;
            links.get(id).and_then(|l| l.cert_sha256.clone())
        };
        self.registry.set_paired(id, true).await;
        self.trusted
            .insert(TrustedDevice {
                id: id.to_owned(),
                name,
                cert_sha256,
                last_ip: None,
                download_dir: None,
                max_incoming_size: None,
                plugins: HashMap::new(),
            })
            .await;
    }

    /// Forgets a pairing (peer unpair or explicit removal).
    ///
    /// When `notify_peer` is set, the peer receives `kdeconnect.pair: false`
    /// so it drops its side of the pairing too (local-initiated unpair). Peer
    /// initiated unpair passes `false` to avoid echoing the packet back.
    /// Returns true when the device was paired and removed.
    pub async fn unpair(&self, id: &str, notify_peer: bool) -> bool {
        let was_paired = self.trusted.get(id).await.is_some();
        if notify_peer && !self.send_packet(id, &pair_packet(false)).await {
            // The phone may be offline; local state is cleared regardless.
            tracing::debug!(device = %id, "no active control channel to notify unpair");
        }
        self.registry.set_paired(id, false).await;
        self.notifications.clear_device(id).await;
        self.battery.clear_device(id).await;
        self.connectivity.clear_device(id).await;
        self.trusted.remove(id).await;
        let _ = self
            .events
            .send(Event::PairingRemoved { id: id.to_owned() });
        was_paired
    }

    /// Dismisses a notification locally and asks the phone to do the same.
    /// Fails when the device is not paired, the notification is unknown or the
    /// notifications plugin is disabled for that device.
    pub async fn dismiss_notification(
        &self,
        device_id: &str,
        notification_id: &str,
    ) -> Result<(), DismissError> {
        if self.trusted.get(device_id).await.is_none() {
            tracing::debug!(device = %device_id, "cannot dismiss for an unpaired device");
            return Err(DismissError::NotPaired);
        }
        if !self
            .trusted
            .plugin_enabled(device_id, notifications::NOTIFICATION_PACKET)
            .await
        {
            tracing::debug!(
                device = %device_id,
                "notifications plugin is disabled for this device"
            );
            return Err(DismissError::PluginDisabled);
        }
        if self
            .notifications
            .remove(device_id, notification_id)
            .await
            .is_none()
        {
            return Err(DismissError::UnknownNotification);
        }
        let _ = self.events.send(Event::NotificationDismissed {
            device_id: device_id.to_owned(),
            id: notification_id.to_owned(),
        });
        if !self
            .send_packet(device_id, &notifications::dismiss_packet(notification_id))
            .await
        {
            tracing::debug!(
                device = %device_id,
                "no active control channel to forward the dismissal"
            );
        }
        Ok(())
    }

    /// Sends a ping to a device. Fails when the device is not paired, the
    /// `kdeconnect.ping` plugin is disabled for it or there is no active
    /// control channel.
    pub async fn send_ping(&self, device_id: &str, message: &str) -> Result<(), PingError> {
        if self.trusted.get(device_id).await.is_none() {
            tracing::debug!(device = %device_id, "cannot ping an unpaired device");
            return Err(PingError::NotPaired);
        }
        if !self
            .trusted
            .plugin_enabled(device_id, ping::PING_PACKET)
            .await
        {
            tracing::debug!(device = %device_id, "ping plugin is disabled for this device");
            return Err(PingError::PluginDisabled);
        }
        if !self
            .send_packet(device_id, &ping::ping_packet(message))
            .await
        {
            tracing::debug!(
                device = %device_id,
                "no active control channel to send the ping"
            );
            return Err(PingError::NotConnected);
        }
        Ok(())
    }

    /// Asks a device to announce itself (ring). Fails when the device is not
    /// paired, the `kdeconnect.findmyphone.request` plugin is disabled for it
    /// or there is no active control channel.
    pub async fn send_find_my_phone(&self, device_id: &str) -> Result<(), FindMyPhoneError> {
        if self.trusted.get(device_id).await.is_none() {
            tracing::debug!(device = %device_id, "cannot find an unpaired device");
            return Err(FindMyPhoneError::NotPaired);
        }
        if !self
            .trusted
            .plugin_enabled(device_id, find_my_phone::FIND_MY_PHONE_PACKET)
            .await
        {
            tracing::debug!(device = %device_id, "find-my-phone plugin is disabled for this device");
            return Err(FindMyPhoneError::PluginDisabled);
        }
        if !self
            .send_packet(device_id, &find_my_phone::request_packet())
            .await
        {
            tracing::debug!(
                device = %device_id,
                "no active control channel to send the find-my-phone request"
            );
            return Err(FindMyPhoneError::NotConnected);
        }
        Ok(())
    }

    /// Sends text to a device as a `kdeconnect.share.request`. Empty text is
    /// ignored. Fails when the device is not paired, the plugin is disabled
    /// or there is no control channel.
    pub async fn send_share_text(&self, device_id: &str, text: &str) -> Result<(), SendShareError> {
        if text.is_empty() {
            return Ok(());
        }
        self.send_share(device_id, share::text_packet(text)).await
    }

    /// Sends a URL to a device as a `kdeconnect.share.request`. Empty URLs are
    /// ignored.
    pub async fn send_share_url(&self, device_id: &str, url: &str) -> Result<(), SendShareError> {
        if url.is_empty() {
            return Ok(());
        }
        self.send_share(device_id, share::url_packet(url)).await
    }

    async fn send_share(
        &self,
        device_id: &str,
        packet: NetworkPacket,
    ) -> Result<(), SendShareError> {
        if self.trusted.get(device_id).await.is_none() {
            tracing::debug!(device = %device_id, "cannot share to an unpaired device");
            return Err(SendShareError::NotPaired);
        }
        if !self
            .trusted
            .plugin_enabled(device_id, share::SHARE_PACKET)
            .await
        {
            tracing::debug!(device = %device_id, "share plugin is disabled for this device");
            return Err(SendShareError::PluginDisabled);
        }
        if !self.send_packet(device_id, &packet).await {
            tracing::debug!(device = %device_id, "no active control channel to share");
            return Err(SendShareError::NotConnected);
        }
        Ok(())
    }

    /// Sends a local file to a device. The bytes are served over an ephemeral
    /// payload listener announced in the request. Fails when the device is not
    /// paired, the plugin is disabled, there is no control channel or the file
    /// is unreadable.
    pub async fn send_share_file(&self, device_id: &str, path: &str) -> Result<(), SendShareError> {
        if self.trusted.get(device_id).await.is_none() {
            tracing::debug!(device = %device_id, "cannot share to an unpaired device");
            return Err(SendShareError::NotPaired);
        }
        if !self
            .trusted
            .plugin_enabled(device_id, share::SHARE_PACKET)
            .await
        {
            tracing::debug!(device = %device_id, "share plugin is disabled for this device");
            return Err(SendShareError::PluginDisabled);
        }
        let path = PathBuf::from(path);
        let metadata = tokio::fs::metadata(&path)
            .await
            .map_err(|_| SendShareError::FileError)?;
        if !metadata.is_file() {
            return Err(SendShareError::FileError);
        }
        let size = metadata.len();
        let filename = path
            .file_name()
            .and_then(|name| name.to_str())
            .ok_or(SendShareError::FileError)?
            .to_owned();
        let Some(cert) = self.certificate().await else {
            tracing::warn!(device = %device_id, "no TLS identity configured for payload transfers");
            return Err(SendShareError::FileError);
        };
        let listener = crate::core::payload::bind_payload_listener()
            .await
            .map_err(|_| SendShareError::FileError)?;
        let port = listener
            .local_addr()
            .map_err(|_| SendShareError::FileError)?
            .port();
        let last_modified = metadata.modified().ok().and_then(|time| {
            time.duration_since(std::time::UNIX_EPOCH)
                .ok()
                .map(|d| d.as_millis() as i64)
        });
        let packet = share::file_packet(&filename, size, port, last_modified);
        if !self.send_packet(device_id, &packet).await {
            return Err(SendShareError::NotConnected);
        }
        tracing::info!(
            device = %device_id,
            filename = %filename,
            size,
            "sending shared file"
        );
        tokio::spawn(async move {
            if let Err(e) = crate::core::payload::send_payload(&cert, listener, &path, size).await {
                tracing::warn!(path = %path.display(), error = %e, "outgoing share failed");
            }
        });
        Ok(())
    }

    /// Sets (or with an empty `dir` clears) the per-device download directory.
    pub async fn set_download_dir(
        &self,
        device_id: &str,
        dir: &str,
    ) -> Result<(), DownloadDirError> {
        if self.trusted.get(device_id).await.is_none() {
            return Err(DownloadDirError::UnknownDevice);
        }
        let dir = dir.trim();
        let dir = if dir.is_empty() {
            None
        } else {
            Some(dir.to_owned())
        };
        if self.trusted.set_download_dir(device_id, dir).await {
            tracing::info!(device = %device_id, "download directory changed");
            self.emit(Event::DownloadDirChanged {
                device_id: device_id.to_owned(),
            });
        }
        Ok(())
    }

    /// Effective download directory for a device: its override, else the XDG
    /// download dir, else `$HOME/Downloads`. The directory is created if it
    /// does not exist.
    pub async fn effective_download_dir(&self, device_id: &str) -> std::io::Result<PathBuf> {
        let dir = match self.trusted.download_dir(device_id).await {
            Some(dir) => PathBuf::from(dir),
            None => default_download_dir(),
        };
        tokio::fs::create_dir_all(&dir).await?;
        Ok(dir)
    }

    /// Effective cap for incoming shared files, in bytes.
    pub async fn effective_max_size(&self, device_id: &str) -> u64 {
        self.trusted
            .max_incoming_size(device_id)
            .await
            .or_else(|| self.config.max_incoming_size())
            .unwrap_or(DEFAULT_MAX_INCOMING_SIZE)
    }

    /// Persists the per-device plugin state. Disabling a plugin also drops its
    /// active state (mirrored notifications) so clients resync once reenabled.
    /// Fails for unpaired devices and for plugins the daemon does not
    /// implement; setting the current value is a no-op.
    pub async fn set_plugin_enabled(
        &self,
        device_id: &str,
        plugin: &str,
        enabled: bool,
    ) -> Result<(), PluginError> {
        if self.trusted.get(device_id).await.is_none() {
            tracing::debug!(device = %device_id, "plugin toggle for an unpaired device");
            return Err(PluginError::UnknownDevice);
        }
        if !crate::plugins::supported_plugins().contains(&plugin) {
            tracing::debug!(device = %device_id, plugin = %plugin, "unknown plugin");
            return Err(PluginError::UnsupportedPlugin);
        }
        if !enabled {
            if plugin == notifications::NOTIFICATION_PACKET {
                self.notifications.clear_device(device_id).await;
            }
            if plugin == battery::BATTERY_PACKET {
                self.battery.clear_device(device_id).await;
            }
            if plugin == connectivity_report::CONNECTIVITY_REPORT_PACKET {
                self.connectivity.clear_device(device_id).await;
            }
        }
        if self
            .trusted
            .set_plugin_enabled(device_id, plugin, enabled)
            .await
        {
            tracing::info!(device = %device_id, plugin = %plugin, %enabled, "plugin state changed");
            self.emit(Event::PluginStateChanged {
                device_id: device_id.to_owned(),
                plugin: plugin.to_owned(),
                enabled,
            });
        }
        Ok(())
    }

    /// Persists the last address of a paired device so unicast discovery can
    /// reach it after a daemon restart. Unpaired devices are ignored.
    pub async fn remember_peer_address(&self, id: &str, ip: &str) {
        if self.trusted.get(id).await.is_none() {
            return;
        }
        if self.trusted.update_last_ip(id, ip).await {
            tracing::debug!(device = %id, %ip, "updated last known address");
        }
    }

    /// Registers the sender of a freshly established control channel and
    /// replaces any previous link for that device. Returns a generation that
    /// must be passed to `unregister_link` on teardown.
    pub async fn register_link(
        &self,
        id: &str,
        tx: mpsc::UnboundedSender<NetworkPacket>,
        cert_sha256: Option<String>,
    ) -> u64 {
        let generation = self.link_generation.fetch_add(1, Ordering::Relaxed) + 1;
        let mut links = self.links.lock().await;
        links.insert(
            id.to_owned(),
            LinkHandle {
                generation,
                tx,
                cert_sha256,
            },
        );
        generation
    }

    /// Removes a link only if it is still the newest one for that device.
    /// Returns whether it was current.
    pub async fn unregister_link(&self, id: &str, generation: u64) -> bool {
        let mut links = self.links.lock().await;
        if links
            .get(id)
            .is_some_and(|link| link.generation == generation)
        {
            links.remove(id);
            return true;
        }
        false
    }

    /// Sends a packet through the device's current control channel.
    /// Returns false when the device has no active link.
    pub async fn send_packet(&self, id: &str, packet: &NetworkPacket) -> bool {
        let links = self.links.lock().await;
        links
            .get(id)
            .is_some_and(|link| link.tx.send(packet.clone()).is_ok())
    }

    fn emit(&self, event: Event) {
        // A missing receiver (tests) is not an error.
        let _ = self.events.send(event);
    }

    /// Marks a device as seen; emits `DeviceSeen` when it is new and returns
    /// whether it was newly created.
    pub async fn seen(&self, body: &IdentityBody, notify: bool) -> bool {
        let (created, _renamed) = self.registry.upsert_identity(body).await;
        if created || notify {
            self.emit(Event::DeviceSeen {
                id: body.device_id.clone(),
                name: body.device_name.clone(),
            });
        }
        created
    }

    /// Registers a pairing request; returns a channel carrying the user's
    /// decision (Accept/Reject).
    pub async fn register_pair_request(&self, id: String) -> oneshot::Receiver<PairDecision> {
        let (tx, rx) = oneshot::channel();
        let mut map = self.pending_pairs.lock().await;
        // If there was already a pending request from the same peer, cancel it.
        if let Some(old) = map.insert(id.clone(), tx) {
            tracing::warn!(device = %id, "cancelling previous pending pair request");
            let _ = old.send(PairDecision::Reject);
        }
        drop(map);
        let name = self
            .registry
            .get(&id)
            .await
            .map(|d| d.name)
            .unwrap_or_else(|| id.clone());
        self.emit(Event::PairingRequested { id, name });
        rx
    }

    /// Resolves the pending request with a decision. `false` when there was
    /// no pending request for that device.
    pub async fn decide_pairing(&self, id: &str, decision: PairDecision) -> bool {
        let tx = self.pending_pairs.lock().await.remove(id);
        match tx {
            Some(tx) => tx.send(decision).is_ok(),
            None => false,
        }
    }
}

/// XDG download directory, falling back to `$HOME/Downloads` and finally the
/// current directory.
fn default_download_dir() -> PathBuf {
    dirs::download_dir().unwrap_or_else(|| {
        dirs::home_dir()
            .unwrap_or_else(|| PathBuf::from("."))
            .join("Downloads")
    })
}

/// Loop consuming UI commands and translating them into decisions.
pub async fn command_loop(app: Arc<App>, mut rx: mpsc::UnboundedReceiver<UiCommand>) {
    while let Some(cmd) = rx.recv().await {
        match cmd {
            UiCommand::AcceptPairing(id) => {
                tracing::info!(device = %id, "accept_pairing");
                if !app.decide_pairing(&id, PairDecision::Accept).await {
                    tracing::warn!(device = %id, "no pending pair request");
                }
            }
            UiCommand::RejectPairing(id) => {
                tracing::info!(device = %id, "reject_pairing");
                if !app.decide_pairing(&id, PairDecision::Reject).await {
                    tracing::warn!(device = %id, "no pending pair request");
                }
            }
            UiCommand::Unpair(id) => {
                tracing::info!(device = %id, "unpair");
                if !app.unpair(&id, true).await {
                    tracing::warn!(device = %id, "device was not paired");
                }
            }
            UiCommand::DismissNotification {
                device_id,
                notification_id,
                reply,
            } => {
                tracing::debug!(
                    device = %device_id,
                    notification = %notification_id,
                    "dismiss_notification"
                );
                let result = app.dismiss_notification(&device_id, &notification_id).await;
                if let Err(e) = &result {
                    tracing::warn!(
                        device = %device_id,
                        notification = %notification_id,
                        error = %e,
                        "cannot dismiss notification"
                    );
                }
                let _ = reply.send(result);
            }
            UiCommand::Ping {
                device_id,
                message,
                reply,
            } => {
                tracing::info!(device = %device_id, "ping");
                let result = app.send_ping(&device_id, &message).await;
                if let Err(e) = &result {
                    tracing::warn!(device = %device_id, error = %e, "cannot ping");
                }
                let _ = reply.send(result);
            }
            UiCommand::FindMyPhone { device_id, reply } => {
                tracing::info!(device = %device_id, "find_my_phone");
                let result = app.send_find_my_phone(&device_id).await;
                if let Err(e) = &result {
                    tracing::warn!(device = %device_id, error = %e, "cannot find my phone");
                }
                let _ = reply.send(result);
            }
            UiCommand::SendFile {
                device_id,
                path,
                reply,
            } => {
                tracing::info!(device = %device_id, path = %path, "send_file");
                let result = app.send_share_file(&device_id, &path).await;
                if let Err(e) = &result {
                    tracing::warn!(device = %device_id, error = %e, "cannot share file");
                }
                let _ = reply.send(result);
            }
            UiCommand::SendText {
                device_id,
                text,
                reply,
            } => {
                tracing::info!(device = %device_id, "send_text");
                let result = app.send_share_text(&device_id, &text).await;
                if let Err(e) = &result {
                    tracing::warn!(device = %device_id, error = %e, "cannot share text");
                }
                let _ = reply.send(result);
            }
            UiCommand::SendUrl {
                device_id,
                url,
                reply,
            } => {
                tracing::info!(device = %device_id, "send_url");
                let result = app.send_share_url(&device_id, &url).await;
                if let Err(e) = &result {
                    tracing::warn!(device = %device_id, error = %e, "cannot share url");
                }
                let _ = reply.send(result);
            }
            UiCommand::SetDownloadDir {
                device_id,
                dir,
                reply,
            } => {
                tracing::info!(device = %device_id, dir = %dir, "set_download_dir");
                let result = app.set_download_dir(&device_id, &dir).await;
                if let Err(e) = &result {
                    tracing::warn!(device = %device_id, error = %e, "cannot set download directory");
                }
                let _ = reply.send(result);
            }
            UiCommand::SendClipboard {
                device_id,
                content,
                timestamp,
                reply,
            } => {
                tracing::info!(
                    device = %device_id,
                    bytes = content.len(),
                    connect = timestamp.is_some(),
                    "send_clipboard"
                );
                let result = match timestamp {
                    Some(timestamp) => {
                        clipboard::send_connect(&app, &device_id, &content, timestamp).await
                    }
                    None => clipboard::send(&app, &device_id, &content).await,
                };
                if let Err(e) = &result {
                    tracing::warn!(device = %device_id, error = %e, "cannot send clipboard");
                }
                let _ = reply.send(result);
            }
            UiCommand::SetPluginEnabled {
                device_id,
                plugin,
                enabled,
                reply,
            } => {
                tracing::info!(
                    device = %device_id,
                    plugin = %plugin,
                    %enabled,
                    "set_plugin_enabled"
                );
                let result = app.set_plugin_enabled(&device_id, &plugin, enabled).await;
                if let Err(e) = &result {
                    tracing::warn!(device = %device_id, plugin = %plugin, error = %e, "cannot toggle plugin");
                }
                let _ = reply.send(result);
            }
            UiCommand::SetDeviceName { name, reply } => {
                tracing::info!(name = %name, "set_device_name");
                let result = app.set_local_name(&name).await;
                if let Err(e) = &result {
                    tracing::warn!(error = %e, "cannot change the device name");
                }
                let _ = reply.send(result.map(|_| ()));
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;

    use tokio::sync::{mpsc, oneshot};

    use super::*;
    use crate::trusted::TrustedStore;

    fn desktop() -> (Arc<App>, mpsc::UnboundedReceiver<Event>) {
        let local = LocalIdentity::new("a".repeat(32), "Desktop".to_owned(), 1716);
        let (events, events_rx) = mpsc::unbounded_channel();
        let app = App::new(local, events, TrustedStore::in_memory());
        (app, events_rx)
    }

    #[tokio::test]
    async fn sets_and_resets_the_local_name() {
        let (app, mut events) = desktop();
        assert_eq!(app.local_name().await, "Desktop");

        assert_eq!(
            app.set_local_name("  New Name  ")
                .await
                .expect("valid name"),
            "New Name"
        );
        assert_eq!(app.local_name().await, "New Name");
        match events.try_recv().expect("name event") {
            Event::LocalNameChanged { name } => assert_eq!(name, "New Name"),
            other => panic!("unexpected event: {other:?}"),
        }

        assert_eq!(
            app.local_packet().await.body["deviceName"],
            serde_json::json!("New Name")
        );

        // An empty name clears the override and falls back to the host name.
        assert_eq!(app.set_local_name("   ").await.expect("reset"), "Desktop");
        assert_eq!(app.local_name().await, "Desktop");
        match events.try_recv().expect("reset event") {
            Event::LocalNameChanged { name } => assert_eq!(name, "Desktop"),
            other => panic!("unexpected event: {other:?}"),
        }
    }

    #[tokio::test]
    async fn repeated_name_is_not_announced() {
        let (app, mut events) = desktop();

        app.set_local_name("Desktop").await.expect("same name");
        assert!(
            events.try_recv().is_err(),
            "unchanged names must not emit an event"
        );
    }

    #[tokio::test]
    async fn rejects_names_over_the_limit() {
        let (app, mut events) = desktop();
        let long = "x".repeat(MAX_DEVICE_NAME_CHARS + 1);

        assert!(matches!(
            app.set_local_name(&long).await,
            Err(DeviceNameError::TooLong { .. })
        ));
        assert_eq!(app.local_name().await, "Desktop");
        assert!(events.try_recv().is_err());
    }

    #[tokio::test]
    async fn name_change_is_sent_over_active_links() {
        let (app, _events) = desktop();
        let device_id = "b".repeat(32);
        let (tx, mut rx) = mpsc::unbounded_channel();
        app.register_link(&device_id, tx, None).await;

        app.set_local_name("Renamed Desktop")
            .await
            .expect("valid name");

        let packet = rx.recv().await.expect("identity update");
        assert_eq!(packet.packet_type, "kdeconnect.identity");
        let body = crate::identity::parse_identity(&packet).expect("identity packet");
        assert_eq!(body.device_name, "Renamed Desktop");
    }

    #[tokio::test]
    async fn set_device_name_command_replies_with_the_effective_name() {
        let (app, _events) = desktop();
        let (cmd_tx, cmd_rx) = mpsc::unbounded_channel();
        tokio::spawn(command_loop(app.clone(), cmd_rx));

        let (reply, response) = oneshot::channel();
        cmd_tx
            .send(UiCommand::SetDeviceName {
                name: "Commanded".to_owned(),
                reply,
            })
            .expect("command queued");

        assert_eq!(response.await.expect("reply"), Ok(()));
        assert_eq!(app.local_name().await, "Commanded");
    }

    #[tokio::test]
    async fn ping_command_reports_delivery_errors() {
        let local = LocalIdentity::new("a".repeat(32), "Desktop".to_owned(), 1716);
        let (events, _events_rx) = mpsc::unbounded_channel();
        let app = App::new(local, events, TrustedStore::in_memory());
        let (cmd_tx, cmd_rx) = mpsc::unbounded_channel();
        tokio::spawn(command_loop(app.clone(), cmd_rx));

        let (reply, response) = oneshot::channel();
        cmd_tx
            .send(UiCommand::Ping {
                device_id: "b".repeat(32),
                message: "hi".to_owned(),
                reply,
            })
            .expect("command queued");

        assert_eq!(
            response.await.expect("reply"),
            Err(PingError::NotPaired),
            "an unknown device must report the failure to the D-Bus caller"
        );
    }

    #[tokio::test]
    async fn clipboard_command_reports_delivery_errors() {
        let local = LocalIdentity::new("a".repeat(32), "Desktop".to_owned(), 1716);
        let (events, _events_rx) = mpsc::unbounded_channel();
        let app = App::new(local, events, TrustedStore::in_memory());
        let (cmd_tx, cmd_rx) = mpsc::unbounded_channel();
        tokio::spawn(command_loop(app.clone(), cmd_rx));

        let (reply, response) = oneshot::channel();
        cmd_tx
            .send(UiCommand::SendClipboard {
                device_id: "b".repeat(32),
                content: "hola".to_owned(),
                timestamp: None,
                reply,
            })
            .expect("command queued");

        assert_eq!(
            response.await.expect("reply"),
            Err(ClipboardError::NotPaired),
            "an unknown device must report the failure to the D-Bus caller"
        );
    }

    #[tokio::test]
    async fn plugin_toggle_rejects_unpaired_devices() {
        let local = LocalIdentity::new("a".repeat(32), "Desktop".to_owned(), 1716);
        let (events, _events_rx) = mpsc::unbounded_channel();
        let app = App::new(local, events, TrustedStore::in_memory());
        let device_id = "b".repeat(32);

        assert_eq!(
            app.set_plugin_enabled(&device_id, "kdeconnect.ping", false)
                .await,
            Err(PluginError::UnknownDevice)
        );
    }

    #[tokio::test]
    async fn plugin_toggle_rejects_unsupported_plugins() {
        let local = LocalIdentity::new("a".repeat(32), "Desktop".to_owned(), 1716);
        let (events, _events_rx) = mpsc::unbounded_channel();
        let app = App::new(local, events, TrustedStore::in_memory());
        let device_id = "b".repeat(32);
        app.trusted
            .insert(TrustedDevice {
                id: device_id.clone(),
                name: "Phone".to_owned(),
                cert_sha256: None,
                last_ip: None,
                download_dir: None,
                max_incoming_size: None,
                plugins: HashMap::new(),
            })
            .await;

        assert_eq!(
            app.set_plugin_enabled(&device_id, "kdeconnect.nope", false)
                .await,
            Err(PluginError::UnsupportedPlugin)
        );
    }

    #[tokio::test]
    async fn plugin_toggle_persists_and_announces_supported_plugins() {
        let local = LocalIdentity::new("a".repeat(32), "Desktop".to_owned(), 1716);
        let (events, mut events_rx) = mpsc::unbounded_channel();
        let app = App::new(local, events, TrustedStore::in_memory());
        let device_id = "b".repeat(32);
        app.trusted
            .insert(TrustedDevice {
                id: device_id.clone(),
                name: "Phone".to_owned(),
                cert_sha256: None,
                last_ip: None,
                download_dir: None,
                max_incoming_size: None,
                plugins: HashMap::new(),
            })
            .await;

        app.set_plugin_enabled(&device_id, "kdeconnect.ping", false)
            .await
            .expect("supported plugin");
        assert!(
            !app.trusted
                .plugin_enabled(&device_id, "kdeconnect.ping")
                .await
        );
        match events_rx.try_recv().expect("plugin event") {
            Event::PluginStateChanged {
                device_id: id,
                plugin,
                enabled,
            } => {
                assert_eq!(id, device_id);
                assert_eq!(plugin, "kdeconnect.ping");
                assert!(!enabled);
            }
            other => panic!("unexpected event: {other:?}"),
        }
    }

    #[tokio::test]
    async fn unpair_clears_battery_status() {
        let (app, device_id, _events) = app_with_phone().await;
        app.battery
            .upsert(
                &device_id,
                battery::BatteryStatus {
                    level: 80,
                    charging: false,
                },
            )
            .await;

        assert!(app.unpair(&device_id, false).await);
        assert_eq!(
            app.battery.get(&device_id).await,
            battery::BatteryStatus::UNKNOWN
        );
    }

    #[tokio::test]
    async fn unpair_clears_connectivity_report() {
        let (app, device_id, _events) = app_with_phone().await;
        app.connectivity
            .upsert(
                &device_id,
                BTreeMap::from([(
                    "6".to_owned(),
                    connectivity_report::SignalState {
                        network_type: "LTE".to_owned(),
                        signal_strength: 3,
                    },
                )]),
            )
            .await;

        assert!(app.unpair(&device_id, false).await);
        assert!(app.connectivity.get(&device_id).await.is_empty());
    }

    #[tokio::test]
    async fn disabling_connectivity_report_plugin_clears_report() {
        let (app, device_id, _events) = app_with_phone().await;
        app.connectivity
            .upsert(
                &device_id,
                BTreeMap::from([(
                    "6".to_owned(),
                    connectivity_report::SignalState {
                        network_type: "LTE".to_owned(),
                        signal_strength: 3,
                    },
                )]),
            )
            .await;

        app.set_plugin_enabled(
            &device_id,
            connectivity_report::CONNECTIVITY_REPORT_PACKET,
            false,
        )
        .await
        .expect("supported plugin");
        assert!(app.connectivity.get(&device_id).await.is_empty());
    }

    #[tokio::test]
    async fn disabling_battery_plugin_clears_status() {
        let (app, device_id, _events) = app_with_phone().await;
        app.battery
            .upsert(
                &device_id,
                battery::BatteryStatus {
                    level: 80,
                    charging: true,
                },
            )
            .await;

        app.set_plugin_enabled(&device_id, battery::BATTERY_PACKET, false)
            .await
            .expect("supported plugin");
        assert_eq!(
            app.battery.get(&device_id).await,
            battery::BatteryStatus::UNKNOWN
        );
    }

    #[tokio::test]
    async fn dismiss_notification_reports_missing_and_disabled() {
        let (app, device_id, _events) = app_with_phone().await;
        assert_eq!(
            app.dismiss_notification(&device_id, "ghost").await,
            Err(DismissError::UnknownNotification)
        );

        app.trusted
            .set_plugin_enabled(&device_id, notifications::NOTIFICATION_PACKET, false)
            .await;
        assert_eq!(
            app.dismiss_notification(&device_id, "ghost").await,
            Err(DismissError::PluginDisabled)
        );

        assert_eq!(
            app.dismiss_notification(&"c".repeat(32), "ghost").await,
            Err(DismissError::NotPaired)
        );
    }

    /// App with a paired phone and the battery plugin enabled.
    async fn app_with_phone() -> (Arc<App>, String, mpsc::UnboundedReceiver<Event>) {
        let local = LocalIdentity::new("a".repeat(32), "Desktop".to_owned(), 1716);
        let (events, events_rx) = mpsc::unbounded_channel();
        let app = App::new(local, events, TrustedStore::in_memory());
        let device_id = "b".repeat(32);
        app.trusted
            .insert(TrustedDevice {
                id: device_id.clone(),
                name: "Phone".to_owned(),
                cert_sha256: None,
                last_ip: None,
                download_dir: None,
                max_incoming_size: None,
                plugins: HashMap::new(),
            })
            .await;
        (app, device_id, events_rx)
    }

    #[tokio::test]
    async fn effective_download_dir_uses_the_device_override() {
        let (app, device_id, _events) = app_with_phone().await;
        let dir = tempfile::tempdir().expect("download dir");
        app.trusted
            .set_download_dir(&device_id, Some(dir.path().to_string_lossy().into_owned()))
            .await;

        let effective = app.effective_download_dir(&device_id).await.expect("dir");
        assert_eq!(effective, dir.path());
        assert!(effective.is_dir(), "the download dir is created on demand");
    }

    #[tokio::test]
    async fn effective_max_size_prefers_the_device_override() {
        let dir = tempfile::tempdir().expect("config dir");
        std::fs::write(
            dir.path().join("config.json"),
            r#"{"maxIncomingSize": 2048}"#,
        )
        .expect("config file");
        let config = ConfigStore::load(dir.path(), "Desktop".to_owned());
        let local = LocalIdentity::new("a".repeat(32), "Desktop".to_owned(), 1716);
        let (events, _events_rx) = mpsc::unbounded_channel();
        let app = App::with_config(local, events, TrustedStore::in_memory(), config);
        let device_id = "b".repeat(32);
        app.trusted
            .insert(TrustedDevice {
                id: device_id.clone(),
                name: "Phone".to_owned(),
                cert_sha256: None,
                last_ip: None,
                download_dir: None,
                max_incoming_size: Some(1024),
                plugins: HashMap::new(),
            })
            .await;

        assert_eq!(app.effective_max_size(&device_id).await, 1024);
    }

    #[tokio::test]
    async fn effective_max_size_defaults_to_500mib() {
        let (app, device_id, _events) = app_with_phone().await;
        assert_eq!(
            app.effective_max_size(&device_id).await,
            DEFAULT_MAX_INCOMING_SIZE
        );
    }
}
