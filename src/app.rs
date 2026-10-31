//! Shared daemon state: device registry, events pushed to D-Bus and pending
//! pairing requests.

use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;

use tokio::sync::{mpsc, oneshot, Mutex, RwLock};

use crate::identity::{IdentityBody, LocalIdentity, NetworkPacket};
use crate::trusted::{TrustedDevice, TrustedStore};

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
}

/// Commands arriving from the D-Bus interface toward the core.
#[derive(Debug)]
pub enum UiCommand {
    AcceptPairing(String),
    RejectPairing(String),
}

/// Decision on a pairing request.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PairDecision {
    Accept,
    Reject,
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
    pub local: LocalIdentity,
    pub registry: DeviceRegistry,
    pub trusted: TrustedStore,
    /// Channel of events toward D-Bus.
    pub events: mpsc::UnboundedSender<Event>,
    /// Pending pairing requests: deviceId -> decision sender.
    pending_pairs: Mutex<HashMap<String, oneshot::Sender<PairDecision>>>,
    /// Latest control channel per device.
    links: Mutex<HashMap<String, LinkHandle>>,
    link_generation: AtomicU64,
}

impl App {
    pub fn new(
        local: LocalIdentity,
        events: mpsc::UnboundedSender<Event>,
        trusted: TrustedStore,
    ) -> Arc<Self> {
        Arc::new(Self {
            local,
            registry: DeviceRegistry::new(),
            trusted,
            events,
            pending_pairs: Mutex::new(HashMap::new()),
            links: Mutex::new(HashMap::new()),
            link_generation: AtomicU64::new(0),
        })
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
            })
            .await;
    }

    /// Forgets a pairing (peer unpair or explicit removal).
    pub async fn unpair(&self, id: &str) {
        self.registry.set_paired(id, false).await;
        self.trusted.remove(id).await;
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
    pub async fn unregister_link(&self, id: &str, generation: u64) {
        let mut links = self.links.lock().await;
        if links
            .get(id)
            .is_some_and(|link| link.generation == generation)
        {
            links.remove(id);
        }
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
        }
    }
}
