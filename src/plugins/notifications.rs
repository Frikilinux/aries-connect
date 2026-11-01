//! Notification mirroring (`kdeconnect.notification`).
//!
//! The phone sends one packet per notification and another one with
//! `isCancel: true` when it is dismissed. Dismissals requested from the
//! desktop go back as `kdeconnect.notification.request` with `cancel: <id>`.
//!
//! Only paired devices are accepted: the control channel itself is
//! unauthenticated until pairing, so an arbitrary LAN peer could otherwise
//! inject notifications.

use std::collections::HashMap;
use std::sync::Arc;

use serde::Deserialize;
use tokio::sync::RwLock;

use crate::app::{App, Event};
use crate::identity::NetworkPacket;

/// Packet sent by the phone for each notification.
pub const NOTIFICATION_PACKET: &str = "kdeconnect.notification";
/// Packet used to dismiss a notification on the phone.
pub const DISMISS_PACKET: &str = "kdeconnect.notification.request";

/// Upper bound per device; older entries are dropped first.
const MAX_PER_DEVICE: usize = 100;

/// One active notification.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Notification {
    pub id: String,
    pub app_name: String,
    pub title: String,
    pub text: String,
    /// Phone timestamp in milliseconds; used to order and expire entries.
    pub time: i64,
}

/// In-memory active notifications, grouped by device.
#[derive(Default)]
pub struct NotificationStore {
    devices: RwLock<HashMap<String, HashMap<String, Notification>>>,
}

impl NotificationStore {
    pub fn new() -> Self {
        Self::default()
    }

    /// Inserts or replaces a notification. Returns whether it was new.
    pub async fn upsert(&self, device_id: &str, notification: Notification) -> bool {
        let mut devices = self.devices.write().await;
        let per_device = devices.entry(device_id.to_owned()).or_default();
        let is_new = per_device
            .insert(notification.id.clone(), notification)
            .is_none();
        while per_device.len() > MAX_PER_DEVICE {
            let oldest = per_device
                .values()
                .min_by_key(|n| n.time)
                .map(|n| n.id.clone());
            match oldest {
                Some(id) => {
                    per_device.remove(&id);
                }
                None => break,
            }
        }
        is_new
    }

    /// Removes one notification; returns it when present.
    pub async fn remove(&self, device_id: &str, id: &str) -> Option<Notification> {
        let mut devices = self.devices.write().await;
        let removed = devices
            .get_mut(device_id)
            .and_then(|per_device| per_device.remove(id));
        if devices
            .get(device_id)
            .is_some_and(|per_device| per_device.is_empty())
        {
            devices.remove(device_id);
        }
        removed
    }

    /// Drops every notification of a device (disconnect or unpair).
    pub async fn clear_device(&self, device_id: &str) {
        self.devices.write().await.remove(device_id);
    }

    /// Lists active notifications ordered by phone timestamp.
    pub async fn list(&self) -> Vec<(String, Notification)> {
        let devices = self.devices.read().await;
        let mut out = Vec::new();
        for (device_id, per_device) in devices.iter() {
            for notification in per_device.values() {
                out.push((device_id.clone(), notification.clone()));
            }
        }
        out.sort_by_key(|(_, notification)| notification.time);
        out
    }
}

/// Wire format of a `kdeconnect.notification` body.
#[derive(Debug, Deserialize)]
struct IncomingNotification {
    #[serde(default)]
    id: serde_json::Value,
    #[serde(rename = "appName", default)]
    app_name: String,
    #[serde(default)]
    title: String,
    #[serde(default)]
    text: String,
    #[serde(rename = "isCancel", default)]
    is_cancel: bool,
    #[serde(default)]
    time: serde_json::Value,
}

/// Android sends the timestamp as a string; other clients send a number.
fn time_to_ms(value: &serde_json::Value) -> i64 {
    match value {
        serde_json::Value::Number(n) => n.as_i64().unwrap_or(0),
        serde_json::Value::String(s) => s.parse().unwrap_or(0),
        _ => 0,
    }
}

/// Android may send the id as a string or a number.
fn id_to_string(value: &serde_json::Value) -> Option<String> {
    match value {
        serde_json::Value::String(id) if !id.is_empty() => Some(id.clone()),
        serde_json::Value::Number(id) => Some(id.to_string()),
        _ => None,
    }
}

