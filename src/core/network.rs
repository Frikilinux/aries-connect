//! TCP transport: KDE Connect protocol v8 control channel.
//!
//! Handshake (as the TCP server, i.e. the side that accepts the connection):
//!
//! 1. The connecting peer sends its identity packet as **plaintext**, one
//!    newline-terminated JSON line.
//! 2. We then start the TLS handshake as the **client** (the TCP server is
//!    always the TLS client in KDE Connect), presenting our certificate.
//! 3. Protocol 8 requires exchanging identities a second time over TLS; both
//!    sides write their identity line first.
//! 4. From then on, packets are newline-delimited JSON.

use std::io;
use std::sync::Arc;
use std::time::Duration;

use rustls::ClientConfig;
use socket2::{Domain, Protocol, Socket, Type};
use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader};
use tokio::net::{TcpListener, TcpStream};
use tokio_rustls::{client::TlsStream, TlsConnector};

use crate::app::{App, Event};
use crate::core::pairing;
use crate::identity::{is_valid_device_id, parse_identity, parse_pair, NetworkPacket};

/// Maximum size for identity packets on the control channel.
pub const MAX_IDENTITY_BYTES: usize = 64 * 1024;
/// Maximum size for regular frames (notifications etc. are bigger).
pub const MAX_FRAME_BYTES: usize = 4 * 1024 * 1024;
/// How long the peer has to send its plaintext identity.
const PLAINTEXT_IDENTITY_TIMEOUT: Duration = Duration::from_secs(5);
/// How long the TLS handshake may take.
const TLS_HANDSHAKE_TIMEOUT: Duration = Duration::from_secs(10);
/// How long to wait for the post-handshake secure identity.
const SECURE_IDENTITY_TIMEOUT: Duration = Duration::from_secs(10);
/// Minimum protocol version we accept (v8 = double identity exchange).
const MIN_PROTOCOL_VERSION: i64 = 8;
/// TCP keepalive idle time before the first probe.
const KEEPALIVE_IDLE: Duration = Duration::from_secs(15);
/// Time between keepalive probes once the peer stops answering.
const KEEPALIVE_INTERVAL: Duration = Duration::from_secs(5);
/// Failed probes before the socket is considered dead (~30 s in total).
const KEEPALIVE_RETRIES: u32 = 3;

/// Enables TCP keepalive on an accepted control channel.
///
/// Without it, a peer that vanishes without closing its socket (Wi-Fi turned
/// off, phone out of range) leaves a half-open connection that looks alive
/// forever: `Connected` would stay `true` and the cached battery status would
/// never be cleared. With these values the kernel declares the peer dead after
/// roughly `KEEPALIVE_IDLE + KEEPALIVE_INTERVAL * KEEPALIVE_RETRIES`.
fn enable_keepalive(stream: &TcpStream) -> io::Result<()> {
    let socket = socket2::SockRef::from(stream);
    socket.set_keepalive(true)?;
    socket.set_tcp_keepalive(
        &socket2::TcpKeepalive::new()
            .with_time(KEEPALIVE_IDLE)
            .with_interval(KEEPALIVE_INTERVAL)
            .with_retries(KEEPALIVE_RETRIES),
    )
}

/// TCP listener bound with SO_REUSEADDR (port 1716 is shared with UDP).
async fn tcp_listener(port: u16) -> io::Result<TcpListener> {
    let socket = Socket::new(Domain::IPV4, Type::STREAM, Some(Protocol::TCP))?;
    socket.set_reuse_address(true)?;
    let addr: std::net::SocketAddr = format!("0.0.0.0:{port}")
        .parse()
        .map_err(io::Error::other)?;
    socket.bind(&addr.into())?;
    socket.listen(16)?;
    socket.set_nonblocking(true)?;
    TcpListener::from_std(socket.into())
}

/// Builds the TLS connector from the daemon certificate.
pub fn tls_connector(cert: &crate::tls::IdentityCertificate) -> io::Result<TlsConnector> {
    let config: ClientConfig = crate::tls::client_config(cert)?;
    Ok(TlsConnector::from(Arc::new(config)))
}

