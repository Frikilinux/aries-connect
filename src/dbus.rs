//! D-Bus bridge: exposes daemon methods and forwards core events as signals.
//!
//! See `doc/dbus.md` for the full interface.

use std::sync::Arc;

use tokio::sync::mpsc;
use zbus::object_server::SignalEmitter;

use crate::app::{App, Event, UiCommand};

pub const DBUS_NAME: &str = "dev.zotta.AriesConnect";
pub const DBUS_PATH: &str = "/dev/zotta/AriesConnect";

/// Object served at `DBUS_PATH` for pairing control and device listings.
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

    /// List known devices as (id, name, paired, connected) tuples.
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

    /// Dismiss a notification locally and on the phone.
    async fn dismiss_notification(
        &self,
        device_id: String,
        notification_id: String,
    ) -> zbus::fdo::Result<()> {
        self.cmd_tx
            .send(UiCommand::DismissNotification {
                device_id,
                notification_id,
            })
            .map_err(|e| zbus::fdo::Error::Failed(e.to_string()))
    }

    /// Whether `plugin` (e.g. `kdeconnect.notification`) is enabled for the
    /// device. Unknown devices report `false`.
    async fn plugin_enabled(&self, device_id: String, plugin: String) -> zbus::fdo::Result<bool> {
        Ok(self.app.trusted.plugin_enabled(&device_id, &plugin).await)
    }

    /// Enable or disable `plugin` for the device; the change is persisted and
    /// announced with the `PluginStateChanged` signal.
    async fn set_plugin_enabled(
        &self,
        device_id: String,
        plugin: String,
        enabled: bool,
    ) -> zbus::fdo::Result<()> {
        self.cmd_tx
            .send(UiCommand::SetPluginEnabled {
                device_id,
                plugin,
                enabled,
            })
            .map_err(|e| zbus::fdo::Error::Failed(e.to_string()))
    }
}

/// Registers the service and returns the command receiver that must be fed
/// into `app::command_loop`; starts the events -> signals loop.
pub async fn spawn(
    app: Arc<App>,
    events: mpsc::UnboundedReceiver<Event>,
) -> Result<mpsc::UnboundedReceiver<UiCommand>, zbus::Error> {
    let (cmd_tx, cmd_rx) = mpsc::unbounded_channel();

    let iface = AriesConnectIface {
        app: app.clone(),
        cmd_tx,
    };

    let conn = zbus::connection::Builder::session()?
        .name(DBUS_NAME)?
        .serve_at(DBUS_PATH, iface)?
        .build()
        .await?;

    // The task keeps the connection alive for the lifetime of the daemon.
    tokio::spawn(signal_loop(conn, events));

    Ok(cmd_rx)
}

/// Forwards core events to D-Bus signals.
async fn signal_loop(conn: zbus::Connection, mut events: mpsc::UnboundedReceiver<Event>) {
    let emitter = match SignalEmitter::new::<&str>(&conn, DBUS_PATH) {
        Ok(e) => e,
        Err(e) => {
            tracing::error!(error = %e, "cannot create signal emitter");
            return;
        }
    };

    while let Some(ev) = events.recv().await {
        let (name, result) = match ev {
            Event::DeviceSeen { id, name } => (
                "DeviceSeen",
                emitter.emit(DBUS_NAME, "DeviceSeen", &(id, name)).await,
            ),
            Event::Connected { id } => (
                "Connected",
                emitter
                    .emit(DBUS_NAME, "Connected", &(id, String::new()))
                    .await,
            ),
            Event::Disconnected { id } => (
                "Disconnected",
                emitter
                    .emit(DBUS_NAME, "Disconnected", &(id, String::new()))
                    .await,
            ),
            Event::PairingRequested { id, name } => (
                "PairingRequested",
                emitter
                    .emit(DBUS_NAME, "PairingRequested", &(id, name))
                    .await,
            ),
            Event::PairingAccepted { id } => (
                "PairingAccepted",
                emitter
                    .emit(DBUS_NAME, "PairingAccepted", &(id, String::new()))
                    .await,
            ),
            Event::PairingRejected { id, reason } => (
                "PairingRejected",
                emitter
                    .emit(DBUS_NAME, "PairingRejected", &(id, reason))
                    .await,
            ),
            Event::NotificationReceived {
                device_id,
                id,
                app_name,
                title,
                text,
            } => (
                "NotificationReceived",
                emitter
                    .emit(
                        DBUS_NAME,
                        "NotificationReceived",
                        &(device_id, id, app_name, title, text),
                    )
                    .await,
            ),
            Event::NotificationDismissed { device_id, id } => (
                "NotificationDismissed",
                emitter
                    .emit(DBUS_NAME, "NotificationDismissed", &(device_id, id))
                    .await,
            ),
            Event::PluginStateChanged {
                device_id,
                plugin,
                enabled,
            } => (
                "PluginStateChanged",
                emitter
                    .emit(
                        DBUS_NAME,
                        "PluginStateChanged",
                        &(device_id, plugin, enabled),
                    )
                    .await,
            ),
        };

        if let Err(e) = result {
            tracing::warn!(signal = name, error = %e, "failed to emit D-Bus signal");
        }
    }
}
