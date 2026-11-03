//! D-Bus bridge: service object, per-device objects and signals.
//!
//! The service object only exposes service-wide data and the pairing prompt.
//! Everything tied to a device lives on its own object, served under
//! [`DEVICE_PATH_PREFIX`], with readable properties and `PropertiesChanged`
//! notifications. `org.freedesktop.DBus.ObjectManager` on the service path
//! lets clients enumerate them (`GetManagedObjects`).
//!
//! See `doc/dbus.md` for the full interface.

use std::collections::HashMap;
use std::sync::Arc;

use tokio::sync::mpsc;
use zbus::fdo::ObjectManager;
use zbus::object_server::SignalEmitter;
use zbus::zvariant::OwnedObjectPath;

use crate::app::{App, Device, Event, UiCommand};
use crate::plugins::battery;

pub const DBUS_NAME: &str = "dev.zotta.AriesConnect";
pub const DBUS_PATH: &str = "/dev/zotta/AriesConnect";
/// Interface served by every per-device object.
pub const DEVICE_INTERFACE: &str = "dev.zotta.AriesConnect.Device";
/// Path prefix of the per-device objects.
pub const DEVICE_PATH_PREFIX: &str = "/dev/zotta/AriesConnect/Device";

// ---------------------------------------------------------------------------
// Service object
// ---------------------------------------------------------------------------

/// Object served at [`DBUS_PATH`] for device listings and the pairing prompt.
struct AriesConnectIface {
    app: Arc<App>,
    cmd_tx: mpsc::UnboundedSender<UiCommand>,
}

#[zbus::interface(name = "dev.zotta.AriesConnect")]
impl AriesConnectIface {
    /// Accept a pending pairing request from `device_id`.
    async fn accept_pairing(&self, device_id: String) -> zbus::fdo::Result<()> {
        self.cmd_tx
            .send(UiCommand::AcceptPairing(device_id))
            .map_err(|e| zbus::fdo::Error::Failed(e.to_string()))
    }

    /// Reject a pending pairing request from `device_id`.
    async fn reject_pairing(&self, device_id: String) -> zbus::fdo::Result<()> {
        self.cmd_tx
            .send(UiCommand::RejectPairing(device_id))
            .map_err(|e| zbus::fdo::Error::Failed(e.to_string()))
    }

    /// List known devices as (id, name, paired, connected) tuples. Device
    /// objects expose the same data as properties; this is a convenience
    /// snapshot for clients that do not use `ObjectManager`.
    async fn devices(&self) -> zbus::fdo::Result<Vec<(String, String, bool, bool)>> {
        Ok(self
            .app
            .registry
            .list()
            .await
            .into_iter()
            .map(|d| (d.id, d.name, d.paired, d.connected))
            .collect())
    }

    /// List active notifications as
    /// (deviceId, notificationId, appName, title, text) tuples.
    async fn active_notifications(
        &self,
    ) -> zbus::fdo::Result<Vec<(String, String, String, String, String)>> {
        Ok(self
            .app
            .notifications
            .list()
            .await
            .into_iter()
            .map(|(device_id, n)| (device_id, n.id, n.app_name, n.title, n.text))
            .collect())
    }

    /// A device requested pairing; accept or reject it with the service
    /// methods within 30 seconds.
    #[zbus(signal)]
    async fn pairing_requested(
        emitter: &SignalEmitter<'_>,
        id: String,
        name: String,
    ) -> zbus::Result<()>;

    /// The pairing was rejected, cancelled or timed out.
    #[zbus(signal)]
    async fn pairing_rejected(
        emitter: &SignalEmitter<'_>,
        id: String,
        reason: String,
    ) -> zbus::Result<()>;
}

// ---------------------------------------------------------------------------
// Device object
// ---------------------------------------------------------------------------

/// Object served for every known device at
/// `<DEVICE_PATH_PREFIX>/<sanitized-id>`.
struct DeviceIface {
    app: Arc<App>,
    cmd_tx: mpsc::UnboundedSender<UiCommand>,
    device_id: String,
}

impl DeviceIface {
    /// Current registry entry; `None` once the device is forgotten.
    async fn device(&self) -> Option<Device> {
        self.app.registry.get(&self.device_id).await
    }
}

#[zbus::interface(name = "dev.zotta.AriesConnect.Device")]
impl DeviceIface {
    /// KDE Connect device id. This is the authoritative id: the object path
    /// only contains a sanitized version of it.
    #[zbus(property)]
    async fn device_id(&self) -> String {
        self.device_id.clone()
    }