/// Accepts peers forever; every connection runs as an independent task.
/// Returns the actually bound port (useful with ephemeral port 0).
pub async fn spawn(app: Arc<App>, connector: TlsConnector, port: u16) -> io::Result<u16> {
    let listener = tcp_listener(port).await?;
    let bound = listener.local_addr()?.port();
    tokio::spawn(accept_loop(app, connector, listener));
    Ok(bound)
}

async fn accept_loop(app: Arc<App>, connector: TlsConnector, listener: TcpListener) {
    tracing::info!(
        port = listener.local_addr().map(|a| a.port()).unwrap_or(0),
        "TCP control channel listening"
    );
    loop {
        let (stream, peer) = match listener.accept().await {
            Ok(v) => v,
            Err(e) => {
                tracing::warn!(error = %e, "tcp accept error");
                tokio::time::sleep(Duration::from_millis(500)).await;
                continue;
            }
        };
        tracing::debug!(%peer, "incoming TCP connection");
        if let Err(e) = enable_keepalive(&stream) {
            tracing::warn!(%peer, error = %e, "failed to enable TCP keepalive");
        }
        let app: Arc<App> = app.clone();
        let connector = connector.clone();
        tokio::spawn(async move {
            match handle_connection(app.clone(), connector, stream).await {
                Ok(()) => {}
                Err(e) => {
                    tracing::debug!(%peer, "connection ended: {e}");
                }
            }
        });
    }
}

/// Reads a newline-terminated line byte-by-byte.
///
/// Byte-wise reading matters for the plaintext identity: any byte read past
/// the newline would be part of the TLS handshake and must stay in the socket
/// buffer for the TLS client we start next.
async fn read_plain_line(
    stream: &mut TcpStream,
    max: usize,
    timeout: Duration,
) -> io::Result<Vec<u8>> {
    tokio::time::timeout(timeout, async {
        let mut out = Vec::with_capacity(1024);
        loop {
            let byte = stream.read_u8().await?;
            if byte == b'\n' {
                return Ok(out);
            }
            out.push(byte);
            if out.len() > max {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    "identity packet too large",
                ));
            }
        }
    })
    .await
    .map_err(|_| io::Error::new(io::ErrorKind::TimedOut, "peer did not send its identity"))?
}

/// Reads a newline-terminated line from a buffered TLS stream.
async fn read_tls_line<R>(
    reader: &mut BufReader<R>,
    max: usize,
    timeout: Duration,
) -> io::Result<Vec<u8>>
where
    R: tokio::io::AsyncRead + Unpin,
{
    tokio::time::timeout(timeout, async {
        let mut line = Vec::with_capacity(1024);
        let n = reader.read_until(b'\n', &mut line).await?;
        if n == 0 {
            return Err(io::Error::new(
                io::ErrorKind::UnexpectedEof,
                "connection closed mid-handshake",
            ));
        }
        if line.len() > max {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "packet too large",
            ));
        }
        if line.last() == Some(&b'\n') {
            line.pop();
        }
        Ok(line)
    })
    .await
    .map_err(|_| io::Error::new(io::ErrorKind::TimedOut, "timed out waiting for packet"))?
}

