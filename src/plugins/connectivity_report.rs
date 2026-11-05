//! Connectivity report (`kdeconnect.connectivity_report`).
//!
//! The phone pushes the coarse signal strength of each subscription (SIM).
//! The daemon stores the last report per device and exposes it as the
//! read-only `SignalStrengths` property. Reports are accepted only from paired
//! devices with the plugin enabled for them.

use std::collections::{BTreeMap, HashMap};
use std::sync::Arc;

use serde::Deserialize;
use tokio::sync::RwLock;

use crate::app::{App, Event};
use crate::identity::NetworkPacket;

/// Packet sent by the device that reports its connectivity.
pub const CONNECTIVITY_REPORT_PACKET: &str = "kdeconnect.connectivity_report";

/// State of one subscription (SIM).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SignalState {
    /// Cellular network type reported by the phone (`LTE`, `5G`, `HSPA`, ...).
    pub network_type: String,
    /// Coarse signal strength in `0..=4`.
    pub signal_strength: i32,
}

/// Signal states keyed by subscription id.
pub type SignalStrengths = BTreeMap<String, SignalState>;

/// Last report of each device.
#[derive(Default)]
pub struct ConnectivityStore {
    devices: RwLock<HashMap<String, SignalStrengths>>,
}

impl ConnectivityStore {
    pub fn new() -> Self {
        Self::default()
    }

    /// Inserts or replaces the report. Returns whether it changed, so callers
    /// only announce real updates.
    pub async fn upsert(&self, device_id: &str, strengths: SignalStrengths) -> bool {
        let mut devices = self.devices.write().await;
        if devices.get(device_id) == Some(&strengths) {
            return false;
        }
        devices.insert(device_id.to_owned(), strengths);
        true
    }

    /// Last known report; empty when nothing was reported yet.
    pub async fn get(&self, device_id: &str) -> SignalStrengths {
        self.devices
            .read()
            .await
            .get(device_id)
            .cloned()
            .unwrap_or_default()
    }

    /// Drops the report of a device (disconnect, unpair or plugin disabled).
    pub async fn clear_device(&self, device_id: &str) {
        self.devices.write().await.remove(device_id);
    }
}

/// Wire format of the `signalStrengths` field.
#[derive(Debug, Deserialize)]
struct IncomingReport {
    #[serde(rename = "signalStrengths")]
    signal_strengths: BTreeMap<String, IncomingSignal>,
}

#[derive(Debug, Deserialize)]
struct IncomingSignal {
    #[serde(rename = "networkType", default = "unknown_network_type")]
    network_type: String,
    /// Kept as a JSON value because the protocol declares a number and some
    /// clients may send a float.
    #[serde(rename = "signalStrength", default)]
    signal_strength: serde_json::Value,
}

fn unknown_network_type() -> String {
    "Unknown".to_owned()
}

/// Normalizes a reported strength into the protocol's `0..=4` range. Missing
/// or invalid values count as `0`.
fn strength_to_level(value: &serde_json::Value) -> i32 {
    let Some(number) = value.as_f64() else {
        return 0;
    };
    (number.round() as i64).clamp(0, 4) as i32
}

/// Extracts the map from a packet body. `None` when the packet does not carry
/// the `signalStrengths` dictionary.
fn parse_report(packet: &NetworkPacket) -> Option<SignalStrengths> {
    let body: IncomingReport = serde_json::from_value(packet.body.clone()).ok()?;
    Some(
        body.signal_strengths
            .into_iter()
            .map(|(subscription_id, signal)| {
                (
                    subscription_id,
                    SignalState {
                        network_type: signal.network_type,
                        signal_strength: strength_to_level(&signal.signal_strength),
                    },
                )
            })
            .collect(),
    )
}