    /// Human-readable device name, as announced in the identity.
    #[zbus(property)]
    async fn device_name(&self) -> String {
        self.device().await.map(|d| d.name).unwrap_or_default()
    }

    /// Device type announced by the peer (`phone`, `tablet`, `desktop`, ...).
    #[zbus(property)]
    async fn device_type(&self) -> String {
        self.device()
            .await
            .map(|d| d.device_type)
            .unwrap_or_default()
    }

    /// Whether the device is currently paired with this daemon.
    #[zbus(property)]
    async fn paired(&self) -> bool {
        self.device().await.is_some_and(|d| d.paired)
    }

    /// Whether the device has an active control channel right now.
    #[zbus(property)]
    async fn connected(&self) -> bool {
        self.device().await.is_some_and(|d| d.connected)
    }

    /// Every plugin implemented by the daemon and whether it is enabled for
    /// this device. Plugins absent from the persisted overrides count as
    /// enabled.
    #[zbus(property)]
    async fn plugins(&self) -> HashMap<String, bool> {
        let mut plugins = HashMap::new();
        for plugin in crate::plugins::supported_plugins() {
            plugins.insert(
                (*plugin).to_owned(),
                self.app
                    .trusted
                    .plugin_enabled(&self.device_id, plugin)
                    .await,
            );
        }
        plugins
    }

    /// Last reported battery percentage: `0..=100`, or `-1` when unknown, the
    /// device has no battery or the `kdeconnect.battery` plugin is disabled.
    #[zbus(property)]
    async fn battery_level(&self) -> i32 {
        if !self
            .app
            .trusted
            .plugin_enabled(&self.device_id, battery::BATTERY_PACKET)
            .await
        {
            return -1;
        }
        self.app.battery.get(&self.device_id).await.level
    }

    /// Whether the device reported that it is charging.
    #[zbus(property)]
    async fn is_charging(&self) -> bool {
        if !self
            .app
            .trusted
            .plugin_enabled(&self.device_id, battery::BATTERY_PACKET)
            .await
        {
            return false;
        }
        self.app.battery.get(&self.device_id).await.charging
    }

    /// Forget the pairing, notifying the peer when it is connected.
    async fn unpair(&self) -> zbus::fdo::Result<()> {
        self.cmd_tx
            .send(UiCommand::Unpair(self.device_id.clone()))
            .map_err(|e| zbus::fdo::Error::Failed(e.to_string()))
    }

    /// Show a ping notification on the device; `message` may be empty. Fails
    /// when the device is not paired, the plugin is disabled or it is not
    /// connected.
    async fn ping(&self, message: String) -> zbus::fdo::Result<()> {
        let (reply, response) = tokio::sync::oneshot::channel();
        self.cmd_tx
            .send(UiCommand::Ping {
                device_id: self.device_id.clone(),
                message,
                reply,
            })
            .map_err(|e| zbus::fdo::Error::Failed(e.to_string()))?;
        response
            .await
            .map_err(|_| zbus::fdo::Error::Failed("daemon stopped".to_owned()))?
            .map_err(|e| zbus::fdo::Error::Failed(e.to_string()))
    }

    /// Dismiss a notification locally and ask the phone to do the same.
    async fn dismiss_notification(&self, notification_id: String) -> zbus::fdo::Result<()> {
        self.cmd_tx
            .send(UiCommand::DismissNotification {
                device_id: self.device_id.clone(),
                notification_id,
            })
            .map_err(|e| zbus::fdo::Error::Failed(e.to_string()))
    }

    /// Enable or disable a plugin for this device. The change is persisted
    /// and announced through `PropertiesChanged` on the `Plugins` property.
    /// Fails for plugins the daemon does not implement.
    async fn set_plugin_enabled(&self, plugin: String, enabled: bool) -> zbus::fdo::Result<()> {
        let (reply, response) = tokio::sync::oneshot::channel();
        self.cmd_tx
            .send(UiCommand::SetPluginEnabled {
                device_id: self.device_id.clone(),
                plugin,
                enabled,
                reply,
            })
            .map_err(|e| zbus::fdo::Error::Failed(e.to_string()))?;
        response
            .await
            .map_err(|_| zbus::fdo::Error::Failed("daemon stopped".to_owned()))?
            .map_err(|e| zbus::fdo::Error::Failed(e.to_string()))
    }

