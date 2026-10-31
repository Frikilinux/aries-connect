//! Local end-to-end test of the KDE Connect v8 control channel.
//!
//! The daemon is the TCP **server**: the test plays the phone (TCP client)
//! which first sends its identity in plaintext, then acts as the TLS **server**
//! and finally exchanges identities over TLS, like the Android app does.

use std::sync::Arc;
use std::time::Duration;

use rustls::ServerConfig;
use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader};
use tokio::net::TcpStream;
use tokio::sync::mpsc::UnboundedReceiver;
use tokio_rustls::server::TlsStream;
use tokio_rustls::TlsAcceptor;

use aries_connect::app::{App, Event, PairDecision};
use aries_connect::identity::{pair_packet, parse_identity, LocalIdentity, NetworkPacket};
use aries_connect::tls::IdentityCertificate;

const DAEMON_ID: &str = "0123456789abcdef0123456789abcdef";
const PHONE_ID: &str = "fedcba9876543210fedcba9876543210";

/// Simulated phone identity packet.
fn phone_identity(port: u16) -> NetworkPacket {
    LocalIdentity::new(PHONE_ID.to_owned(), "Test Phone".to_owned(), port).packet()
}

/// Reads one newline-framed JSON packet.
async fn read_packet(reader: &mut BufReader<TlsStream<TcpStream>>) -> NetworkPacket {
    let mut line = Vec::new();
    let n = reader.read_until(b'\n', &mut line).await.unwrap();
    assert!(n > 0, "connection closed while waiting for a packet");
    serde_json::from_slice(&line[..n]).expect("frame parses as JSON")
}

async fn send_packet(reader: &mut BufReader<TlsStream<TcpStream>>, packet: &NetworkPacket) {
    reader
        .get_mut()
        .write_all(&packet.to_frame())
        .await
        .unwrap();
    reader.get_mut().flush().await.unwrap();
}

/// Test setup: daemon core on an ephemeral port. Returns the app, the port and
/// the event receiver so tests can react to pairing requests (no D-Bus).
async fn spawn_daemon() -> (Arc<App>, u16, UnboundedReceiver<Event>) {
    let dir = tempfile::tempdir().expect("tempdir");
    let cert = IdentityCertificate::load_or_create(dir.path().to_owned())
        .await
        .expect("certificate");
    let connector = aries_connect::core::network::tls_connector(&cert).expect("connector");

    let (events_tx, events_rx) = tokio::sync::mpsc::unbounded_channel();
    let app = App::new(
        LocalIdentity::new(DAEMON_ID.to_owned(), "Test Desktop".to_owned(), 1716),
        events_tx,
        aries_connect::trusted::TrustedStore::in_memory(),
    );

    let port = aries_connect::core::network::spawn(app.clone(), connector, 0)
        .await
        .expect("spawn daemon core");

    std::mem::forget(dir);
    (app, port, events_rx)
}

/// Acts like the Android app during pairing: plaintext identity, then TLS as
/// the server, then the secure identity exchange.
async fn phone_handshake(port: u16) -> BufReader<TlsStream<TcpStream>> {
    // The phone needs its own certificate for the TLS server side.
    let phone_dir = tempfile::tempdir().expect("phone tempdir");
    let phone_cert = IdentityCertificate::load_or_create(phone_dir.path().to_owned())
        .await
        .expect("phone certificate");
    let acceptor = TlsAcceptor::from(Arc::new(
        ServerConfig::builder()
            .with_no_client_auth()
            .with_single_cert(vec![phone_cert.cert.clone()], phone_cert.key.clone_key())
            .expect("phone TLS config"),
    ));
    std::mem::forget(phone_dir);

    let mut tcp = TcpStream::connect(("127.0.0.1", port)).await.unwrap();

    // 1. Plaintext identity (newline terminated).
    tcp.write_all(&phone_identity(port).to_frame())
        .await
        .unwrap();
    tcp.flush().await.unwrap();

    // 2. The daemon starts TLS as the client; we are the server.
    let tls = acceptor.accept(tcp).await.expect("phone TLS server");
    let mut reader = BufReader::new(tls);

    // 3. Identity exchange over TLS: both sides write first.
    reader
        .get_mut()
        .write_all(&phone_identity(port).to_frame())
        .await
        .unwrap();
    reader.get_mut().flush().await.unwrap();

    let daemon_identity = read_packet(&mut reader).await;
    let body = parse_identity(&daemon_identity).expect("daemon identity");
    assert_eq!(body.device_id, DAEMON_ID);

    reader
}

/// Reacts to a PairingRequested event the way D-Bus AcceptPairing does.
async fn auto_accept_pairing(app: Arc<App>, mut events: UnboundedReceiver<Event>) {
    while let Some(ev) = events.recv().await {
        if let Event::PairingRequested { id, .. } = ev {
            assert!(
                app.decide_pairing(&id, PairDecision::Accept).await,
                "decision accepted by the daemon"
            );
            return;
        }
    }
    panic!("PairingRequested never arrived");
}

#[tokio::test]
async fn pairing_accept_flow_works() {
    let (app, port, events) = spawn_daemon().await;
    tokio::spawn(auto_accept_pairing(app.clone(), events));

    let mut reader = phone_handshake(port).await;

    // Request pairing and wait for the reply (bounded against hangs).
    send_packet(&mut reader, &pair_packet(true)).await;
    let reply = tokio::time::timeout(Duration::from_secs(10), read_packet(&mut reader))
        .await
        .expect("pair reply must arrive before the timeout");
    assert_eq!(reply.packet_type, "kdeconnect.pair");
    assert_eq!(
        reply.body.get("pair").and_then(|v| v.as_bool()),
        Some(true),
        "daemon accepted the pairing request"
    );

    let devices = app.registry.list().await;
    let peer = devices
        .into_iter()
        .find(|d| d.id == PHONE_ID)
        .expect("phone registered in the daemon");
    assert!(peer.paired);
    assert!(peer.connected);
}

#[tokio::test]
async fn rejects_old_protocol_versions() {
    let (_, port, _events) = spawn_daemon().await;

    let mut tcp = TcpStream::connect(("127.0.0.1", port)).await.unwrap();
    let mut old = LocalIdentity::new(PHONE_ID.to_owned(), "Old Phone".to_owned(), port).packet();
    old.body["protocolVersion"] = serde_json::json!(7);
    tcp.write_all(&old.to_frame()).await.unwrap();
    tcp.flush().await.unwrap();

    // The daemon must close the connection instead of starting TLS.
    let mut buf = [0u8; 1];
    let res = tokio::time::timeout(Duration::from_secs(5), tcp.read(&mut buf)).await;
    assert!(matches!(res, Ok(Ok(0))), "expected EOF, got {res:?}");
}
