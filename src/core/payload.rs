//! TLS payload transfers used by the Share plugin.
//!
//! The JSON control channel only carries metadata. Binary payloads travel over
//! a separate **TLS** connection that reuses the device identity certificate,
//! exactly like KDE Connect: the sender listens on a payload port advertised in
//! `payloadTransferInfo`, the receiver connects to it with TLS and reads
//! exactly the announced number of bytes.

use std::io;
use std::net::IpAddr;
use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

use rustls_pki_types::ServerName;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use tokio_rustls::{TlsAcceptor, TlsConnector};

use crate::tls::IdentityCertificate;

/// How long to wait for the peer to connect / accept the payload socket.
const CONNECT_TIMEOUT: Duration = Duration::from_secs(15);
/// The TLS handshake may wait for a queued multi-file connection, so it is
/// more generous than a plain connect.
const HANDSHAKE_TIMEOUT: Duration = Duration::from_secs(120);
/// Inactivity timeout for a single read/write on the payload socket.
const IO_TIMEOUT: Duration = Duration::from_secs(120);
/// Chunk size used while streaming.
const CHUNK: usize = 64 * 1024;
/// Minimum number of bytes between progress reports.
const PROGRESS_STEP: u64 = 1024 * 1024;
/// Conventional KDE Connect payload port range (main TCP range is 1716..=1764).
const MIN_PAYLOAD_PORT: u16 = 1739;
const MAX_PAYLOAD_PORT: u16 = 1764;

/// Binds a payload TLS listener in the conventional payload port range,
/// falling back to an ephemeral port when every port is taken.
pub async fn bind_payload_listener() -> io::Result<TcpListener> {
    for port in MIN_PAYLOAD_PORT..=MAX_PAYLOAD_PORT {
        if let Ok(listener) = TcpListener::bind(("0.0.0.0", port)).await {
            return Ok(listener);
        }
    }
    TcpListener::bind(("0.0.0.0", 0)).await
}

/// Connects to `peer_ip:port` over TLS and copies exactly `expected` bytes into
/// `path`. `on_progress` is called with the number of bytes received so far, at
/// most every MiB and once at the end. Returns the bytes written.
pub async fn receive_payload<F>(
    cert: &IdentityCertificate,
    peer_ip: IpAddr,
    port: u16,
    expected: u64,
    path: &Path,
    mut on_progress: F,
) -> io::Result<u64>
where
    F: FnMut(u64),
{
    let connector = TlsConnector::from(Arc::new(crate::tls::client_config(cert)?));
    let tcp = tokio::time::timeout(CONNECT_TIMEOUT, TcpStream::connect((peer_ip, port)))
        .await
        .map_err(|_| io::Error::new(io::ErrorKind::TimedOut, "payload connect timed out"))??;
    let server_name = ServerName::try_from("kdeconnect")
        .map_err(|_| io::Error::new(io::ErrorKind::InvalidInput, "bad TLS server name"))?
        .to_owned();
    let mut stream = tokio::time::timeout(HANDSHAKE_TIMEOUT, connector.connect(server_name, tcp))
        .await
        .map_err(|_| io::Error::new(io::ErrorKind::TimedOut, "payload TLS handshake timed out"))?
        .map_err(io::Error::other)?;

    let mut file = tokio::fs::File::create(path).await?;
    let mut buf = vec![0u8; CHUNK];
    let mut total = 0u64;
    let mut last_reported = 0u64;

    while total < expected {
        let want = usize::try_from((expected - total).min(CHUNK as u64)).unwrap_or(CHUNK);
        let n = tokio::time::timeout(IO_TIMEOUT, stream.read(&mut buf[..want]))
            .await
            .map_err(|_| io::Error::new(io::ErrorKind::TimedOut, "payload read timed out"))??;
        if n == 0 {
            return Err(io::Error::new(
                io::ErrorKind::UnexpectedEof,
                "payload ended before the announced size",
            ));
        }
        file.write_all(&buf[..n]).await?;
        total += n as u64;
        if total - last_reported >= PROGRESS_STEP || total == expected {
            on_progress(total);
            last_reported = total;
        }
    }

    file.flush().await?;
    Ok(total)
}

/// Accepts one TLS payload connection on `listener` and streams `path` to it.
pub async fn send_payload(
    cert: &IdentityCertificate,
    listener: TcpListener,
    path: &Path,
    size: u64,
) -> io::Result<()> {
    let acceptor = TlsAcceptor::from(Arc::new(crate::tls::server_config(cert)?));
    let (tcp, _peer) = tokio::time::timeout(CONNECT_TIMEOUT, listener.accept())
        .await
        .map_err(|_| io::Error::new(io::ErrorKind::TimedOut, "payload accept timed out"))??;
    let mut stream = tokio::time::timeout(HANDSHAKE_TIMEOUT, acceptor.accept(tcp))
        .await
        .map_err(|_| io::Error::new(io::ErrorKind::TimedOut, "payload TLS handshake timed out"))?
        .map_err(io::Error::other)?;

    let mut file = tokio::fs::File::open(path).await?;
    let sent = tokio::time::timeout(IO_TIMEOUT, tokio::io::copy(&mut file, &mut stream))
        .await
        .map_err(|_| io::Error::new(io::ErrorKind::TimedOut, "payload send timed out"))??;
    stream.flush().await?;
    if sent != size {
        tracing::warn!(expected = size, sent, "payload size mismatch");
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn transfers_bytes_over_a_local_tls_socket() {
        let dir = tempfile::tempdir().expect("tempdir");
        let cert = Arc::new(
            IdentityCertificate::load_or_create(dir.path().to_owned())
                .await
                .expect("certificate"),
        );
        let source = dir.path().join("source.bin");
        let payload = b"hello payload".to_vec();
        std::fs::write(&source, &payload).expect("write source");

        let listener = bind_payload_listener().await.expect("bind");
        let port = listener.local_addr().expect("addr").port();

        let send_task = tokio::spawn({
            let source = source.clone();
            let cert = cert.clone();
            let size = payload.len() as u64;
            async move { send_payload(&cert, listener, &source, size).await }
        });

        let dest = dir.path().join("dest.bin");
        let mut progress = Vec::new();
        let received = receive_payload(
            &cert,
            std::net::IpAddr::V4(std::net::Ipv4Addr::LOCALHOST),
            port,
            payload.len() as u64,
            &dest,
            |n| progress.push(n),
        )
        .await
        .expect("receive");

        assert_eq!(received, payload.len() as u64);
        assert_eq!(std::fs::read(&dest).expect("read dest"), payload);
        assert_eq!(*progress.last().expect("progress"), payload.len() as u64);
        send_task.await.expect("send task").expect("send");
    }
}