    /// A paired device pinged us. The optional message may be empty.
    #[zbus(signal)]
    async fn ping_received(emitter: &SignalEmitter<'_>, message: String) -> zbus::Result<()>;

    /// The phone posted a notification.
    #[zbus(signal)]
    async fn notification_received(
        emitter: &SignalEmitter<'_>,
        id: String,
        app_name: String,
        title: String,
        text: String,
    ) -> zbus::Result<()>;

    /// A notification was dismissed, locally or on the phone.
    #[zbus(signal)]
    async fn notification_dismissed(emitter: &SignalEmitter<'_>, id: String) -> zbus::Result<()>;
}

// ---------------------------------------------------------------------------
// Object paths
// ---------------------------------------------------------------------------

/// Object path of a device.
///
/// KDE Connect ids may contain `-`, which is not allowed in a D-Bus object
/// path element, so every character outside `[A-Za-z0-9_]` is replaced by `_`.
/// The raw id is always available as the `DeviceId` property, so clients must
/// not parse the path. Returns `None` for an id that sanitizes to nothing.
pub fn device_path(device_id: &str) -> Option<OwnedObjectPath> {
    let mut safe = String::with_capacity(device_id.len());
    for c in device_id.chars() {
        if c.is_ascii_alphanumeric() || c == '_' {
            safe.push(c);
        } else {
            safe.push('_');
        }
    }
    if safe.is_empty() {
        return None;
    }
    format!("{DEVICE_PATH_PREFIX}/{safe}").try_into().ok()
}

// ---------------------------------------------------------------------------
// Registration and event loop
// ---------------------------------------------------------------------------

/// Registers the service and returns the command receiver that must be fed
/// into `app::command_loop`; starts the events -> signals loop.
pub async fn spawn(
    app: Arc<App>,
    events: mpsc::UnboundedReceiver<Event>,
) -> Result<mpsc::UnboundedReceiver<UiCommand>, zbus::Error> {
    let (cmd_tx, cmd_rx) = mpsc::unbounded_channel();

    let service = AriesConnectIface {
        app: app.clone(),
        cmd_tx: cmd_tx.clone(),
    };

    let conn = zbus::connection::Builder::session()?
        .name(DBUS_NAME)?
        .serve_at(DBUS_PATH, service)?
        .build()
        .await?;

    // One object per device already known (seeded from the trust store).
    for device in app.registry.list().await {
        ensure_device_object(&conn, &app, &cmd_tx, &device.id).await;
    }

    // Registered last so the subtree is already populated. It lets clients
    // enumerate device objects and watch them appear/disappear.
    if let Err(e) = conn.object_server().at(DBUS_PATH, ObjectManager).await {
        tracing::warn!(error = %e, "cannot register the ObjectManager");
    }

    tokio::spawn(event_loop(conn, app, cmd_tx, events));

    Ok(cmd_rx)
}

/// Registers the object of `device_id` if it is not there yet. The object
/// server keeps the existing interface when one is already registered.
async fn ensure_device_object(
    conn: &zbus::Connection,
    app: &Arc<App>,
    cmd_tx: &mpsc::UnboundedSender<UiCommand>,
    device_id: &str,
) {
    let Some(path) = device_path(device_id) else {
        tracing::warn!(device = %device_id, "cannot derive an object path");
        return;
    };
    let iface = DeviceIface {
        app: app.clone(),
        cmd_tx: cmd_tx.clone(),
        device_id: device_id.to_owned(),
    };
    if let Err(e) = conn.object_server().at(path.as_str(), iface).await {
        tracing::warn!(device = %device_id, error = %e, "cannot register the device object");
    }
}

/// Property of [`DeviceIface`] announced with `PropertiesChanged`.
#[derive(Debug, Clone, Copy)]
enum DeviceProperty {
    Name,
    Paired,
    Connected,
    Plugins,
    BatteryLevel,
    IsCharging,
}

/// Emits `PropertiesChanged` for one device property, reading the new value
/// from the interface getter.
async fn emit_property_changed(conn: &zbus::Connection, device_id: &str, property: DeviceProperty) {
    let Some(path) = device_path(device_id) else {
        return;
    };
    let iface_ref = match conn
        .object_server()
        .interface::<_, DeviceIface>(path.as_str())
        .await
    {
        Ok(iface) => iface,
        Err(e) => {
            tracing::debug!(
                device = %device_id,
                error = %e,
                "device object not available for PropertiesChanged"
            );
            return;
        }
    };
    let emitter = iface_ref.signal_emitter();
    let iface = iface_ref.get().await;
    let result = match property {
        DeviceProperty::Name => iface.device_name_changed(emitter).await,
        DeviceProperty::Paired => iface.paired_changed(emitter).await,
        DeviceProperty::Connected => iface.connected_changed(emitter).await,
        DeviceProperty::Plugins => iface.plugins_changed(emitter).await,
        DeviceProperty::BatteryLevel => iface.battery_level_changed(emitter).await,
        DeviceProperty::IsCharging => iface.is_charging_changed(emitter).await,
    };
    if let Err(e) = result {
        tracing::warn!(
            device = %device_id,
            property = ?property,
            error = %e,
            "failed to emit PropertiesChanged"
        );
    }
}

/// Signal emitter bound to the object of `device_id`.
fn device_emitter(conn: &zbus::Connection, device_id: &str) -> Option<SignalEmitter<'static>> {
    let path = device_path(device_id)?;
    Some(SignalEmitter::from_parts(conn.clone(), path.into()))
}

/// Forwards core events to D-Bus: keeps device objects in sync and emits the
/// remaining signals.
async fn event_loop(
    conn: zbus::Connection,
    app: Arc<App>,
    cmd_tx: mpsc::UnboundedSender<UiCommand>,
    mut events: mpsc::UnboundedReceiver<Event>,
) {
    let service = match SignalEmitter::new::<&str>(&conn, DBUS_PATH) {
        Ok(emitter) => emitter,
        Err(e) => {
            tracing::error!(error = %e, "cannot create signal emitter");
            return;
        }
    };

    // Last name announced per device, so repeated discovery packets do not
    // produce a `PropertiesChanged` every few seconds.
    let mut names: HashMap<String, String> = app
        .registry
        .list()
        .await
        .into_iter()
        .map(|d| (d.id, d.name))
        .collect();

    while let Some(ev) = events.recv().await {
        match ev {
            Event::DeviceSeen { id, name } => {
                ensure_device_object(&conn, &app, &cmd_tx, &id).await;
                let renamed = names.get(&id).is_some_and(|old| old != &name);
                names.insert(id.clone(), name);
                if renamed {
                    emit_property_changed(&conn, &id, DeviceProperty::Name).await;
                }
            }
            Event::Connected { id } => {
                ensure_device_object(&conn, &app, &cmd_tx, &id).await;
                emit_property_changed(&conn, &id, DeviceProperty::Connected).await;
            }
            Event::Disconnected { id } => {
                emit_property_changed(&conn, &id, DeviceProperty::Connected).await;
                emit_property_changed(&conn, &id, DeviceProperty::BatteryLevel).await;
                emit_property_changed(&conn, &id, DeviceProperty::IsCharging).await;
            }
            Event::PairingRequested { id, name } => {
                if let Err(e) = AriesConnectIface::pairing_requested(&service, id, name).await {
                    tracing::warn!(error = %e, "failed to emit PairingRequested");
                }
            }
            Event::PairingAccepted { id } => {
                emit_property_changed(&conn, &id, DeviceProperty::Paired).await;
            }
            Event::PairingRejected { id, reason } => {
                if let Err(e) = AriesConnectIface::pairing_rejected(&service, id, reason).await {
                    tracing::warn!(error = %e, "failed to emit PairingRejected");
                }
            }
            Event::PairingRemoved { id } => {
                emit_property_changed(&conn, &id, DeviceProperty::Paired).await;
                emit_property_changed(&conn, &id, DeviceProperty::BatteryLevel).await;
                emit_property_changed(&conn, &id, DeviceProperty::IsCharging).await;
            }
            Event::PluginStateChanged {
                device_id, plugin, ..
            } => {
                emit_property_changed(&conn, &device_id, DeviceProperty::Plugins).await;
                if plugin == battery::BATTERY_PACKET {
                    emit_property_changed(&conn, &device_id, DeviceProperty::BatteryLevel).await;
                    emit_property_changed(&conn, &device_id, DeviceProperty::IsCharging).await;
                }
            }
            Event::BatteryChanged { device_id, .. } => {
                emit_property_changed(&conn, &device_id, DeviceProperty::BatteryLevel).await;
                emit_property_changed(&conn, &device_id, DeviceProperty::IsCharging).await;
            }
            Event::NotificationReceived {
                device_id,
                id,
                app_name,
                title,
                text,
            } => {
                let Some(emitter) = device_emitter(&conn, &device_id) else {
                    continue;
                };
                if let Err(e) =
                    DeviceIface::notification_received(&emitter, id, app_name, title, text).await
                {
                    tracing::warn!(device = %device_id, error = %e, "failed to emit NotificationReceived");
                }
            }
            Event::NotificationDismissed { device_id, id } => {
                let Some(emitter) = device_emitter(&conn, &device_id) else {
                    continue;
                };
                if let Err(e) = DeviceIface::notification_dismissed(&emitter, id).await {
                    tracing::warn!(device = %device_id, error = %e, "failed to emit NotificationDismissed");
                }
            }
            Event::PingReceived { device_id, message } => {
                let Some(emitter) = device_emitter(&conn, &device_id) else {
                    continue;
                };
                if let Err(e) = DeviceIface::ping_received(&emitter, message).await {
                    tracing::warn!(device = %device_id, error = %e, "failed to emit PingReceived");
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn device_path_sanitizes_hyphens() {
        let id = "0123456789abcdef-0123456789abcdef";
        let path = device_path(id).expect("valid path");
        assert_eq!(
            path.as_str(),
            "/dev/zotta/AriesConnect/Device/0123456789abcdef_0123456789abcdef"
        );
    }

    #[test]
    fn device_path_keeps_alphanumeric_ids() {
        let id = "a".repeat(32);
        let path = device_path(&id).expect("valid path");
        assert_eq!(
            path.as_str(),
            format!("/dev/zotta/AriesConnect/Device/{id}")
        );
    }

    #[test]
    fn device_path_rejects_empty_ids() {
        assert!(device_path("").is_none());
    }

    #[test]
    fn every_supported_plugin_is_advertised() {
        let identity = crate::identity::LocalIdentity::new("a".repeat(32), "D".to_owned(), 1716);
        for plugin in crate::plugins::supported_plugins() {
            assert!(
                identity.incoming_capabilities.iter().any(|c| c == plugin),
                "incoming capabilities must advertise {plugin}"
            );
            assert!(
                identity.outgoing_capabilities.iter().any(|c| c == plugin),
                "outgoing capabilities must advertise {plugin}"
            );
        }
    }

    #[tokio::test]
    async fn plugins_property_lists_state_of_every_supported_plugin() {
        use tokio::sync::mpsc;

        use crate::app::App;
        use crate::identity::LocalIdentity;
        use crate::trusted::{TrustedDevice, TrustedStore};

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
                plugins: std::collections::HashMap::from([("kdeconnect.ping".to_owned(), false)]),
            })
            .await;

        let (cmd_tx, _cmd_rx) = mpsc::unbounded_channel();
        let iface = DeviceIface {
            app,
            cmd_tx,
            device_id,
        };
        let plugins = iface.plugins().await;

        assert_eq!(plugins.len(), crate::plugins::supported_plugins().len());
        assert_eq!(plugins.get("kdeconnect.ping"), Some(&false));
        assert_eq!(plugins.get("kdeconnect.notification"), Some(&true));
        assert_eq!(plugins.get("kdeconnect.battery"), Some(&true));
    }

    async fn device_iface(plugins: HashMap<String, bool>) -> DeviceIface {
        use tokio::sync::mpsc;

        use crate::app::App;
        use crate::identity::LocalIdentity;
        use crate::trusted::{TrustedDevice, TrustedStore};

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
                plugins,
            })
            .await;
        let (cmd_tx, _cmd_rx) = mpsc::unbounded_channel();
        DeviceIface {
            app,
            cmd_tx,
            device_id,
        }
    }

    #[tokio::test]
    async fn battery_properties_default_to_unknown() {
        let iface = device_iface(HashMap::new()).await;

        assert_eq!(iface.battery_level().await, -1);
        assert!(!iface.is_charging().await);
    }

    #[tokio::test]
    async fn battery_properties_report_the_last_status() {
        let iface = device_iface(HashMap::new()).await;
        iface
            .app
            .battery
            .upsert(
                &iface.device_id,
                battery::BatteryStatus {
                    level: 42,
                    charging: true,
                },
            )
            .await;

        assert_eq!(iface.battery_level().await, 42);
        assert!(iface.is_charging().await);
    }

    #[tokio::test]
    async fn battery_properties_hide_status_when_plugin_disabled() {
        let plugins = HashMap::from([(battery::BATTERY_PACKET.to_owned(), false)]);
        let iface = device_iface(plugins).await;
        iface
            .app
            .battery
            .upsert(
                &iface.device_id,
                battery::BatteryStatus {
                    level: 42,
                    charging: true,
                },
            )
            .await;

        assert_eq!(iface.battery_level().await, -1);
        assert!(!iface.is_charging().await);
    }
}
