//! KDE Connect packets: identity representation and (de)serialization helpers.
//!
//! Packet format (protocol v7+): `{"id": <ts_ms>, "type": ..., "body": {...}}`.
//! Over TCP: newline-delimited JSON frames.
//! Over UDP: the JSON packet itself; legacy clients prepend a random salt
//! followed by `kdeconnect.`, which the parser tolerates.

use serde::{Deserialize, Serialize};

/// Device type we report to peers.
pub const DEVICE_TYPE: &str = "desktop";
/// Protocol version we speak (7 = capabilities, 8 = current).
pub const PROTOCOL_VERSION: i64 = 8;

// ---------------------------------------------------------------------------
// Generic packet
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct NetworkPacket {
    #[serde(default)]
    pub id: i64,
    #[serde(rename = "type")]
    pub packet_type: String,
    #[serde(default)]
    pub body: serde_json::Value,
}

impl NetworkPacket {
    pub fn new(packet_type: impl Into<String>, body: serde_json::Value) -> Self {
        Self {
            id: now_ms(),
            packet_type: packet_type.into(),
            body,
        }
    }

    /// Serializes the packet with the `\n` framing byte appended (TCP).
    pub fn to_frame(&self) -> Vec<u8> {
        let mut out = serde_json::to_vec(self).expect("packet serializes");
        out.push(b'\n');
        out
    }
}

fn now_ms() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}

// ---------------------------------------------------------------------------
// Identity
// ---------------------------------------------------------------------------

/// Body of a `kdeconnect.identity` packet as it travels on the wire.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct IdentityBody {
    #[serde(rename = "deviceId")]
    pub device_id: String,
    #[serde(rename = "deviceName")]
    pub device_name: String,
    #[serde(rename = "deviceType")]
    pub device_type: String,
    #[serde(rename = "protocolVersion")]
    pub protocol_version: i64,
    /// Optional TCP port (defaults to 1716 when absent).
    #[serde(rename = "tcpPort", default, skip_serializing_if = "Option::is_none")]
    pub tcp_port: Option<u16>,
    #[serde(rename = "incomingCapabilities", default)]
    pub incoming_capabilities: Vec<String>,
    #[serde(rename = "outgoingCapabilities", default)]
    pub outgoing_capabilities: Vec<String>,
}

/// Android validates device ids with `^[a-zA-Z0-9_-]{32,38}$`; anything else
/// is silently dropped as an "invalid identity packet".
pub fn is_valid_device_id(id: &str) -> bool {
    (32..=38).contains(&id.len())
        && id
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-')
}

/// Identity of the local daemon.
#[derive(Debug, Clone)]
pub struct LocalIdentity {
    pub device_id: String,
    pub device_name: String,
    pub tcp_port: u16,
    pub incoming_capabilities: Vec<String>,
    pub outgoing_capabilities: Vec<String>,
}

impl LocalIdentity {
    /// Capabilities declared by this iteration.
    ///
    /// The supported plugins are derived from [`crate::plugins::supported_plugins`]
    /// so the identity cannot drift from the `Plugins` property.
    pub fn new(device_id: String, device_name: String, tcp_port: u16) -> Self {
        let mut capabilities: Vec<String> = vec!["kdeconnect.pair".to_owned()];
        capabilities.extend(
            crate::plugins::supported_plugins()
                .iter()
                .map(|plugin| (*plugin).to_owned()),
        );
        capabilities.push("kdeconnect.notification.request".to_owned());

        Self {
            device_id,
            device_name,
            tcp_port,
            incoming_capabilities: capabilities.clone(),
            outgoing_capabilities: capabilities,
        }
    }

    pub fn packet(&self) -> NetworkPacket {
        let body = IdentityBody {
            device_id: self.device_id.clone(),
            device_name: self.device_name.clone(),
            device_type: DEVICE_TYPE.to_owned(),
            protocol_version: PROTOCOL_VERSION,
            tcp_port: Some(self.tcp_port),
            incoming_capabilities: self.incoming_capabilities.clone(),
            outgoing_capabilities: self.outgoing_capabilities.clone(),
        };
        NetworkPacket::new(
            "kdeconnect.identity",
            serde_json::to_value(&body).expect("identity body serializes"),
        )
    }
}

/// Extracts an `IdentityBody` from a packet if it is a valid identity.
pub fn parse_identity(packet: &NetworkPacket) -> Option<IdentityBody> {
    if packet.packet_type != "kdeconnect.identity" {
        return None;
    }
    serde_json::from_value(packet.body.clone()).ok()
}

// ---------------------------------------------------------------------------
// Pair
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PairBody {
    #[serde(rename = "pair")]
    pub pair: bool,
}

/// Builds a `kdeconnect.pair` packet.
pub fn pair_packet(accept: bool) -> NetworkPacket {
    let body = PairBody { pair: accept };
    NetworkPacket::new(
        "kdeconnect.pair",
        serde_json::to_value(&body).expect("pair body serializes"),
    )
}

/// Decodes the body of a `kdeconnect.pair` packet (None if it is not one).
pub fn parse_pair(packet: &NetworkPacket) -> Option<PairBody> {
    if packet.packet_type != "kdeconnect.pair" {
        return None;
    }
    serde_json::from_value(packet.body.clone()).ok()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::plugins::ping;

    #[test]
    fn identity_advertises_ping_in_both_directions() {
        let identity = LocalIdentity::new("a".repeat(32), "Desktop".to_owned(), 1716);
        for capabilities in [
            &identity.incoming_capabilities,
            &identity.outgoing_capabilities,
        ] {
            assert!(
                capabilities.iter().any(|c| c == ping::PING_PACKET),
                "identity must advertise {}",
                ping::PING_PACKET
            );
        }
    }
}
