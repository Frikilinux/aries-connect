//! Ping (`kdeconnect.ping`).
//!
//! Both directions use the same packet with an optional `message`: the phone
//! shows a notification when it receives one and the daemon surfaces incoming
//! pings over D-Bus. The protocol has no acknowledgement, so a received ping is
//! never answered.
//!
//! Only paired devices are accepted, and only while the `kdeconnect.ping`
//! plugin is enabled for that device.

use std::sync::Arc;

use serde::Deserialize;

use crate::app::{App, Event};
use crate::identity::NetworkPacket;

/// Packet type used in both directions.
pub const PING_PACKET: &str = "kdeconnect.ping";

/// Wire format of a `kdeconnect.ping` body. The message is optional: a client
/// may send just `{}` for a plain ping.
#[derive(Debug, Default, Deserialize)]
#[serde(default)]
struct IncomingPing {
    message: String,
}

/// Handles ping packets. Returns `true` when the packet was consumed.
pub async fn handle_packet(
    app: &Arc<App>,
    device_id: Option<&str>,
    packet: &NetworkPacket,
) -> bool {
    if packet.packet_type != PING_PACKET {
        return false;
    }
    let Some(device_id) = device_id else {
        return true;
    };
    if app.trusted.get(device_id).await.is_none() {
        tracing::debug!(device = %device_id, "ignoring ping from an unpaired device");
        return true;
    }
    if !app.trusted.plugin_enabled(device_id, PING_PACKET).await {
        tracing::debug!(device = %device_id, "ping plugin is disabled for this device");
        return true;
    }
    let body: IncomingPing = match serde_json::from_value(packet.body.clone()) {
        Ok(body) => body,
        Err(e) => {
            // A ping with an unexpected body is still a ping; surface it with
            // an empty message instead of dropping it.
            tracing::debug!(device = %device_id, error = %e, "malformed ping packet");
            IncomingPing::default()
        }
    };
    tracing::debug!(device = %device_id, "ping received");
    let _ = app.events.send(Event::PingReceived {
        device_id: device_id.to_owned(),
        message: body.message,
    });
    true
}

/// Builds the packet that pings a device.
pub fn ping_packet(message: &str) -> NetworkPacket {
    NetworkPacket::new(PING_PACKET, serde_json::json!({ "message": message }))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;
    use std::sync::Arc;

    use tokio::sync::mpsc;

    use crate::app::App;
    use crate::identity::LocalIdentity;
    use crate::trusted::{TrustedDevice, TrustedStore};

    const DEVICE_ID: &str = "bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb";

    /// Paired device with `plugins` as per-device overrides. Returns the app and
    /// the event receiver so tests can assert what reached D-Bus.
    async fn paired_app(
        plugins: HashMap<String, bool>,
    ) -> (Arc<App>, mpsc::UnboundedReceiver<Event>) {
        let local = LocalIdentity::new("a".repeat(32), "Desktop".to_owned(), 1716);
        let (events, events_rx) = mpsc::unbounded_channel();
        let app = App::new(local, events, TrustedStore::in_memory());
        app.trusted
            .insert(TrustedDevice {
                id: DEVICE_ID.to_owned(),
                name: "Phone".to_owned(),
                cert_sha256: None,
                last_ip: None,
                plugins,
            })
            .await;
        (app, events_rx)
    }

    #[tokio::test]
    async fn emits_event_for_incoming_ping() {
        let (app, mut events) = paired_app(HashMap::new()).await;
        let packet = ping_packet("Hello from the phone");

        assert!(handle_packet(&app, Some(DEVICE_ID), &packet).await);
        match events.try_recv().expect("ping event") {
            Event::PingReceived { device_id, message } => {
                assert_eq!(device_id, DEVICE_ID);
                assert_eq!(message, "Hello from the phone");
            }
            other => panic!("unexpected event: {other:?}"),
        }
    }

    #[tokio::test]
    async fn accepts_ping_without_message() {
        let (app, mut events) = paired_app(HashMap::new()).await;
        let packet = NetworkPacket::new(PING_PACKET, serde_json::json!({}));

        assert!(handle_packet(&app, Some(DEVICE_ID), &packet).await);
        match events.try_recv().expect("ping event") {
            Event::PingReceived { message, .. } => assert!(message.is_empty()),
            other => panic!("unexpected event: {other:?}"),
        }
    }

    #[tokio::test]
    async fn ignores_ping_when_plugin_disabled() {
        let plugins = HashMap::from([(PING_PACKET.to_owned(), false)]);
        let (app, mut events) = paired_app(plugins).await;

        assert!(handle_packet(&app, Some(DEVICE_ID), &ping_packet("hi")).await);
        assert!(
            events.try_recv().is_err(),
            "disabled devices must not surface pings"
        );
    }

    #[tokio::test]
    async fn ignores_ping_from_unpaired_device() {
        let local = LocalIdentity::new("a".repeat(32), "Desktop".to_owned(), 1716);
        let (events, mut events_rx) = mpsc::unbounded_channel();
        let app = App::new(local, events, TrustedStore::in_memory());

        assert!(handle_packet(&app, Some(DEVICE_ID), &ping_packet("hi")).await);
        assert!(events_rx.try_recv().is_err(), "unpaired peers are ignored");
    }

    #[test]
    fn ping_packet_carries_the_message() {
        let packet = ping_packet("beep");
        assert_eq!(packet.packet_type, PING_PACKET);
        assert_eq!(
            packet.body.get("message").and_then(|v| v.as_str()),
            Some("beep")
        );
    }
}
