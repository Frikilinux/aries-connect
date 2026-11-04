//! Clipboard sync (`kdeconnect.clipboard`).
//!
//! The clipboard is bidirectional: every change is sent as
//! `kdeconnect.clipboard`, and when the control channel is established both
//! sides may send `kdeconnect.clipboard.connect` with the clipboard timestamp
//! so the newest content wins.
//!
//! The daemon never reads or writes the local clipboard: it only relays
//! packets between the phone and a session client over D-Bus, which owns the
//! clipboard through the compositor. See `doc/dbus.md`.
//!
//! Only paired devices are accepted, only while the plugin is enabled and only
//! for non-empty text up to [`MAX_CLIPBOARD_BYTES`].

use std::sync::Arc;

use serde::Deserialize;

use crate::app::{App, Event};
use crate::identity::NetworkPacket;

/// Packet sent on every clipboard change.
pub const CLIPBOARD_PACKET: &str = "kdeconnect.clipboard";
/// Packet sent when a device connects, carrying the clipboard timestamp.
pub const CLIPBOARD_CONNECT_PACKET: &str = "kdeconnect.clipboard.connect";

/// Maximum clipboard text accepted or sent (1 MiB).
pub const MAX_CLIPBOARD_BYTES: usize = 1024 * 1024;

/// Why a clipboard send was rejected.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ClipboardError {
    /// The device is unknown or not paired.
    NotPaired,
    /// The `kdeconnect.clipboard` plugin is disabled for the device.
    PluginDisabled,
    /// The device has no active control channel.
    NotConnected,
    /// The text exceeds [`MAX_CLIPBOARD_BYTES`].
    TooLarge,
}

impl std::fmt::Display for ClipboardError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let message = match self {
            Self::NotPaired => "device is not paired",
            Self::PluginDisabled => "kdeconnect.clipboard is disabled for this device",
            Self::NotConnected => "device has no active control channel",
            Self::TooLarge => "clipboard text is too large",
        };
        f.write_str(message)
    }
}

impl std::error::Error for ClipboardError {}

/// Wire format of both clipboard packets. `content` is optional in
/// `clipboard.connect`, and `timestamp` may be a number or a string depending
/// on the client.
#[derive(Debug, Default, Deserialize)]
#[serde(default)]
struct IncomingClipboard {
    content: Option<String>,
    timestamp: serde_json::Value,
}

/// Normalizes a millisecond timestamp; missing or invalid values count as
/// unknown (`0`).
fn timestamp_to_ms(value: &serde_json::Value) -> i64 {
    match value {
        serde_json::Value::Number(number) => number
            .as_i64()
            .or_else(|| number.as_f64().map(|timestamp| timestamp as i64))
            .unwrap_or(0),
        serde_json::Value::String(text) => text.parse().unwrap_or(0),
        _ => 0,
    }
}

/// Builds a regular clipboard packet.
pub fn clipboard_packet(content: &str) -> NetworkPacket {
    NetworkPacket::new(CLIPBOARD_PACKET, serde_json::json!({ "content": content }))
}

/// Builds the clipboard packet sent when a device connects.
pub fn clipboard_connect_packet(content: &str, timestamp: i64) -> NetworkPacket {
    NetworkPacket::new(
        CLIPBOARD_CONNECT_PACKET,
        serde_json::json!({ "content": content, "timestamp": timestamp }),
    )
}

