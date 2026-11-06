//! UDP discovery on port 1716: announce the local identity and decode peers.
//!
//! Datagrams are plain JSON; the legacy random-salt + `kdeconnect.` prefix is
//! also accepted. Announcements go to the limited broadcast address, every
//! interface's directed broadcast, loopback and the last known address of each
//! peer (unicast), and each received identity is answered unicast (Android
//! sends from ephemeral ports, so `ip:1716` alone is unreliable).
//!
//! Unicast announcements matter while the phone screen is off: Android may
//! filter broadcast/multicast traffic in power saving, but unicast datagrams
//! still reach it and trigger discovery.

use std::collections::HashMap;
use std::io;
use std::net::{Ipv4Addr, SocketAddr};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use socket2::{Domain, Protocol, Socket, Type};
use tokio::{net::UdpSocket, time};

use crate::app::App;
use crate::identity::NetworkPacket;

/// LAN broadcast interval for announcing ourselves.
const ANNOUNCE_INTERVAL: tokio::time::Duration = time::Duration::from_secs(10);

/// Minimum delay between unicast replies to the same device.
const REPLY_COOLDOWN: std::time::Duration = std::time::Duration::from_secs(3);

/// A peer address is remembered for this long.
const PEER_TTL: Duration = Duration::from_secs(60 * 60);

/// Upper bound for the remembered peer table.
const MAX_PEERS: usize = 64;

/// Last known address of every device heard over UDP.
#[derive(Default)]
struct PeerBook {
    peers: Mutex<HashMap<String, PeerEntry>>,
}

#[derive(Clone, Copy)]
struct PeerEntry {
    addr: SocketAddr,
    seen: Instant,
}

impl PeerBook {
    /// Preloads a persisted address (paired devices at startup).
    fn seed(&self, device_id: &str, ip: Ipv4Addr, port: u16) {
        self.peers.lock().expect("peer book poisoned").insert(
            device_id.to_owned(),
            PeerEntry {
                addr: SocketAddr::new(ip.into(), port),
                seen: Instant::now(),
            },
        );
    }

    fn remember(&self, device_id: &str, from: SocketAddr) {
        let SocketAddr::V4(addr) = from else {
            return;
        };
        if addr.ip().is_loopback() || addr.ip().is_unspecified() {
            return;
        }
        let mut peers = self.peers.lock().expect("peer book poisoned");
        let now = Instant::now();
        if peers.len() >= MAX_PEERS {
            peers.retain(|_, entry| now.duration_since(entry.seen) < PEER_TTL);
            if peers.len() >= MAX_PEERS {
                peers.clear();
            }
        }
        peers.insert(
            device_id.to_owned(),
            PeerEntry {
                addr: from,
                seen: now,
            },
        );
    }

    /// Addresses to announce to: the conventional port on the peer IP and the
    /// last source port (Android may use an ephemeral one).
    fn targets(&self, port: u16) -> Vec<SocketAddr> {
        let now = Instant::now();
        let peers = self.peers.lock().expect("peer book poisoned");
        let mut targets = Vec::new();
        for entry in peers.values() {
            if now.duration_since(entry.seen) >= PEER_TTL {
                continue;
            }
            let conventional = SocketAddr::new(entry.addr.ip(), port);
            targets.push(conventional);
            if entry.addr != conventional {
                targets.push(entry.addr);
            }
        }
        targets
    }
}

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
async fn identity_datagrams(app: &App) -> [Vec<u8>; 2] {
    let json = serde_json::to_vec(&app.local_packet().await).expect("identity serializes");
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
    let peers = Arc::new(PeerBook::default());
    for device in app.trusted.all().await {
        if let Some(ip) = device
            .last_ip
            .as_deref()
            .and_then(|value| value.parse::<Ipv4Addr>().ok())
        {
            peers.seed(&device.id, ip, config.port);
        }
    }
    tracing::info!(
        targets = ?targets,
        "UDP discovery active"
    );
    tokio::spawn(announce_loop(
        app.clone(),
        socket.clone(),
        targets.clone(),
        peers.clone(),
        config.port,
    ));
    tokio::spawn(receive_loop(app, socket, config.port, targets, peers));
    Ok(())
}

/// Periodically announces our identity so phones can discover (and reconnect
/// to) us. Includes unicast to peers already heard from.
async fn announce_loop(
    app: Arc<App>,
    socket: Arc<UdpSocket>,
    broadcast_targets: Vec<SocketAddr>,
    peers: Arc<PeerBook>,
    port: u16,
) {
    loop {
        let dgrams = identity_datagrams(&app).await;
        let mut targets = broadcast_targets.clone();
        targets.extend(peers.targets(port));
        targets.sort_by_key(|a| a.ip());
        targets.dedup_by_key(|a| a.ip());
        for target in &targets {
            for dgram in &dgrams {
                if let Err(e) = socket.send_to(dgram, target).await {
                    tracing::debug!(%target, error = %e, "broadcast failed");
                }
            }
        }
        // A name change wakes the loop before the regular interval elapses.
        tokio::select! {
            () = tokio::time::sleep(ANNOUNCE_INTERVAL) => {}
            () = app.wait_for_announce() => {}
        }
    }
}

/// Sends our identity straight to a peer we just heard from.
///
/// Android sends discovery datagrams from ephemeral UDP ports, so replying to
/// `ip:1716` may reach nobody: we answer to the exact source address *and*
/// to the conventional port, in both packet framings.
async fn reply_unicast(app: &App, socket: &UdpSocket, from: SocketAddr, our_port: u16) {
    let dgrams = identity_datagrams(app).await;
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
async fn receive_loop(
    app: Arc<App>,
    socket: Arc<UdpSocket>,
    port: u16,
    targets: Vec<SocketAddr>,
    peers: Arc<PeerBook>,
) {
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

        let local_id = app.local_id().await;
        match crate::identity::parse_identity(&packet) {
            Some(body) if body.device_id == local_id => {
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
                peers.remember(&body.device_id, from);
                if let std::net::IpAddr::V4(ip) = from.ip() {
                    app.remember_peer_address(&body.device_id, &ip.to_string())
                        .await;
                }
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
                    let dgrams = identity_datagrams(&app).await;
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

#[cfg(test)]
mod tests {
    use super::*;

    fn addr(value: &str) -> SocketAddr {
        value.parse().expect("socket address")
    }

    #[test]
    fn remembers_conventional_and_source_ports() {
        let book = PeerBook::default();
        book.remember("phone", addr("192.168.1.20:40000"));
        let targets = book.targets(1716);
        assert!(targets.contains(&addr("192.168.1.20:1716")));
        assert!(targets.contains(&addr("192.168.1.20:40000")));
    }

    #[test]
    fn seeded_peer_is_announced() {
        let book = PeerBook::default();
        book.seed("phone", Ipv4Addr::new(192, 168, 1, 20), 1716);
        assert_eq!(book.targets(1716), vec![addr("192.168.1.20:1716")]);
    }

    #[test]
    fn ignores_loopback_and_replaces_old_address() {
        let book = PeerBook::default();
        book.remember("phone", addr("127.0.0.1:1716"));
        assert!(book.targets(1716).is_empty());

        book.remember("phone", addr("192.168.1.20:1716"));
        book.remember("phone", addr("192.168.1.21:1716"));
        assert_eq!(book.targets(1716), vec![addr("192.168.1.21:1716")]);
    }
}
