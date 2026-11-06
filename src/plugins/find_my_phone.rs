//! Find My Phone (`kdeconnect.findmyphone.request`).
//!
//! A single packet type used in both directions: the receiver is asked to
//! announce its location, usually by playing a sound. By convention, sending a
//! second request cancels the previous one.
//!
//! This daemon only implements the outgoing direction (PC -> phone): it asks a
//! paired phone to ring. Incoming requests from a phone are consumed (and
//! logged at debug level) but the daemon has no audio output, so it does not
//! ring itself; a frontend can implement that on top of the protocol if needed.
//!
//! Only paired devices are accepted, and only while the
//! `kdeconnect.findmyphone.request` plugin is enabled for that device.

use std::sync::Arc;

use crate::app::App;
use crate::identity::NetworkPacket;

/// Packet type used in both directions.
pub const FIND_MY_PHONE_PACKET: &str = "kdeconnect.findmyphone.request";

/// Builds the packet that asks a device to announce itself.
pub fn request_packet() -> NetworkPacket {
    NetworkPacket::new(FIND_MY_PHONE_PACKET, serde_json::json!({}))
}

/// Handles incoming find-my-phone requests. Returns `true` when the packet was
/// consumed. The daemon has no audio output, so it only logs the request.
pub async fn handle_packet(
    app: &Arc<App>,
    device_id: Option<&str>,
    packet: &NetworkPacket,
) -> bool {
    if packet.packet_type != FIND_MY_PHONE_PACKET {
        return false;
    }
    let Some(device_id) = device_id else {
        return true;
    };
    if app.trusted.get(device_id).await.is_none() {
        tracing::debug!(device = %device_id, "ignoring find-my-phone request from an unpaired device");
        return true;
    }
    if !app
        .trusted
        .plugin_enabled(device_id, FIND_MY_PHONE_PACKET)
        .await
    {
        tracing::debug!(device = %device_id, "find-my-phone plugin is disabled for this device");
        return true;
    }
    tracing::debug!(device = %device_id, "find-my-phone request received");
    true
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

    async fn paired_app(plugins: HashMap<String, bool>) -> Arc<App> {
        let local = LocalIdentity::new("a".repeat(32), "Desktop".to_owned(), 1716);
        let (events, _events_rx) = mpsc::unbounded_channel();
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
        app
    }

    #[tokio::test]
    async fn consumes_request_from_paired_device() {
        let app = paired_app(HashMap::new()).await;
        let packet = request_packet();

        assert!(handle_packet(&app, Some(DEVICE_ID), &packet).await);
    }

    #[tokio::test]
    async fn ignores_request_when_plugin_disabled() {
        let plugins = HashMap::from([(FIND_MY_PHONE_PACKET.to_owned(), false)]);
        let app = paired_app(plugins).await;
        let packet = request_packet();

        // Still consumed (not forwarded), but does not error.
        assert!(handle_packet(&app, Some(DEVICE_ID), &packet).await);
    }

    #[tokio::test]
    async fn ignores_request_from_unpaired_device() {
        let local = LocalIdentity::new("a".repeat(32), "Desktop".to_owned(), 1716);
        let (events, _events_rx) = mpsc::unbounded_channel();
        let app = App::new(local, events, TrustedStore::in_memory());

        assert!(handle_packet(&app, Some(DEVICE_ID), &request_packet()).await);
    }

    #[tokio::test]
    async fn ignores_packets_of_other_types() {
        let app = paired_app(HashMap::new()).await;
        let packet = NetworkPacket::new("kdeconnect.ping", serde_json::json!({}));

        assert!(!handle_packet(&app, Some(DEVICE_ID), &packet).await);
    }

    #[test]
    fn request_packet_has_empty_body() {
        let packet = request_packet();
        assert_eq!(packet.packet_type, FIND_MY_PHONE_PACKET);
        assert_eq!(packet.body, serde_json::json!({}));
    }
}
