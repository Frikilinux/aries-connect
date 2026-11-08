//! Share (`kdeconnect.share`).
//!
//! Shares files, text and URLs in both directions. Text and URLs travel in the
//! packet body; files travel over a separate TCP payload channel announced via
//! `payloadTransferInfo`. The daemon has no UI: it stores incoming files and
//! surfaces everything over D-Bus, leaving opening/displaying to the frontend.
//!
//! Only paired devices are accepted, and only while the
//! `kdeconnect.share.request` plugin is enabled for that device.

use std::net::IpAddr;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use serde::Deserialize;

use crate::app::{App, Event};
use crate::identity::NetworkPacket;

/// Packet type used for share requests in both directions.
pub const SHARE_PACKET: &str = "kdeconnect.share.request";
/// Batch metadata (number/size of files) announced before a multi-file share.
pub const SHARE_UPDATE_PACKET: &str = "kdeconnect.share.request.update";

#[derive(Debug, Default, Deserialize)]
#[serde(default)]
struct ShareBody {
    filename: Option<String>,
    text: Option<String>,
    url: Option<String>,
    #[serde(rename = "open")]
    open: bool,
    #[serde(rename = "numberOfFiles")]
    number_of_files: Option<u32>,
    #[serde(rename = "totalPayloadSize")]
    total_payload_size: Option<u64>,
}

impl ShareBody {
    fn number_of_files_or_one(&self) -> u32 {
        self.number_of_files.unwrap_or(1).max(1)
    }
}

/// Handles share packets. Returns `true` when the packet was consumed.
pub async fn handle_packet(
    app: &Arc<App>,
    device_id: Option<&str>,
    peer_ip: Option<IpAddr>,
    packet: &NetworkPacket,
) -> bool {
    if packet.packet_type == SHARE_UPDATE_PACKET {
        handle_update(app, device_id, packet).await;
        return true;
    }
    if packet.packet_type != SHARE_PACKET {
        return false;
    }
    let Some(device_id) = device_id else {
        return true;
    };
    if !authorized(app, device_id).await {
        return true;
    }
    let body: ShareBody = match serde_json::from_value(packet.body.clone()) {
        Ok(body) => body,
        Err(e) => {
            tracing::debug!(device = %device_id, error = %e, "malformed share packet");
            return true;
        }
    };

    if let Some(text) = body.text.as_deref() {
        if !text.is_empty() {
            tracing::debug!(device = %device_id, "share text received");
            let _ = app.events.send(Event::ShareTextReceived {
                device_id: device_id.to_owned(),
                text: text.to_owned(),
            });
        }
        return true;
    }
    if let Some(url) = body.url.as_deref() {
        if !url.is_empty() {
            tracing::debug!(device = %device_id, "share url received");
            let _ = app.events.send(Event::ShareUrlReceived {
                device_id: device_id.to_owned(),
                url: url.to_owned(),
            });
        }
        return true;
    }
    if let Some(filename) = body.filename.as_deref() {
        match peer_ip {
            Some(peer_ip) => {
                handle_incoming_file(app, device_id, peer_ip, filename, packet, &body).await;
            }
            None => tracing::debug!(device = %device_id, "share file without a peer address"),
        }
    }
    true
}

/// Consumes batch metadata and primes the per-device batch totals.
async fn handle_update(app: &Arc<App>, device_id: Option<&str>, packet: &NetworkPacket) {
    let Some(device_id) = device_id else {
        return;
    };
    if !authorized(app, device_id).await {
        return;
    }
    let body: ShareBody = serde_json::from_value(packet.body.clone()).unwrap_or_default();
    let total_files = body.number_of_files.unwrap_or(0);
    let total_bytes = body.total_payload_size.unwrap_or(0);
    if total_files > 0 || total_bytes > 0 {
        app.share
            .ensure_batch(device_id, total_files, total_bytes)
            .await;
    }
}

async fn authorized(app: &Arc<App>, device_id: &str) -> bool {
    if app.trusted.get(device_id).await.is_none() {
        tracing::debug!(device = %device_id, "ignoring share request from an unpaired device");
        return false;
    }
    if !app.trusted.plugin_enabled(device_id, SHARE_PACKET).await {
        tracing::debug!(device = %device_id, "share plugin is disabled for this device");
        return false;
    }
    true
}