/// Runs one connection until EOF.
async fn handle_connection(
    app: Arc<App>,
    connector: TlsConnector,
    mut stream: TcpStream,
) -> Result<(), io::Error> {
    // --- 1. Plaintext identity from the connecting peer -------------------
    let plain_line =
        read_plain_line(&mut stream, MAX_IDENTITY_BYTES, PLAINTEXT_IDENTITY_TIMEOUT).await?;
    let plain_packet: NetworkPacket = parse_json(&plain_line, "plaintext identity")?;
    let plain = parse_identity(&plain_packet)
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidData, "not an identity packet"))?;

    tracing::info!(
        id = %plain.device_id,
        name = %plain.device_name,
        protocol = plain.protocol_version,
        "peer announced itself (plaintext)"
    );

    if plain.protocol_version < MIN_PROTOCOL_VERSION {
        return Err(io::Error::new(
            io::ErrorKind::Unsupported,
            format!("peer protocol {} is too old", plain.protocol_version),
        ));
    }
    if !is_valid_device_id(&plain.device_id) {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "peer device id does not match KDE Connect's format",
        ));
    }
    // Connection targeting: the peer may ask for a specific device/version.
    if let Some(target) = plain_packet
        .body
        .get("targetDeviceId")
        .and_then(|v| v.as_str())
    {
        if target != app.local.device_id {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "connection request for another device id",
            ));
        }
    }

    // --- 2. TLS handshake as client over the same socket -------------------
    let server_name = rustls_pki_types::ServerName::try_from("kdeconnect")
        .map_err(|_| io::Error::new(io::ErrorKind::InvalidInput, "bad TLS server name"))?
        .to_owned();
    let tls: TlsStream<TcpStream> = tokio::time::timeout(
        TLS_HANDSHAKE_TIMEOUT,
        connector.connect(server_name, stream),
    )
    .await
    .map_err(|_| io::Error::new(io::ErrorKind::TimedOut, "TLS handshake timed out"))?
    .map_err(io::Error::other)?;
    tracing::debug!(id = %plain.device_id, "TLS established");

    // Certificate pinning: a paired device must present the same certificate
    // it used when it was trusted.
    let peer_cert_sha256 = tls
        .get_ref()
        .1
        .peer_certificates()
        .and_then(|certs| certs.first())
        .map(|cert| crate::trusted::fingerprint(cert.as_ref()));
    if let Some(trusted) = app.trusted.get(&plain.device_id).await {
        if let (Some(stored), Some(current)) = (&trusted.cert_sha256, &peer_cert_sha256) {
            if stored != current {
                return Err(io::Error::new(
                    io::ErrorKind::PermissionDenied,
                    "peer certificate does not match the pinned one",
                ));
            }
        }
    }

    let (tls_reader, mut writer) = tokio::io::split(tls);
    let mut reader = BufReader::new(tls_reader);

    // --- 3. Identity exchange over TLS (protocol >= 8) ---------------------
    send_packet(&mut writer, &app.local.packet()).await?;
    let secure_line =
        read_tls_line(&mut reader, MAX_IDENTITY_BYTES, SECURE_IDENTITY_TIMEOUT).await?;
    let secure_packet: NetworkPacket = parse_json(&secure_line, "secure identity")?;
    let secure = parse_identity(&secure_packet).ok_or_else(|| {
        io::Error::new(io::ErrorKind::InvalidData, "not a secure identity packet")
    })?;
    if secure.device_id != plain.device_id {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "device id changed during handshake",
        ));
    }
    if secure.protocol_version != plain.protocol_version {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "protocol version changed during handshake",
        ));
    }

    let peer_id = secure.device_id.clone();
    tracing::debug!(
        id = %peer_id,
        incoming = ?secure.incoming_capabilities,
        outgoing = ?secure.outgoing_capabilities,
        "peer capabilities"
    );
    app.seen(&secure, true).await;
    app.registry.set_connected(&peer_id, true).await;
    tracing::info!(
        id = %peer_id,
        name = %secure.device_name,
        "control channel established"
    );
    let _ = app.events.send(Event::Connected {
        id: peer_id.clone(),
    });

    // Outgoing packets go through a channel so writers always target the
    // device's current link, even if the peer replaces its socket meanwhile.
    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel::<NetworkPacket>();
    let generation = app.register_link(&peer_id, tx, peer_cert_sha256).await;
    let mut writer = writer;
    let writer_task = tokio::spawn(async move {
        while let Some(packet) = rx.recv().await {
            if writer.write_all(&packet.to_frame()).await.is_err() {
                break;
            }
            if writer.flush().await.is_err() {
                break;
            }
        }
    });

    // --- 4. Regular packet loop -------------------------------------------
    let loop_result = async {
        loop {
            let line = match read_tls_line(&mut reader, MAX_FRAME_BYTES, Duration::from_secs(3600))
                .await
            {
                Ok(l) => l,
                Err(e) if e.kind() == io::ErrorKind::UnexpectedEof => break,
                Err(e) => return Err(e),
            };
            if line.is_empty() {
                continue;
            }
            let packet: NetworkPacket = match parse_json(&line, "packet") {
                Ok(p) => p,
                Err(e) => {
                    tracing::debug!(error = %e, "malformed packet, ignoring");
                    continue;
                }
            };
            dispatch(&app, Some(peer_id.clone()), packet).await;
        }
        Ok::<(), io::Error>(())
    }
    .await;

    // Android replaces its socket frequently; only report the disconnect when
    // the closed link is still the newest one for this device.
    let was_current = app.unregister_link(&peer_id, generation).await;
    writer_task.abort();
    if was_current {
        tracing::info!(device = %peer_id, "control channel closed");
        app.registry.set_connected(&peer_id, false).await;
        // Do not keep a stale battery level while the phone is offline.
        app.battery.clear_device(&peer_id).await;
        let _ = app.events.send(Event::Disconnected {
            id: peer_id.clone(),
        });
    }
    loop_result?;
    Ok(())
}

