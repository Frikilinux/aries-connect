//! UDP discovery on port 1716: announce the local identity and decode peers.
//!
//! Datagrams are plain JSON; the legacy random-salt + `kdeconnect.` prefix is
//! also accepted. Announcements go to the limited broadcast address, every
//! interface's directed broadcast and loopback, and each received identity is
//! answered unicast (Android sends from ephemeral ports, so `ip:1716` alone is
//! unreliable).

use std::collections::HashMap;
use std::io;
use std::net::{Ipv4Addr, SocketAddr};
use std::sync::Arc;
use std::time::Instant;

use socket2::{Domain, Protocol, Socket, Type};
use tokio::{net::UdpSocket, time};

use crate::app::App;
use crate::identity::NetworkPacket;

/// LAN broadcast interval for announcing ourselves.
const ANNOUNCE_INTERVAL: tokio::time::Duration = time::Duration::from_secs(10);

/// Minimum delay between unicast replies to the same device.
const REPLY_COOLDOWN: std::time::Duration = std::time::Duration::from_secs(3);

pub struct DiscoveryConfig {
    pub port: u16,
}

fn bind_addr(port: u16) -> SocketAddr {
    SocketAddr::from((Ipv4Addr::UNSPECIFIED, port))
}

/// Creates a UDP socket (broadcast + SO_REUSEADDR so the real KDE Connect
/// daemon can try to bind 1716 as well).
fn udp_socket(port: u16) -> io::Result<UdpSocket> {
    let socket = Socket::new(Domain::IPV4, Type::DGRAM, Some(Protocol::UDP))?;
    socket.set_reuse_address(true)?;
    socket.set_broadcast(true)?;
    socket.bind(&bind_addr(port).into())?;
    socket.set_nonblocking(true)?;
    UdpSocket::from_std(socket.into())
}

/// Serialized identity datagram.
///
/// Modern KDE Connect clients (Android included) send the JSON packet as-is
/// over UDP. Old ones prepend a random salt plus `kdeconnect.`. We emit both
/// variants when replying, because receiver implementations differ, and the
/// payloads are tiny.
fn identity_datagrams(app: &App) -> [Vec<u8>; 2] {
    let json = serde_json::to_vec(&app.local.packet()).expect("identity serializes");
    let mut prefixed = Vec::with_capacity(24 + json.len());
    prefixed.extend_from_slice(format!("{:x}", rand::random::<u64>()).as_bytes());
    prefixed.extend_from_slice(b"kdeconnect.");
    prefixed.extend_from_slice(&json);
    [json, prefixed]
}

/// All addresses we announce to: limited broadcast, per-interface directed
/// broadcasts and loopback.
fn announce_targets(port: u16) -> Vec<SocketAddr> {
    let mut targets = vec![
        SocketAddr::from((Ipv4Addr::BROADCAST, port)),
        SocketAddr::from((Ipv4Addr::LOCALHOST, port)),
    ];

    match if_addrs::get_if_addrs() {
        Ok(ifaces) => {
            for iface in ifaces {
                if let if_addrs::IfAddr::V4(v4) = iface.addr {
                    if v4.ip.is_loopback() || v4.ip.is_unspecified() {
                        continue;
                    }
                    if let Some(broadcast) = v4.broadcast {
                        targets.push(SocketAddr::from((broadcast, port)));
                    }
                }
            }
        }
        Err(e) => tracing::warn!(error = %e, "cannot enumerate network interfaces"),
    }

    targets.sort_by_key(|a| a.ip());
    targets.dedup_by_key(|a| a.ip());
    targets
}

/// Spawns the announce + receive loops. Handles socket errors without killing
/// the process.
pub async fn spawn(app: Arc<App>, config: DiscoveryConfig) -> io::Result<()> {
    let socket = Arc::new(udp_socket(config.port)?);
    let targets = announce_targets(config.port);
    tracing::info!(
        targets = ?targets,
        "UDP discovery active"
    );
    tokio::spawn(announce_loop(app.clone(), socket.clone(), targets.clone()));
    tokio::spawn(receive_loop(app, socket, config.port, targets));
    Ok(())
}

/// Periodically broadcasts our identity so phones can discover us.
async fn announce_loop(app: Arc<App>, socket: Arc<UdpSocket>, targets: Vec<SocketAddr>) {
    loop {
        let dgrams = identity_datagrams(&app);
        for target in &targets {
            for dgram in &dgrams {
                if let Err(e) = socket.send_to(dgram, target).await {
                    tracing::debug!(%target, error = %e, "broadcast failed");
                }
            }
        }
        tokio::time::sleep(ANNOUNCE_INTERVAL).await;
    }
}

