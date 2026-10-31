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
        let (name, first, second): (&str, String, String) = match ev {
            Event::DeviceSeen { id, name } => ("DeviceSeen", id, name),
            Event::Connected { id } => ("Connected", id, String::new()),
            Event::Disconnected { id } => ("Disconnected", id, String::new()),
            Event::PairingRequested { id, name } => ("PairingRequested", id, name),
            Event::PairingAccepted { id } => ("PairingAccepted", id, String::new()),
            Event::PairingRejected { id, reason } => ("PairingRejected", id, reason),
        };

        let body = (first, second);
        if let Err(e) = emitter.emit(DBUS_NAME, name, &body).await {
            tracing::warn!(signal = name, error = %e, "failed to emit D-Bus signal");
        }
    }
}