/// Handles incoming clipboard packets. Returns `true` when the packet was
/// consumed.
pub async fn handle_packet(
    app: &Arc<App>,
    device_id: Option<&str>,
    packet: &NetworkPacket,
) -> bool {
    let is_connect = packet.packet_type == CLIPBOARD_CONNECT_PACKET;
    if packet.packet_type != CLIPBOARD_PACKET && !is_connect {
        return false;
    }
    let Some(device_id) = device_id else {
        return true;
    };
    if app.trusted.get(device_id).await.is_none() {
        tracing::debug!(device = %device_id, "ignoring clipboard from an unpaired device");
        return true;
    }
    if !app
        .trusted
        .plugin_enabled(device_id, CLIPBOARD_PACKET)
        .await
    {
        tracing::debug!(device = %device_id, "clipboard plugin is disabled for this device");
        return true;
    }
    let body: IncomingClipboard = match serde_json::from_value(packet.body.clone()) {
        Ok(body) => body,
        Err(e) => {
            tracing::debug!(device = %device_id, error = %e, "malformed clipboard packet");
            return true;
        }
    };
    let Some(content) = body.content.filter(|content| !content.is_empty()) else {
        tracing::debug!(device = %device_id, "ignoring empty clipboard");
        return true;
    };
    if content.len() > MAX_CLIPBOARD_BYTES {
        tracing::warn!(
            device = %device_id,
            bytes = content.len(),
            "ignoring oversized clipboard"
        );
        return true;
    }
    let timestamp = if is_connect {
        let timestamp = timestamp_to_ms(&body.timestamp);
        if timestamp <= 0 {
            tracing::debug!(
                device = %device_id,
                "ignoring clipboard.connect without a timestamp"
            );
            return true;
        }
        timestamp
    } else {
        0
    };
    tracing::debug!(
        device = %device_id,
        bytes = content.len(),
        connect = is_connect,
        "clipboard received"
    );
    let _ = app.events.send(Event::ClipboardReceived {
        device_id: device_id.to_owned(),
        content,
        timestamp,
    });
    true
}

/// Sends the local clipboard as a regular update.
pub async fn send(app: &Arc<App>, device_id: &str, content: &str) -> Result<(), ClipboardError> {
    send_inner(app, device_id, content, None).await
}

/// Sends the local clipboard as the connection-time packet.
pub async fn send_connect(
    app: &Arc<App>,
    device_id: &str,
    content: &str,
    timestamp: i64,
) -> Result<(), ClipboardError> {
    if timestamp <= 0 {
        tracing::debug!(device = %device_id, "ignoring clipboard.connect without a timestamp");
        return Ok(());
    }
    send_inner(app, device_id, content, Some(timestamp)).await
}

