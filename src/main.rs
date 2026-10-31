//! AriesConnect: a KDE Connect-compatible daemon without KDE/Qt dependencies.
//!
//! Starts the identity/TLS state, UDP discovery, the TCP control channel and
//! the D-Bus service (`dev.zotta.AriesConnect`).

use std::io::Write;
use std::path::Path;

use tokio::sync::mpsc;

use aries_connect::app::App;
use aries_connect::core::network::tls_connector;
use aries_connect::identity::{is_valid_device_id, LocalIdentity};

/// Listening port for both UDP and TCP (KDE Connect convention).
const DEFAULT_PORT: u16 = 1716;

fn data_dir() -> std::path::PathBuf {
    if let Ok(dir) = std::env::var("AC_DATA_DIR") {
        return dir.into();
    }
    let home = std::env::var("HOME").unwrap_or_else(|_| "~".to_owned());
    let xdg = std::env::var("XDG_DATA_HOME")
        .ok()
        .filter(|s| !s.is_empty());
    let base: std::path::PathBuf = xdg
        .map(Into::into)
        .unwrap_or_else(|| format!("{home}/.local/share").into());
    base.join("aries-connect")
}

fn port() -> u16 {
    std::env::var("AC_PORT")
        .ok()
        .and_then(|v| v.trim().parse().ok())
        .unwrap_or(DEFAULT_PORT)
}

/// Host name used as the device display name.
fn hostname() -> String {
    std::env::var("HOSTNAME")
        .ok()
        .filter(|h| !h.is_empty())
        .or_else(|| std::fs::read_to_string("/etc/hostname").ok())
        .map(|h| h.trim().to_owned())
        .filter(|h| !h.is_empty())
        .unwrap_or_else(|| "aries-connect".to_owned())
}

/// Loads the persisted device id or generates a random one.
///
/// Android rejects identities whose device id does not match
/// `^[a-zA-Z0-9_-]{32,38}$`, so a hostname cannot be used.
fn load_or_create_device_id(dir: &Path) -> String {
    let path = dir.join("device_id");
    if let Ok(existing) = std::fs::read_to_string(&path) {
        let id = existing.trim();
        if is_valid_device_id(id) {
            return id.to_owned();
        }
    }

    let id: String = (0..32)
        .map(|_| {
            const HEX: &[u8; 16] = b"0123456789abcdef";
            HEX[rand::random::<usize>() % 16] as char
        })
        .collect();
    if let Err(e) = std::fs::write(&path, &id) {
        tracing::warn!(error = %e, "cannot persist device id; using an ephemeral one");
    }
    id
}

fn init_logging() {
    let filter = tracing_subscriber::EnvFilter::try_from_default_env()
        .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info"));
    let _ = tracing_subscriber::fmt()
        .with_env_filter(filter)
        .with_writer(std::io::stderr)
        .try_init();
}

#[tokio::main]
async fn main() {
    init_logging();

    let port = port();
    let dir = data_dir();
    tracing::info!(
        hostname = hostname(),
        port,
        data_dir = %dir.display(),
        "starting aries-connect"
    );

    // Certificate (persisted, stable fingerprint across restarts).
    let cert = match aries_connect::tls::IdentityCertificate::load_or_create(dir.clone()).await {
        Ok(c) => c,
        Err(e) => {
            tracing::error!(error = %e, "cannot load or create TLS certificate");
            std::process::exit(1);
        }
    };
    let connector = match tls_connector(&cert) {
        Ok(c) => c,
        Err(e) => {
            tracing::error!(error = %e, "invalid TLS setup");
            std::process::exit(1);
        }
    };

    // Persistent KDE Connect device id (Android requires 32-38 chars).
    let device_id = load_or_create_device_id(&dir);
    tracing::info!(device_id = %device_id, "device identity");

    // Shared state + event channel toward D-Bus.
    let (events_tx, events_rx) = mpsc::unbounded_channel();
    let trusted = aries_connect::trusted::TrustedStore::load(&dir);
    let app = App::new(
        LocalIdentity::new(device_id, hostname(), port),
        events_tx,
        trusted,
    );
    app.seed_trusted().await;

    // D-Bus service + command loop.
    let cmd_rx = match aries_connect::dbus::spawn(app.clone(), events_rx).await {
        Ok(v) => v,
        Err(e) => {
            tracing::error!(error = %e, "cannot register the D-Bus service");
            std::process::exit(1);
        }
    };
    tracing::info!(
        service = aries_connect::dbus::DBUS_NAME,
        path = aries_connect::dbus::DBUS_PATH,
        "D-Bus service registered"
    );
    tokio::spawn(aries_connect::app::command_loop(app.clone(), cmd_rx));

    // UDP discovery.
    if let Err(e) = aries_connect::core::discovery::spawn(
        app.clone(),
        aries_connect::core::discovery::DiscoveryConfig { port },
    )
    .await
    {
        tracing::error!(error = %e, "cannot start UDP discovery");
        std::process::exit(1);
    }
    // TCP + TLS control channel.
    if let Err(e) = aries_connect::core::network::spawn(app.clone(), connector, port).await {
        tracing::error!(error = %e, "cannot start the TCP control channel");
        std::process::exit(1);
    }

    tracing::info!(
        "AriesConnect ready; test with `busctl --user introspect {svc} {path}`",
        svc = aries_connect::dbus::DBUS_NAME,
        path = aries_connect::dbus::DBUS_PATH
    );
    let _ = std::io::stderr().flush();

    // The daemon runs until interrupted.
    if let Err(e) = tokio::signal::ctrl_c().await {
        tracing::error!(error = %e, "failed to install SIGINT handler");
    }
    tracing::info!("shutting down");
}