fn parse_json(bytes: &[u8], what: &str) -> io::Result<NetworkPacket> {
    serde_json::from_slice(bytes).map_err(|e| {
        tracing::debug!(kind = what, error = %e, "invalid JSON packet");
        io::Error::new(io::ErrorKind::InvalidData, format!("invalid {what}"))
    })
}

/// Routes one packet to its handler.
async fn dispatch(app: &Arc<App>, peer_id: Option<String>, packet: NetworkPacket) {
    // A peer that is not in the trust store but keeps sending plugin packets
    // still believes it is paired: this happens when the desktop unpairs while
    // the phone is offline, so the `pair: false` notification was lost. Tell
    // it to forget the pairing, like kdeconnect-kde does. Pair and identity
    // packets are handled below.
    if packet.packet_type != "kdeconnect.identity" && packet.packet_type != "kdeconnect.pair" {
        if let Some(id) = peer_id.as_deref() {
            if app.trusted.get(id).await.is_none() {
                tracing::info!(device = %id, "unpaired peer sent a packet, sending unpair");
                app.unpair(id, true).await;
                return;
            }
        }
    }

    match packet.packet_type.as_str() {
        "kdeconnect.identity" => {
            // Late identity updates (e.g. device rename) are accepted.
            if let Some(body) = parse_identity(&packet) {
                if body.device_id == app.local.device_id {
                    return;
                }
                app.seen(&body, true).await;
            }
        }
        "kdeconnect.pair" => {
            let accept = parse_pair(&packet).map(|b| b.pair).unwrap_or(false);
            tracing::debug!(accept, device = ?peer_id, "pair packet received");
            // Handled in its own task: the decision may take up to 30 s and
            // must not block reading further packets on this connection.
            tokio::spawn(pairing::handle_pair(app.clone(), peer_id, accept));
        }
        "kdeconnect.battery" => {
            crate::plugins::battery::handle_packet(app, peer_id.as_deref(), &packet).await;
        }
        "kdeconnect.clipboard" | "kdeconnect.clipboard.connect" => {
            crate::plugins::clipboard::handle_packet(app, peer_id.as_deref(), &packet).await;
        }
        "kdeconnect.notification" => {
            crate::plugins::notifications::handle_packet(app, peer_id.as_deref(), &packet).await;
        }
        "kdeconnect.ping" => {
            crate::plugins::ping::handle_packet(app, peer_id.as_deref(), &packet).await;
        }
        other => {
            tracing::debug!(unknown = other, "ignoring unsupported packet type");
        }
    }
}

async fn send_packet<W>(writer: &mut W, packet: &NetworkPacket) -> Result<(), io::Error>
where
    W: AsyncWriteExt + Unpin,
{
    writer.write_all(&packet.to_frame()).await?;
    writer.flush().await
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn accepted_sockets_enable_keepalive() {
        let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind");
        let addr = listener.local_addr().expect("local address");
        let _client = TcpStream::connect(addr).await.expect("connect");
        let (server, _) = listener.accept().await.expect("accept");

        enable_keepalive(&server).expect("enable keepalive");

        let socket = socket2::SockRef::from(&server);
        assert!(socket.keepalive().expect("keepalive state"));
    }
}