/// Handles connectivity report packets. Returns `true` when the packet was
/// consumed.
pub async fn handle_packet(
    app: &Arc<App>,
    device_id: Option<&str>,
    packet: &NetworkPacket,
) -> bool {
    if packet.packet_type != CONNECTIVITY_REPORT_PACKET {
        return false;
    }
    let Some(device_id) = device_id else {
        return true;
    };
    if app.trusted.get(device_id).await.is_none() {
        tracing::debug!(
            device = %device_id,
            "ignoring connectivity report from an unpaired device"
        );
        return true;
    }
    if !app
        .trusted
        .plugin_enabled(device_id, CONNECTIVITY_REPORT_PACKET)
        .await
    {
        tracing::debug!(
            device = %device_id,
            "connectivity report plugin is disabled for this device"
        );
        return true;
    }
    let Some(report) = parse_report(packet) else {
        tracing::debug!(
            device = %device_id,
            "malformed connectivity report packet"
        );
        return true;
    };
    if app.connectivity.upsert(device_id, report).await {
        tracing::debug!(device = %device_id, "connectivity report changed");
        let _ = app.events.send(Event::ConnectivityChanged {
            device_id: device_id.to_owned(),
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

    fn report_packet(body: serde_json::Value) -> NetworkPacket {
        NetworkPacket::new(CONNECTIVITY_REPORT_PACKET, body)
    }

    fn signal(network_type: &str, signal_strength: i64) -> serde_json::Value {
        serde_json::json!({
            "networkType": network_type,
            "signalStrength": signal_strength,
        })
    }

    #[tokio::test]
    async fn stores_incoming_signal_strengths_and_emits_event() {
        let (app, mut events) = paired_app(HashMap::new()).await;
        let packet = report_packet(serde_json::json!({
            "signalStrengths": {
                "6": signal("LTE", 3),
                "17": signal("HSPA", 0),
            }
        }));

        assert!(handle_packet(&app, Some(DEVICE_ID), &packet).await);
        match events.try_recv().expect("connectivity event") {
            Event::ConnectivityChanged { device_id } => assert_eq!(device_id, DEVICE_ID),
            other => panic!("unexpected event: {other:?}"),
        }

        let strengths = app.connectivity.get(DEVICE_ID).await;
        assert_eq!(strengths.len(), 2);
        assert_eq!(
            strengths.get("6"),
            Some(&SignalState {
                network_type: "LTE".to_owned(),
                signal_strength: 3,
            })
        );
        assert_eq!(
            strengths.get("17"),
            Some(&SignalState {
                network_type: "HSPA".to_owned(),
                signal_strength: 0,
            })
        );
    }

    #[tokio::test]
    async fn ignores_repeated_report() {
        let (app, mut events) = paired_app(HashMap::new()).await;
        let packet = report_packet(serde_json::json!({
            "signalStrengths": { "6": signal("LTE", 3) }
        }));

        assert!(handle_packet(&app, Some(DEVICE_ID), &packet).await);
        events.try_recv().expect("first connectivity event");
        assert!(handle_packet(&app, Some(DEVICE_ID), &packet).await);
        assert!(
            events.try_recv().is_err(),
            "unchanged report must not be announced again"
        );
    }

    #[tokio::test]
    async fn ignores_report_when_plugin_disabled() {
        let plugins = HashMap::from([(CONNECTIVITY_REPORT_PACKET.to_owned(), false)]);
        let (app, mut events) = paired_app(plugins).await;
        let packet = report_packet(serde_json::json!({
            "signalStrengths": { "6": signal("LTE", 3) }
        }));

        assert!(handle_packet(&app, Some(DEVICE_ID), &packet).await);
        assert!(
            events.try_recv().is_err(),
            "disabled devices must not report connectivity"
        );
        assert!(app.connectivity.get(DEVICE_ID).await.is_empty());
    }

    #[tokio::test]
    async fn ignores_report_from_unpaired_device() {
        let local = LocalIdentity::new("a".repeat(32), "Desktop".to_owned(), 1716);
        let (events, mut events_rx) = mpsc::unbounded_channel();
        let app = App::new(local, events, TrustedStore::in_memory());
        let packet = report_packet(serde_json::json!({
            "signalStrengths": { "6": signal("LTE", 3) }
        }));

        assert!(handle_packet(&app, Some(DEVICE_ID), &packet).await);
        assert!(events_rx.try_recv().is_err(), "unpaired peers are ignored");
    }

    #[tokio::test]
    async fn ignores_packet_without_signal_strengths() {
        let (app, mut events) = paired_app(HashMap::new()).await;
        let packet = report_packet(serde_json::json!({ "networkType": 1 }));

        assert!(handle_packet(&app, Some(DEVICE_ID), &packet).await);
        assert!(events.try_recv().is_err());
        assert!(app.connectivity.get(DEVICE_ID).await.is_empty());
    }

    #[test]
    fn normalizes_strength_to_the_protocol_range() {
        assert_eq!(strength_to_level(&serde_json::json!(3)), 3);
        assert_eq!(strength_to_level(&serde_json::json!(3.4)), 3);
        assert_eq!(strength_to_level(&serde_json::json!(9)), 4);
        assert_eq!(strength_to_level(&serde_json::json!(-1)), 0);
        assert_eq!(strength_to_level(&serde_json::Value::Null), 0);
        assert_eq!(strength_to_level(&serde_json::json!("3")), 0);
    }

    #[test]
    fn missing_network_type_counts_as_unknown() {
        let packet = report_packet(serde_json::json!({
            "signalStrengths": { "6": { "signalStrength": 2 } }
        }));
        let report = parse_report(&packet).expect("report parses");
        assert_eq!(
            report.get("6"),
            Some(&SignalState {
                network_type: "Unknown".to_owned(),
                signal_strength: 2,
            })
        );
    }
}