async fn handle_incoming_file(
    app: &Arc<App>,
    device_id: &str,
    peer_ip: IpAddr,
    filename: &str,
    packet: &NetworkPacket,
    body: &ShareBody,
) {
    let Some(size) = packet.payload_size else {
        tracing::debug!(device = %device_id, "share request without a payload size");
        return;
    };
    let Some(port) = packet
        .payload_transfer_info
        .as_ref()
        .and_then(|info| info.port)
    else {
        tracing::debug!(device = %device_id, "share request without a payload port");
        return;
    };
    let max = app.effective_max_size(device_id).await;
    if size > max {
        tracing::info!(device = %device_id, size, max, "refusing oversized shared file");
        return;
    }
    let Some(name) = sanitize_filename(filename) else {
        tracing::warn!(device = %device_id, filename, "refusing unsafe shared filename");
        return;
    };
    let dir = match app.effective_download_dir(device_id).await {
        Ok(dir) => dir,
        Err(e) => {
            tracing::warn!(device = %device_id, error = %e, "cannot create download directory");
            return;
        }
    };
    let Some(cert) = app.certificate().await else {
        tracing::warn!(device = %device_id, "no TLS identity configured for payload transfers");
        return;
    };
    let path = unique_path(&dir, &name).await;
    let total_files = body.number_of_files_or_one();
    let total_bytes = body.total_payload_size.unwrap_or(size).max(size);
    let batch = app
        .share
        .ensure_batch(device_id, total_files, total_bytes)
        .await;
    let _ = app.events.send(Event::ShareProgress {
        device_id: device_id.to_owned(),
        bytes: batch.bytes_done,
        total_bytes: batch.total_bytes,
        files: batch.files_done,
        total_files: batch.total_files,
    });
    tracing::info!(
        device = %device_id,
        filename = %name,
        size,
        open = body.open,
        "receiving shared file"
    );

    let app = app.clone();
    let device_id = device_id.to_owned();
    tokio::spawn(async move {
        let events = app.events.clone();
        let progress_device = device_id.clone();
        let base_bytes = batch.bytes_done;
        let files = batch.files_done;
        let total_bytes = batch.total_bytes;
        let total_files = batch.total_files;
        let result =
            crate::core::payload::receive_payload(&cert, peer_ip, port, size, &path, move |n| {
                let _ = events.send(Event::ShareProgress {
                    device_id: progress_device.clone(),
                    bytes: base_bytes + n,
                    total_bytes,
                    files,
                    total_files,
                });
            })
            .await;

        match result {
            Ok(written) => {
                let state = app.share.file_done(&device_id, written).await;
                let _ = app.events.send(Event::ShareProgress {
                    device_id: device_id.clone(),
                    bytes: state.bytes_done,
                    total_bytes: state.total_bytes,
                    files: state.files_done,
                    total_files: state.total_files,
                });
                let _ = app.events.send(Event::ShareFileReceived {
                    device_id,
                    path: path.to_string_lossy().into_owned(),
                });
            }
            Err(e) => {
                tracing::warn!(device = %device_id, error = %e, "incoming share failed");
                let _ = tokio::fs::remove_file(&path).await;
            }
        }
    });
}

/// Reduces a peer-supplied name to a safe single path component.
fn sanitize_filename(name: &str) -> Option<String> {
    let base = name.rsplit(['/', '\\']).next().unwrap_or("").trim();
    if base.is_empty() || base == "." || base == ".." {
        return None;
    }
    let cleaned: String = base
        .chars()
        .filter(|c| !c.is_control() && *c != '\0')
        .collect();
    if cleaned.is_empty() {
        None
    } else {
        Some(cleaned)
    }
}

/// Picks a non-existing path in `dir`, appending ` (n)` before the extension.
async fn unique_path(dir: &Path, name: &str) -> PathBuf {
    let direct = dir.join(name);
    if tokio::fs::metadata(&direct).await.is_err() {
        return direct;
    }
    let path = Path::new(name);
    let stem = path.file_stem().and_then(|s| s.to_str()).unwrap_or("file");
    let extension = path.extension().and_then(|s| s.to_str()).map(str::to_owned);
    for i in 1..1000 {
        let candidate = match &extension {
            Some(ext) => dir.join(format!("{stem} ({i}).{ext}")),
            None => dir.join(format!("{stem} ({i})")),
        };
        if tokio::fs::metadata(&candidate).await.is_err() {
            return candidate;
        }
    }
    let fallback = format!("{stem}-{}", std::process::id());
    match extension {
        Some(ext) => dir.join(format!("{fallback}.{ext}")),
        None => dir.join(fallback),
    }
}

/// Builds a `kdeconnect.share.request` carrying text.
pub fn text_packet(text: &str) -> NetworkPacket {
    NetworkPacket::new(SHARE_PACKET, serde_json::json!({ "text": text }))
}