/// Sends our identity straight to a peer we just heard from.
///
/// Android sends discovery datagrams from ephemeral UDP ports, so replying to
/// `ip:1716` may reach nobody: we answer to the exact source address *and*
/// to the conventional port, in both packet framings.
async fn reply_unicast(app: &App, socket: &UdpSocket, from: SocketAddr, our_port: u16) {
    let dgrams = identity_datagrams(app);
    let mut targets = vec![from];
    let conventional = SocketAddr::new(from.ip(), our_port);
    if conventional != from {
        targets.push(conventional);
    }
    for target in targets {
        for dgram in &dgrams {
            if let Err(e) = socket.send_to(dgram, target).await {
                tracing::debug!(%target, error = %e, "unicast reply failed");
            }
        }
    }
}

/// Hex preview of the first bytes of a datagram, for protocol debugging.
fn hex_preview(bytes: &[u8], max: usize) -> String {
    let shown = &bytes[..bytes.len().min(max)];
    let mut out = String::with_capacity(shown.len() * 2);
    for b in shown {
        out.push_str(&format!("{b:02x}"));
    }
    if bytes.len() > max {
        out.push_str("...");
    }
    out
}

/// Receives and decodes incoming identity datagrams.
async fn receive_loop(app: Arc<App>, socket: Arc<UdpSocket>, port: u16, targets: Vec<SocketAddr>) {
    let mut buf = vec![0u8; 65536];
    let mut last_reply: HashMap<SocketAddr, Instant> = HashMap::new();

    loop {
        let (len, from) = match socket.recv_from(&mut buf).await {
            Ok(v) => v,
            Err(e) => {
                tracing::warn!(error = %e, "udp recv error");
                tokio::time::sleep(time::Duration::from_secs(1)).await;
                continue;
            }
        };
        tracing::debug!(%from, bytes = len, "udp datagram received");

        let raw = &buf[..len];
        // Datagrams may carry the legacy `kdeconnect.` magic prefix (old
        // implementations) or be plain JSON (modern ones). Start at the
        // first `{`; searching for the magic marker is wrong because the
        // JSON itself contains `"kdeconnect.identity"`.
        let data = match raw.iter().position(|b| *b == b'{') {
            Some(idx) => &raw[idx..],
            None => {
                tracing::debug!(
                    from = %from,
                    hex = %hex_preview(raw, 80),
                    "ignoring datagram without JSON payload"
                );
                continue;
            }
        };

        let packet: NetworkPacket = match serde_json::from_slice(data) {
            Ok(p) => p,
            Err(e) => {
                tracing::debug!(
                    from = %from,
                    hex = %hex_preview(raw, 80),
                    ascii = %String::from_utf8_lossy(&raw[..raw.len().min(80)]),
                    error = %e,
                    "ignoring malformed datagram"
                );
                continue;
            }
        };

        match crate::identity::parse_identity(&packet) {
            Some(body) if body.device_id == app.local.device_id => {
                // Our own broadcast looping back (loopback or LAN echo).
                tracing::trace!("ignoring our own identity datagram");
            }
            Some(body) => {
                tracing::info!(
                    from = %from,
                    id = %body.device_id,
                    name = %body.device_name,
                    "discovered device"
                );
                let created = app.seen(&body, true).await;

                // Answer right away (rate-limited per source) so the peer
                // lists us even if it missed our periodic announce.
                let now = Instant::now();
                let recent = last_reply
                    .get(&from)
                    .is_some_and(|t| now.duration_since(*t) < REPLY_COOLDOWN);
                if !recent {
                    last_reply.insert(from, now);
                    reply_unicast(&app, &socket, from, port).await;
                    // Keep the rate-limit map from growing unbounded.
                    if last_reply.len() > 64 {
                        last_reply.retain(|_, t| now.duration_since(*t) < REPLY_COOLDOWN);
                    }
                }

                // A brand-new device may not know our directed broadcast, so
                // re-announce once to every interface (once per device, to
                // avoid broadcast ping-pong between daemons).
                if created {
                    let dgrams = identity_datagrams(&app);
                    for target in &targets {
                        for dgram in &dgrams {
                            let _ = socket.send_to(dgram, target).await;
                        }
                    }
                }
            }
            None => {
                tracing::debug!(?packet.packet_type, "datagram is not an identity packet");
            }
        }
    }
}
