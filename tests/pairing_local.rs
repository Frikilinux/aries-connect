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

use aries_connect::app::{App, Event, PairDecision, PingError};
use aries_connect::identity::{pair_packet, parse_identity, LocalIdentity, NetworkPacket};
use aries_connect::plugins::battery::BatteryStatus;
use aries_connect::plugins::clipboard;
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

/// Simulated `kdeconnect.notification` packet.
fn notification_packet(id: &str, title: &str) -> NetworkPacket {
    NetworkPacket::new(
        "kdeconnect.notification",
        serde_json::json!({
            "id": id,
            "appName": "Test App",
            "title": title,
            "text": "body",
            "isClearable": true,
            "time": "1",
        }),
    )
}

/// Simulated dismissal coming from the phone.
fn cancel_packet(id: &str) -> NetworkPacket {
    NetworkPacket::new(
        "kdeconnect.notification",
        serde_json::json!({ "id": id, "isCancel": true }),
    )
}

/// Waits for the first event matching `predicate`.
async fn wait_event<F>(events: &mut UnboundedReceiver<Event>, mut predicate: F) -> Event
where
    F: FnMut(&Event) -> bool,
{
    tokio::time::timeout(Duration::from_secs(10), async {
        loop {
            match events.recv().await {
                Some(event) if predicate(&event) => return event,
                Some(_) => {}
                None => panic!("event channel closed"),
            }
        }
    })
    .await
    .expect("expected event before the timeout")
}

#[tokio::test]
async fn notification_flow_works() {
    let (app, port, mut events) = spawn_daemon().await;
    let mut reader = phone_handshake(port).await;
    app.mark_paired(PHONE_ID).await;

    send_packet(&mut reader, &notification_packet("n1", "Hello")).await;
    let event = wait_event(&mut events, |e| {
        matches!(e, Event::NotificationReceived { .. })
    })
    .await;
    match event {
        Event::NotificationReceived {
            device_id,
            id,
            app_name,
            title,
            text,
        } => {
            assert_eq!(device_id, PHONE_ID);
            assert_eq!(id, "n1");
            assert_eq!(app_name, "Test App");
            assert_eq!(title, "Hello");
            assert_eq!(text, "body");
        }
        other => panic!("unexpected event: {other:?}"),
    }

    let active = app.notifications.list().await;
    assert_eq!(active.len(), 1);
    assert_eq!(active[0].0, PHONE_ID);
    assert_eq!(active[0].1.id, "n1");

    // Dismissal from the desktop: removed locally, forwarded to the phone.
    assert!(app.dismiss_notification(PHONE_ID, "n1").await);
    let dismiss = tokio::time::timeout(Duration::from_secs(5), read_packet(&mut reader))
        .await
        .expect("dismiss packet");
    assert_eq!(dismiss.packet_type, "kdeconnect.notification.request");
    assert_eq!(
        dismiss.body.get("cancel").and_then(|v| v.as_str()),
        Some("n1")
    );
    assert!(app.notifications.list().await.is_empty());
    wait_event(&mut events, |e| {
        matches!(e, Event::NotificationDismissed { .. })
    })
    .await;

    // Dismissal on the phone removes it from the mirror.
    send_packet(&mut reader, &notification_packet("n2", "Second")).await;
    wait_event(
        &mut events,
        |e| matches!(e, Event::NotificationReceived { id, .. } if id == "n2"),
    )
    .await;
    send_packet(&mut reader, &cancel_packet("n2")).await;
    let event = wait_event(
        &mut events,
        |e| matches!(e, Event::NotificationDismissed { id, .. } if id == "n2"),
    )
    .await;
    assert!(matches!(event, Event::NotificationDismissed { .. }));
    assert!(app.notifications.list().await.is_empty());
}

#[tokio::test]
async fn notifications_from_unpaired_devices_are_ignored() {
    let (_app, port, mut events) = spawn_daemon().await;
    let mut reader = phone_handshake(port).await;

    wait_event(&mut events, |e| matches!(e, Event::Connected { .. })).await;
    send_packet(&mut reader, &notification_packet("n1", "Hello")).await;

    let result = tokio::time::timeout(Duration::from_secs(1), async {
        loop {
            match events.recv().await {
                Some(Event::NotificationReceived { .. }) => return,
                Some(_) => {}
                None => return,
            }
        }
    })
    .await;
    assert!(result.is_err(), "unpaired notification must be ignored");
}