/// Builds a `kdeconnect.share.request` carrying a URL.
pub fn url_packet(url: &str) -> NetworkPacket {
    NetworkPacket::new(SHARE_PACKET, serde_json::json!({ "url": url }))
}

/// Builds a `kdeconnect.share.request` for a file payload served on `port`.
pub fn file_packet(
    filename: &str,
    size: u64,
    port: u16,
    last_modified: Option<i64>,
) -> NetworkPacket {
    let mut body = serde_json::json!({
        "filename": filename,
        "numberOfFiles": 1,
        "totalPayloadSize": size,
    });
    if let (Some(ts), Some(object)) = (last_modified, body.as_object_mut()) {
        object.insert("lastModified".to_owned(), serde_json::json!(ts));
    }
    NetworkPacket::new(SHARE_PACKET, body).with_payload(size, port)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;

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
                download_dir: None,
                max_incoming_size: None,
                plugins,
            })
            .await;
        (app, events_rx)
    }

    #[tokio::test]
    async fn emits_event_for_shared_text() {
        let (app, mut events) = paired_app(HashMap::new()).await;
        let packet = text_packet("hello");

        assert!(handle_packet(&app, Some(DEVICE_ID), None, &packet).await);
        match events.try_recv().expect("share event") {
            Event::ShareTextReceived { device_id, text } => {
                assert_eq!(device_id, DEVICE_ID);
                assert_eq!(text, "hello");
            }
            other => panic!("unexpected event: {other:?}"),
        }
    }

    #[tokio::test]
    async fn emits_event_for_shared_url() {
        let (app, mut events) = paired_app(HashMap::new()).await;
        let packet = url_packet("https://kdeconnect.kde.org");

        assert!(handle_packet(&app, Some(DEVICE_ID), None, &packet).await);
        match events.try_recv().expect("share event") {
            Event::ShareUrlReceived { url, .. } => {
                assert_eq!(url, "https://kdeconnect.kde.org")
            }
            other => panic!("unexpected event: {other:?}"),
        }
    }

    #[tokio::test]
    async fn ignores_share_when_plugin_disabled() {
        let plugins = HashMap::from([(SHARE_PACKET.to_owned(), false)]);
        let (app, mut events) = paired_app(plugins).await;

        assert!(handle_packet(&app, Some(DEVICE_ID), None, &text_packet("hi")).await);
        assert!(events.try_recv().is_err());
    }

    #[tokio::test]
    async fn ignores_share_from_unpaired_device() {
        let local = LocalIdentity::new("a".repeat(32), "Desktop".to_owned(), 1716);
        let (events, mut events_rx) = mpsc::unbounded_channel();
        let app = App::new(local, events, TrustedStore::in_memory());

        assert!(handle_packet(&app, Some(DEVICE_ID), None, &text_packet("hi")).await);
        assert!(events_rx.try_recv().is_err());
    }

    #[test]
    fn sanitizes_path_traversal() {
        assert_eq!(
            sanitize_filename("../../etc/passwd").as_deref(),
            Some("passwd")
        );
        assert_eq!(
            sanitize_filename("..\\..\\evil.txt").as_deref(),
            Some("evil.txt")
        );
        assert_eq!(sanitize_filename(".."), None);
        assert_eq!(sanitize_filename("."), None);
        assert_eq!(sanitize_filename(""), None);
        assert_eq!(sanitize_filename("ok.txt").as_deref(), Some("ok.txt"));
    }

    #[test]
    fn file_packet_carries_size_and_port() {
        let packet = file_packet("a.png", 1234, 1739, Some(42));
        assert_eq!(packet.packet_type, SHARE_PACKET);
        assert_eq!(packet.payload_size, Some(1234));
        assert_eq!(
            packet.payload_transfer_info.and_then(|info| info.port),
            Some(1739)
        );
        assert_eq!(
            packet.body.get("filename").and_then(|v| v.as_str()),
            Some("a.png")
        );
        assert_eq!(
            packet.body.get("totalPayloadSize"),
            Some(&serde_json::json!(1234))
        );
    }

    #[tokio::test]
    async fn ignores_file_without_payload() {
        let (app, mut events) = paired_app(HashMap::new()).await;
        let packet = NetworkPacket::new(
            SHARE_PACKET,
            serde_json::json!({ "filename": "a.png", "numberOfFiles": 1 }),
        );

        assert!(
            handle_packet(
                &app,
                Some(DEVICE_ID),
                Some(std::net::IpAddr::V4(std::net::Ipv4Addr::LOCALHOST)),
                &packet
            )
            .await
        );
        assert!(events.try_recv().is_err());
    }
}
