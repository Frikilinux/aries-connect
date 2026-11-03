//! Battery status (`kdeconnect.battery`).
//!
//! The phone pushes its battery status when the control channel is
//! established and again whenever the level, the charging state or the
//! low-battery threshold changes. The old `kdeconnect.battery.request` packet
//! is deprecated and current clients do not answer it, so the daemon only
//! consumes status packets.
//!
//! Only paired devices are accepted, and only while the `kdeconnect.battery`
//! plugin is enabled for that device.

use std::collections::HashMap;
use std::sync::Arc;

use serde::Deserialize;
use tokio::sync::RwLock;

use crate::app::{App, Event};
use crate::identity::NetworkPacket;

/// Packet sent by the device that has a battery.
pub const BATTERY_PACKET: &str = "kdeconnect.battery";

/// Last known battery state of one device.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct BatteryStatus {
    /// Percentage in `0..=100`, or `-1` when unknown or the device has no
    /// battery.
    pub level: i32,
    /// Whether the device reported that it is charging.
    pub charging: bool,
}

impl BatteryStatus {
    /// State used before the first report, after disconnecting or unpairing
    /// and while the plugin is disabled.
    pub const UNKNOWN: Self = Self {
        level: -1,
        charging: false,
    };
}

/// Last reported battery state, grouped by device.
#[derive(Default)]
pub struct BatteryStore {
    devices: RwLock<HashMap<String, BatteryStatus>>,
}

impl BatteryStore {
    pub fn new() -> Self {
        Self::default()
    }

    /// Inserts or replaces the status. Returns whether it changed, so callers
    /// only announce real updates.
    pub async fn upsert(&self, device_id: &str, status: BatteryStatus) -> bool {
        let mut devices = self.devices.write().await;
        if devices.get(device_id) == Some(&status) {
            return false;
        }
        devices.insert(device_id.to_owned(), status);
        true
    }

    /// Last known status; [`BatteryStatus::UNKNOWN`] when nothing was
    /// reported yet.
    pub async fn get(&self, device_id: &str) -> BatteryStatus {
        self.devices
            .read()
            .await
            .get(device_id)
            .copied()
            .unwrap_or(BatteryStatus::UNKNOWN)
    }

    /// Drops the status of a device (unpair or plugin disabled).
    pub async fn clear_device(&self, device_id: &str) {
        self.devices.write().await.remove(device_id);
    }
}

/// Wire format of a `kdeconnect.battery` body. Unknown fields such as
/// `thresholdEvent` are ignored.
#[derive(Debug, Deserialize)]
struct IncomingBattery {
    /// Kept as a JSON value because clients may send an integer or a float.
    #[serde(rename = "currentCharge", default)]
    current_charge: serde_json::Value,
    #[serde(rename = "isCharging", default)]
    is_charging: bool,
}

/// Normalizes a reported charge into `-1..=100`; missing or invalid values
/// count as unknown.
fn level_to_i32(value: &serde_json::Value) -> i32 {
    let Some(number) = value.as_f64() else {
        return -1;
    };
    (number.round() as i64).clamp(-1, 100) as i32
}

/// Handles battery packets. Returns `true` when the packet was consumed.
pub async fn handle_packet(
    app: &Arc<App>,
    device_id: Option<&str>,
    packet: &NetworkPacket,
) -> bool {
    if packet.packet_type != BATTERY_PACKET {
        return false;
    }
    let Some(device_id) = device_id else {
        return true;
    };
    if app.trusted.get(device_id).await.is_none() {
        tracing::debug!(device = %device_id, "ignoring battery from an unpaired device");
        return true;
    }
    if !app.trusted.plugin_enabled(device_id, BATTERY_PACKET).await {
        tracing::debug!(device = %device_id, "battery plugin is disabled for this device");
        return true;
    }
    let body: IncomingBattery = match serde_json::from_value(packet.body.clone()) {
        Ok(body) => body,
        Err(e) => {
            tracing::debug!(device = %device_id, error = %e, "malformed battery packet");
            return true;
        }
    };
    let status = BatteryStatus {
        level: level_to_i32(&body.current_charge),
        charging: body.is_charging,
    };
    if app.battery.upsert(device_id, status).await {
        tracing::debug!(
            device = %device_id,
            level = status.level,
            charging = status.charging,
            "battery status changed"
        );
        let _ = app.events.send(Event::BatteryChanged {
            device_id: device_id.to_owned(),
            level: status.level,
            charging: status.charging,
        });
    }
    true
}