/// Handles notification packets. Returns `true` when the packet was consumed.
pub async fn handle_packet(
    app: &Arc<App>,
    device_id: Option<&str>,
    packet: &NetworkPacket,
) -> bool {
    if packet.packet_type != NOTIFICATION_PACKET {
        return false;
    }
    let Some(device_id) = device_id else {
        return true;
    };
    if app.trusted.get(device_id).await.is_none() {
        tracing::debug!(device = %device_id, "ignoring notification from an unpaired device");
        return true;
    }
    if !app
        .trusted
        .plugin_enabled(device_id, NOTIFICATION_PACKET)
        .await
    {
        tracing::debug!(device = %device_id, "notifications plugin is disabled for this device");
        return true;
    }
    let body: IncomingNotification = match serde_json::from_value(packet.body.clone()) {
        Ok(body) => body,
        Err(e) => {
            tracing::debug!(device = %device_id, error = %e, "malformed notification packet");
            return true;
        }
    };
    let Some(id) = id_to_string(&body.id) else {
        tracing::debug!(device = %device_id, "notification without id");
        return true;
    };
    tracing::debug!(
        device = %device_id,
        id = %id,
        app = %body.app_name,
        cancel = body.is_cancel,
        "notification packet"
    );

    if body.is_cancel {
        if app.notifications.remove(device_id, &id).await.is_some() {
            let _ = app.events.send(Event::NotificationDismissed {
                device_id: device_id.to_owned(),
                id,
            });
        }
        return true;
    }

    let notification = Notification {
        id,
        app_name: body.app_name,
        title: body.title,
        text: body.text,
        time: time_to_ms(&body.time),
    };
    tracing::debug!(
        device = %device_id,
        app = %notification.app_name,
        "notification received"
    );
    app.notifications
        .upsert(device_id, notification.clone())
        .await;
    let _ = app.events.send(Event::NotificationReceived {
        device_id: device_id.to_owned(),
        id: notification.id,
        app_name: notification.app_name,
        title: notification.title,
        text: notification.text,
    });
    true
}

/// Builds the packet that dismisses a notification on the phone.
pub fn dismiss_packet(id: &str) -> NetworkPacket {
    NetworkPacket::new(DISMISS_PACKET, serde_json::json!({ "cancel": id }))
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

    #[test]
    fn parses_android_notification_with_string_time() {
        let body: IncomingNotification = serde_json::from_value(serde_json::json!({
            "id": "0|com.example|1|null|10123",
            "appName": "Mail",
            "title": "Hello",
            "text": "body",
            "time": "1793488812970",
            "isClearable": true,
        }))
        .expect("android body parses");
        assert_eq!(body.app_name, "Mail");
        assert_eq!(body.title, "Hello");
        assert_eq!(time_to_ms(&body.time), 1_793_488_812_970);
        assert!(!body.is_cancel);
    }

    #[test]
    fn parses_numeric_time() {
        assert_eq!(time_to_ms(&serde_json::json!(42)), 42);
        assert_eq!(time_to_ms(&serde_json::json!("42")), 42);
        assert_eq!(time_to_ms(&serde_json::Value::Null), 0);
    }

    /// Paired device with `plugin` disabled or enabled via per-device overrides.
    async fn app_with_plugins(plugins: HashMap<String, bool>) -> Arc<App> {
        let local = LocalIdentity::new("a".repeat(32), "Desktop".to_owned(), 1716);
        let (events, _rx) = mpsc::unbounded_channel();
        let app = App::new(local, events, TrustedStore::in_memory());
        app.trusted
            .insert(TrustedDevice {
                id: "b".repeat(32),
                name: "Phone".to_owned(),
                cert_sha256: None,
                last_ip: None,
                plugins,
            })
            .await;
        app
    }

    #[tokio::test]
    async fn ignores_notifications_when_plugin_disabled() {
        let plugins = HashMap::from([(NOTIFICATION_PACKET.to_owned(), false)]);
        let app = app_with_plugins(plugins).await;
        let device_id = "b".repeat(32);
        let packet = NetworkPacket::new(
            NOTIFICATION_PACKET,
            serde_json::json!({ "id": "1|app|1|null|1", "appName": "Mail" }),
        );

        assert!(handle_packet(&app, Some(&device_id), &packet).await);
        assert!(
            app.notifications.list().await.is_empty(),
            "disabled devices must not mirror notifications"
        );
    }

    #[tokio::test]
    async fn accepts_notifications_when_plugin_enabled() {
        let app = app_with_plugins(HashMap::new()).await;
        let device_id = "b".repeat(32);
        let packet = NetworkPacket::new(
            NOTIFICATION_PACKET,
            serde_json::json!({ "id": "1|app|1|null|1", "appName": "Mail" }),
        );

        assert!(handle_packet(&app, Some(&device_id), &packet).await);
        assert_eq!(app.notifications.list().await.len(), 1);
    }
}