async fn send_inner(
    app: &Arc<App>,
    device_id: &str,
    content: &str,
    timestamp: Option<i64>,
) -> Result<(), ClipboardError> {
    if content.is_empty() {
        tracing::debug!(device = %device_id, "ignoring empty clipboard");
        return Ok(());
    }
    if app.trusted.get(device_id).await.is_none() {
        tracing::debug!(device = %device_id, "cannot send clipboard to an unpaired device");
        return Err(ClipboardError::NotPaired);
    }
    if !app
        .trusted
        .plugin_enabled(device_id, CLIPBOARD_PACKET)
        .await
    {
        tracing::debug!(device = %device_id, "clipboard plugin is disabled for this device");
        return Err(ClipboardError::PluginDisabled);
    }
    if content.len() > MAX_CLIPBOARD_BYTES {
        tracing::debug!(
            device = %device_id,
            bytes = content.len(),
            "refusing to send an oversized clipboard"
        );
        return Err(ClipboardError::TooLarge);
    }
    let packet = match timestamp {
        Some(timestamp) => clipboard_connect_packet(content, timestamp),
        None => clipboard_packet(content),
    };
    if !app.send_packet(device_id, &packet).await {
        tracing::debug!(
            device = %device_id,
            "no active control channel to send the clipboard"
        );
        return Err(ClipboardError::NotConnected);
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    use std::collections::HashMap;

    use tokio::sync::mpsc;

    use crate::identity::LocalIdentity;
    use crate::trusted::{TrustedDevice, TrustedStore};

    const DEVICE_ID: &str = "bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb";

    /// Paired device with `plugins` as per-device overrides.
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

    async fn app_with_link(
        plugins: HashMap<String, bool>,
    ) -> (Arc<App>, mpsc::UnboundedReceiver<NetworkPacket>) {
        let (app, _events) = paired_app(plugins).await;
        let (tx, rx) = mpsc::unbounded_channel();
        app.register_link(DEVICE_ID, tx, None).await;
        (app, rx)
    }

    #[tokio::test]
    async fn emits_event_for_incoming_clipboard() {
        let (app, mut events) = paired_app(HashMap::new()).await;
        let packet = clipboard_packet("hola");

        assert!(handle_packet(&app, Some(DEVICE_ID), &packet).await);
        match events.try_recv().expect("clipboard event") {
            Event::ClipboardReceived {
                device_id,
                content,
                timestamp,
            } => {
                assert_eq!(device_id, DEVICE_ID);
                assert_eq!(content, "hola");
                assert_eq!(timestamp, 0);
            }
            other => panic!("unexpected event: {other:?}"),
        }
    }

    #[tokio::test]
    async fn connect_packet_carries_the_timestamp() {
        let (app, mut events) = paired_app(HashMap::new()).await;
        let packet = clipboard_connect_packet("hola", 1_700_000_000_000);

        assert!(handle_packet(&app, Some(DEVICE_ID), &packet).await);
        match events.try_recv().expect("clipboard event") {
            Event::ClipboardReceived { timestamp, .. } => {
                assert_eq!(timestamp, 1_700_000_000_000);
            }
            other => panic!("unexpected event: {other:?}"),
        }
    }

    #[tokio::test]
    async fn connect_packet_accepts_timestamp_as_string() {
        let (app, mut events) = paired_app(HashMap::new()).await;
        let packet = NetworkPacket::new(
            CLIPBOARD_CONNECT_PACKET,
            serde_json::json!({ "content": "hola", "timestamp": "1234" }),
        );

        assert!(handle_packet(&app, Some(DEVICE_ID), &packet).await);
        match events.try_recv().expect("clipboard event") {
            Event::ClipboardReceived { timestamp, .. } => assert_eq!(timestamp, 1234),
            other => panic!("unexpected event: {other:?}"),
        }
    }

    #[tokio::test]
    async fn connect_packet_without_timestamp_is_ignored() {
        let (app, mut events) = paired_app(HashMap::new()).await;
        let packet = NetworkPacket::new(
            CLIPBOARD_CONNECT_PACKET,
            serde_json::json!({ "content": "hola" }),
        );

        assert!(handle_packet(&app, Some(DEVICE_ID), &packet).await);
        assert!(
            events.try_recv().is_err(),
            "clipboard.connect without a timestamp must be ignored"
        );
    }

    #[tokio::test]
    async fn empty_clipboard_is_ignored() {
        let (app, mut events) = paired_app(HashMap::new()).await;
        let packet = clipboard_packet("");

        assert!(handle_packet(&app, Some(DEVICE_ID), &packet).await);
        assert!(events.try_recv().is_err(), "empty clipboard is ignored");
    }

    #[tokio::test]
    async fn oversized_clipboard_is_ignored() {
        let (app, mut events) = paired_app(HashMap::new()).await;
        let packet = clipboard_packet(&"a".repeat(MAX_CLIPBOARD_BYTES + 1));

        assert!(handle_packet(&app, Some(DEVICE_ID), &packet).await);
        assert!(events.try_recv().is_err(), "oversized clipboard is ignored");
    }

    #[tokio::test]
    async fn ignores_clipboard_when_plugin_disabled() {
        let plugins = HashMap::from([(CLIPBOARD_PACKET.to_owned(), false)]);
        let (app, mut events) = paired_app(plugins).await;

        assert!(handle_packet(&app, Some(DEVICE_ID), &clipboard_packet("hola")).await);
        assert!(
            events.try_recv().is_err(),
            "disabled devices must not report clipboard changes"
        );
    }

    #[tokio::test]
    async fn ignores_clipboard_from_unpaired_device() {
        let local = LocalIdentity::new("a".repeat(32), "Desktop".to_owned(), 1716);
        let (events, mut events_rx) = mpsc::unbounded_channel();
        let app = App::new(local, events, TrustedStore::in_memory());

        assert!(handle_packet(&app, Some(DEVICE_ID), &clipboard_packet("hola")).await);
        assert!(events_rx.try_recv().is_err(), "unpaired peers are ignored");
    }

    #[tokio::test]
    async fn send_reports_delivery_errors() {
        let (app, _events) = paired_app(HashMap::new()).await;

        assert_eq!(
            send(&app, DEVICE_ID, "hola").await,
            Err(ClipboardError::NotConnected)
        );

        let plugins = HashMap::from([(CLIPBOARD_PACKET.to_owned(), false)]);
        let (app, _events) = paired_app(plugins).await;
        assert_eq!(
            send(&app, DEVICE_ID, "hola").await,
            Err(ClipboardError::PluginDisabled)
        );

        let local = LocalIdentity::new("a".repeat(32), "Desktop".to_owned(), 1716);
        let (events, _events_rx) = mpsc::unbounded_channel();
        let app = App::new(local, events, TrustedStore::in_memory());
        assert_eq!(
            send(&app, DEVICE_ID, "hola").await,
            Err(ClipboardError::NotPaired)
        );
    }

    #[tokio::test]
    async fn empty_and_oversized_sends_are_rejected() {
        let (app, mut outgoing) = app_with_link(HashMap::new()).await;

        assert_eq!(send(&app, DEVICE_ID, "").await, Ok(()));
        assert!(outgoing.try_recv().is_err(), "empty clipboard is not sent");

        assert_eq!(
            send(&app, DEVICE_ID, &"a".repeat(MAX_CLIPBOARD_BYTES + 1)).await,
            Err(ClipboardError::TooLarge)
        );
        assert!(
            outgoing.try_recv().is_err(),
            "oversized clipboard is not sent"
        );
    }

    #[tokio::test]
    async fn sends_regular_packet() {
        let (app, mut outgoing) = app_with_link(HashMap::new()).await;

        assert_eq!(send(&app, DEVICE_ID, "hola").await, Ok(()));
        let packet = outgoing.try_recv().expect("clipboard packet");
        assert_eq!(packet.packet_type, CLIPBOARD_PACKET);
        assert_eq!(
            packet.body.get("content").and_then(|v| v.as_str()),
            Some("hola")
        );
    }

    #[tokio::test]
    async fn sends_connect_packet() {
        let (app, mut outgoing) = app_with_link(HashMap::new()).await;

        assert_eq!(send_connect(&app, DEVICE_ID, "hola", 1234).await, Ok(()));
        let packet = outgoing.try_recv().expect("clipboard packet");
        assert_eq!(packet.packet_type, CLIPBOARD_CONNECT_PACKET);
        assert_eq!(
            packet.body.get("timestamp").and_then(|v| v.as_i64()),
            Some(1234)
        );
    }

    #[tokio::test]
    async fn connect_without_timestamp_sends_nothing() {
        let (app, mut outgoing) = app_with_link(HashMap::new()).await;

        assert_eq!(send_connect(&app, DEVICE_ID, "hola", 0).await, Ok(()));
        assert!(outgoing.try_recv().is_err());
    }

    #[test]
    fn timestamp_accepts_numbers_and_strings() {
        assert_eq!(timestamp_to_ms(&serde_json::json!(1234)), 1234);
        assert_eq!(timestamp_to_ms(&serde_json::json!(1234.0)), 1234);
        assert_eq!(timestamp_to_ms(&serde_json::json!("1234")), 1234);
        assert_eq!(timestamp_to_ms(&serde_json::json!(null)), 0);
        assert_eq!(timestamp_to_ms(&serde_json::json!("nope")), 0);
    }
}