/// Simulated `kdeconnect.ping` packet.
fn phone_ping(message: &str) -> NetworkPacket {
    NetworkPacket::new("kdeconnect.ping", serde_json::json!({ "message": message }))
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

#[tokio::test]
async fn unpair_notifies_peer_and_removes_device() {
    let (app, port, mut events) = spawn_daemon().await;
    let mut reader = phone_handshake(port).await;
    wait_event(&mut events, |e| matches!(e, Event::Connected { .. })).await;
    // The `Connected` event precedes link publication; give the connection
    // task a moment so `unpair` can deliver the notification.
    tokio::time::sleep(Duration::from_millis(100)).await;
    app.mark_paired(PHONE_ID).await;

    assert!(app.unpair(PHONE_ID, true).await, "device was paired");

    // The phone receives the unpair notification.
    let packet = tokio::time::timeout(Duration::from_secs(5), read_packet(&mut reader))
        .await
        .expect("unpair notification reaches the phone");
    assert_eq!(packet.packet_type, "kdeconnect.pair");
    assert_eq!(
        packet.body.get("pair").and_then(|v| v.as_bool()),
        Some(false),
        "daemon asks the phone to unpair"
    );

    // Local state is gone and the removal is announced.
    assert!(app.trusted.get(PHONE_ID).await.is_none());
    let device = app.registry.get(PHONE_ID).await.expect("device stays seen");
    assert!(!device.paired, "device is no longer paired");

    let event = wait_event(&mut events, |e| matches!(e, Event::PairingRemoved { .. })).await;
    match event {
        Event::PairingRemoved { id } => assert_eq!(id, PHONE_ID),
        other => panic!("unexpected event: {other:?}"),
    }
}

#[tokio::test]
async fn peer_unpair_does_not_echo_pair_packet() {
    let (app, port, mut events) = spawn_daemon().await;
    let mut reader = phone_handshake(port).await;
    app.mark_paired(PHONE_ID).await;

    // The phone unpairs.
    send_packet(&mut reader, &pair_packet(false)).await;

    // The daemon forgets the device and announces it.
    let event = wait_event(&mut events, |e| matches!(e, Event::PairingRemoved { .. })).await;
    assert!(matches!(event, Event::PairingRemoved { .. }));
    assert!(app.trusted.get(PHONE_ID).await.is_none());

    // ...without echoing another pair packet back to the phone.
    let echo = tokio::time::timeout(Duration::from_secs(1), read_packet(&mut reader)).await;
    assert!(
        echo.is_err(),
        "peer-initiated unpair must not send a pair packet back"
    );
}

#[tokio::test]
async fn ping_flow_works() {
    let (app, port, mut events) = spawn_daemon().await;
    let mut reader = phone_handshake(port).await;
    app.mark_paired(PHONE_ID).await;

    // Phone -> desktop: the ping is surfaced over D-Bus.
    send_packet(&mut reader, &phone_ping("Hello from the phone")).await;
    let event = wait_event(&mut events, |e| matches!(e, Event::PingReceived { .. })).await;
    match event {
        Event::PingReceived { device_id, message } => {
            assert_eq!(device_id, PHONE_ID);
            assert_eq!(message, "Hello from the phone");
        }
        other => panic!("unexpected event: {other:?}"),
    }

    // Desktop -> phone: the packet carries the message. The link is published
    // shortly after the identity exchange, so give the task a moment.
    tokio::time::sleep(Duration::from_millis(100)).await;
    app.send_ping(PHONE_ID, "Hello from the desktop")
        .await
        .expect("ping delivered");
    let packet = tokio::time::timeout(Duration::from_secs(5), read_packet(&mut reader))
        .await
        .expect("ping reaches the phone");
    assert_eq!(packet.packet_type, "kdeconnect.ping");
    assert_eq!(
        packet.body.get("message").and_then(|v| v.as_str()),
        Some("Hello from the desktop")
    );
}

#[tokio::test]
async fn pings_from_unpaired_devices_are_ignored() {
    let (app, port, mut events) = spawn_daemon().await;
    let mut reader = phone_handshake(port).await;

    wait_event(&mut events, |e| matches!(e, Event::Connected { .. })).await;
    send_packet(&mut reader, &phone_ping("Hello")).await;

    let result = tokio::time::timeout(Duration::from_secs(1), async {
        loop {
            match events.recv().await {
                Some(Event::PingReceived { .. }) => return,
                Some(_) => {}
                None => return,
            }
        }
    })
    .await;
    assert!(result.is_err(), "unpaired ping must be ignored");

    // The desktop cannot ping it either.
    assert_eq!(
        app.send_ping(PHONE_ID, "Hello").await,
        Err(PingError::NotPaired)
    );
}

#[tokio::test]
async fn disabled_ping_plugin_blocks_both_directions() {
    let (app, port, mut events) = spawn_daemon().await;
    let mut reader = phone_handshake(port).await;
    wait_event(&mut events, |e| matches!(e, Event::Connected { .. })).await;
    tokio::time::sleep(Duration::from_millis(100)).await;
    app.mark_paired(PHONE_ID).await;
    app.set_plugin_enabled(PHONE_ID, "kdeconnect.ping", false)
        .await
        .expect("ping plugin toggle");

    // Incoming pings are dropped.
    send_packet(&mut reader, &phone_ping("Hello")).await;
    let received = tokio::time::timeout(Duration::from_secs(1), async {
        loop {
            match events.recv().await {
                Some(Event::PingReceived { .. }) => return,
                Some(_) => {}
                None => return,
            }
        }
    })
    .await;
    assert!(
        received.is_err(),
        "disabled plugin must drop incoming pings"
    );

    // Outgoing pings are not sent.
    assert_eq!(
        app.send_ping(PHONE_ID, "Hello").await,
        Err(PingError::PluginDisabled)
    );
    let packet = tokio::time::timeout(Duration::from_secs(1), read_packet(&mut reader)).await;
    assert!(packet.is_err(), "disabled plugin must not send pings");
}

/// Simulated `kdeconnect.battery` packet.
fn battery_packet(level: i64, charging: bool) -> NetworkPacket {
    NetworkPacket::new(
        "kdeconnect.battery",
        serde_json::json!({ "currentCharge": level, "isCharging": charging }),
    )
}

fn incoming_clipboard(content: &str) -> NetworkPacket {
    NetworkPacket::new(
        "kdeconnect.clipboard",
        serde_json::json!({ "content": content }),
    )
}

fn incoming_clipboard_connect(content: &str, timestamp: i64) -> NetworkPacket {
    NetworkPacket::new(
        "kdeconnect.clipboard.connect",
        serde_json::json!({ "content": content, "timestamp": timestamp }),
    )
}

#[tokio::test]
async fn clipboard_flow_works() {
    let (app, port, mut events) = spawn_daemon().await;
    let mut reader = phone_handshake(port).await;
    wait_event(&mut events, |e| matches!(e, Event::Connected { .. })).await;
    app.mark_paired(PHONE_ID).await;

    send_packet(&mut reader, &incoming_clipboard("hola desde el móvil")).await;
    let event = wait_event(&mut events, |e| {
        matches!(e, Event::ClipboardReceived { .. })
    })
    .await;
    match event {
        Event::ClipboardReceived {
            device_id,
            content,
            timestamp,
        } => {
            assert_eq!(device_id, PHONE_ID);
            assert_eq!(content, "hola desde el móvil");
            assert_eq!(timestamp, 0);
        }
        other => panic!("unexpected event: {other:?}"),
    }
}

#[tokio::test]
async fn clipboard_connect_flow_works() {
    let (app, port, mut events) = spawn_daemon().await;
    let mut reader = phone_handshake(port).await;
    wait_event(&mut events, |e| matches!(e, Event::Connected { .. })).await;
    app.mark_paired(PHONE_ID).await;

    send_packet(
        &mut reader,
        &incoming_clipboard_connect("texto con timestamp", 1_700_000_000_000),
    )
    .await;
    let event = wait_event(&mut events, |e| {
        matches!(e, Event::ClipboardReceived { .. })
    })
    .await;
    match event {
        Event::ClipboardReceived {
            content, timestamp, ..
        } => {
            assert_eq!(content, "texto con timestamp");
            assert_eq!(timestamp, 1_700_000_000_000);
        }
        other => panic!("unexpected event: {other:?}"),
    }
}

#[tokio::test]
async fn clipboard_from_unpaired_devices_is_ignored() {
    let (_app, port, mut events) = spawn_daemon().await;
    let mut reader = phone_handshake(port).await;
    wait_event(&mut events, |e| matches!(e, Event::Connected { .. })).await;

    send_packet(&mut reader, &incoming_clipboard("no debería llegar")).await;
    let received = tokio::time::timeout(Duration::from_secs(1), async {
        loop {
            match events.recv().await {
                Some(Event::ClipboardReceived { .. }) => return,
                Some(_) => {}
                None => return,
            }
        }
    })
    .await;
    assert!(
        received.is_err(),
        "unpaired devices must not report clipboard changes"
    );
}

#[tokio::test]
async fn disabled_clipboard_plugin_ignores_updates() {
    let (app, port, mut events) = spawn_daemon().await;
    let mut reader = phone_handshake(port).await;
    wait_event(&mut events, |e| matches!(e, Event::Connected { .. })).await;
    app.mark_paired(PHONE_ID).await;
    app.set_plugin_enabled(PHONE_ID, "kdeconnect.clipboard", false)
        .await
        .expect("clipboard plugin toggle");

    send_packet(&mut reader, &incoming_clipboard("no debería llegar")).await;
    let received = tokio::time::timeout(Duration::from_secs(1), async {
        loop {
            match events.recv().await {
                Some(Event::ClipboardReceived { .. }) => return,
                Some(_) => {}
                None => return,
            }
        }
    })
    .await;
    assert!(
        received.is_err(),
        "disabled plugin must drop incoming clipboard updates"
    );
}

#[tokio::test]
async fn daemon_sends_clipboard_packets() {
    let (app, port, mut events) = spawn_daemon().await;
    let mut reader = phone_handshake(port).await;
    wait_event(&mut events, |e| matches!(e, Event::Connected { .. })).await;
    app.mark_paired(PHONE_ID).await;

    clipboard::send(&app, PHONE_ID, "desde el pc")
        .await
        .expect("clipboard sent");
    let packet = read_packet(&mut reader).await;
    assert_eq!(packet.packet_type, "kdeconnect.clipboard");
    assert_eq!(
        packet.body.get("content").and_then(|v| v.as_str()),
        Some("desde el pc")
    );

    clipboard::send_connect(&app, PHONE_ID, "con timestamp", 42)
        .await
        .expect("clipboard.connect sent");
    let packet = read_packet(&mut reader).await;
    assert_eq!(packet.packet_type, "kdeconnect.clipboard.connect");
    assert_eq!(
        packet.body.get("timestamp").and_then(|v| v.as_i64()),
        Some(42)
    );
}

#[tokio::test]
async fn battery_flow_works() {
    let (app, port, mut events) = spawn_daemon().await;
    let mut reader = phone_handshake(port).await;
    app.mark_paired(PHONE_ID).await;

    send_packet(&mut reader, &battery_packet(75, true)).await;
    let event = wait_event(&mut events, |e| matches!(e, Event::BatteryChanged { .. })).await;
    match event {
        Event::BatteryChanged {
            device_id,
            level,
            charging,
        } => {
            assert_eq!(device_id, PHONE_ID);
            assert_eq!(level, 75);
            assert!(charging);
        }
        other => panic!("unexpected event: {other:?}"),
    }
    assert_eq!(
        app.battery.get(PHONE_ID).await,
        BatteryStatus {
            level: 75,
            charging: true
        }
    );
}

#[tokio::test]
async fn battery_from_unpaired_devices_is_ignored() {
    let (app, port, mut events) = spawn_daemon().await;
    let mut reader = phone_handshake(port).await;
    wait_event(&mut events, |e| matches!(e, Event::Connected { .. })).await;

    send_packet(&mut reader, &battery_packet(50, false)).await;
    let received = tokio::time::timeout(Duration::from_secs(1), async {
        loop {
            match events.recv().await {
                Some(Event::BatteryChanged { .. }) => return,
                Some(_) => {}
                None => return,
            }
        }
    })
    .await;
    assert!(
        received.is_err(),
        "unpaired devices must not report battery"
    );
    assert_eq!(app.battery.get(PHONE_ID).await, BatteryStatus::UNKNOWN);
}

#[tokio::test]
async fn disabled_battery_plugin_ignores_updates() {
    let (app, port, mut events) = spawn_daemon().await;
    let mut reader = phone_handshake(port).await;
    app.mark_paired(PHONE_ID).await;
    app.set_plugin_enabled(PHONE_ID, "kdeconnect.battery", false)
        .await
        .expect("battery plugin toggle");

    send_packet(&mut reader, &battery_packet(50, false)).await;
    let received = tokio::time::timeout(Duration::from_secs(1), async {
        loop {
            match events.recv().await {
                Some(Event::BatteryChanged { .. }) => return,
                Some(_) => {}
                None => return,
            }
        }
    })
    .await;
    assert!(
        received.is_err(),
        "disabled plugin must drop incoming battery updates"
    );
    assert_eq!(app.battery.get(PHONE_ID).await, BatteryStatus::UNKNOWN);
}

#[tokio::test]
async fn disconnect_clears_battery_status() {
    let (app, port, mut events) = spawn_daemon().await;
    let mut reader = phone_handshake(port).await;
    wait_event(&mut events, |e| matches!(e, Event::Connected { .. })).await;
    app.mark_paired(PHONE_ID).await;

    send_packet(&mut reader, &battery_packet(75, true)).await;
    wait_event(&mut events, |e| matches!(e, Event::BatteryChanged { .. })).await;
    assert_eq!(
        app.battery.get(PHONE_ID).await,
        BatteryStatus {
            level: 75,
            charging: true
        }
    );

    drop(reader);
    wait_event(&mut events, |e| matches!(e, Event::Disconnected { .. })).await;
    assert_eq!(
        app.battery.get(PHONE_ID).await,
        BatteryStatus::UNKNOWN,
        "the last battery status must not survive a disconnect"
    );
}