#[cfg(test)]
mod tests {
    use super::*;

    use tokio::sync::mpsc;

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

    fn battery_packet(level: i64, charging: bool) -> NetworkPacket {
        NetworkPacket::new(
            BATTERY_PACKET,
            serde_json::json!({ "currentCharge": level, "isCharging": charging }),
        )
    }

    #[tokio::test]
    async fn emits_event_and_stores_incoming_battery() {
        let (app, mut events) = paired_app(HashMap::new()).await;

        assert!(handle_packet(&app, Some(DEVICE_ID), &battery_packet(75, true)).await);
        match events.try_recv().expect("battery event") {
            Event::BatteryChanged {
                device_id,
                level,
                charging,
            } => {
                assert_eq!(device_id, DEVICE_ID);
                assert_eq!(level, 75);
                assert!(charging);
            }
            other => panic!("unexpected event: {other:?}"),
        }
        assert_eq!(
            app.battery.get(DEVICE_ID).await,
            BatteryStatus {
                level: 75,
                charging: true
            }
        );
    }

    #[tokio::test]
    async fn missing_charge_counts_as_unknown() {
        let (app, mut events) = paired_app(HashMap::new()).await;
        let packet = NetworkPacket::new(BATTERY_PACKET, serde_json::json!({ "isCharging": true }));

        assert!(handle_packet(&app, Some(DEVICE_ID), &packet).await);
        match events.try_recv().expect("battery event") {
            Event::BatteryChanged {
                level, charging, ..
            } => {
                assert_eq!(level, -1);
                assert!(charging);
            }
            other => panic!("unexpected event: {other:?}"),
        }
    }

    #[tokio::test]
    async fn ignores_repeated_status() {
        let (app, mut events) = paired_app(HashMap::new()).await;
        let packet = battery_packet(50, false);

        assert!(handle_packet(&app, Some(DEVICE_ID), &packet).await);
        events.try_recv().expect("first battery event");
        assert!(handle_packet(&app, Some(DEVICE_ID), &packet).await);
        assert!(
            events.try_recv().is_err(),
            "unchanged status must not be announced again"
        );
    }

    #[tokio::test]
    async fn ignores_battery_when_plugin_disabled() {
        let plugins = HashMap::from([(BATTERY_PACKET.to_owned(), false)]);
        let (app, mut events) = paired_app(plugins).await;

        assert!(handle_packet(&app, Some(DEVICE_ID), &battery_packet(20, false)).await);
        assert!(
            events.try_recv().is_err(),
            "disabled devices must not report battery"
        );
        assert_eq!(app.battery.get(DEVICE_ID).await, BatteryStatus::UNKNOWN);
    }

    #[tokio::test]
    async fn ignores_battery_from_unpaired_device() {
        let local = LocalIdentity::new("a".repeat(32), "Desktop".to_owned(), 1716);
        let (events, mut events_rx) = mpsc::unbounded_channel();
        let app = App::new(local, events, TrustedStore::in_memory());

        assert!(handle_packet(&app, Some(DEVICE_ID), &battery_packet(20, false)).await);
        assert!(events_rx.try_recv().is_err(), "unpaired peers are ignored");
    }

    #[test]
    fn normalizes_charge_to_the_protocol_range() {
        assert_eq!(level_to_i32(&serde_json::json!(75)), 75);
        assert_eq!(level_to_i32(&serde_json::json!(75.4)), 75);
        assert_eq!(level_to_i32(&serde_json::json!(150)), 100);
        assert_eq!(level_to_i32(&serde_json::json!(-5)), -1);
        assert_eq!(level_to_i32(&serde_json::json!(-1)), -1);
        assert_eq!(level_to_i32(&serde_json::Value::Null), -1);
        assert_eq!(level_to_i32(&serde_json::json!("75")), -1);
    }

    #[test]
    fn ignores_unknown_body_fields() {
        let body: IncomingBattery = serde_json::from_value(serde_json::json!({
            "currentCharge": 42,
            "isCharging": false,
            "thresholdEvent": 1,
            "batteryQuantity": 1,
        }))
        .expect("body parses");
        assert_eq!(level_to_i32(&body.current_charge), 42);
        assert!(!body.is_charging);
    }
}
