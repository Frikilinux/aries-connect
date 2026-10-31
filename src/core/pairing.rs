//! Pairing handshake logic (`kdeconnect.pair` packets).
//!
//! Incoming `pair:true` requests are surfaced to the user as a D-Bus signal
//! and resolved via the D-Bus methods `AcceptPairing`/`RejectPairing`.
//! Unanswered requests expire after `PAIR_TIMEOUT` and auto-reject.
//!
//! Replies are sent through the device's *current* control channel: Android
//! replaces its socket every time it receives a discovery broadcast, so the
//! socket a request arrived on may already be gone when the user decides.

use std::sync::Arc;

use tokio::time;

use crate::app::{App, Event, PairDecision};
use crate::identity::pair_packet;

/// Window the user has to decide on a pairing request.
pub const PAIR_TIMEOUT: tokio::time::Duration = time::Duration::from_secs(30);

/// Handles an incoming `kdeconnect.pair` packet from an identified peer.
pub async fn handle_pair(app: Arc<App>, device_id: Option<String>, accept_requested: bool) {
    let Some(device_id) = device_id else {
        tracing::warn!("pair request from a device that has not identified itself yet");
        return;
    };

    // The peer asked to *unpair*; nothing to negotiate.
    if !accept_requested {
        app.unpair(&device_id).await;
        let _ = app.events.send(Event::PairingRejected {
            id: device_id,
            reason: "peer unpaired".to_owned(),
        });
        return;
    }

    tracing::info!(device = %device_id, "pairing requested by peer");

    // Wait for the user's decision (via D-Bus) or the timeout.
    let rx = app.register_pair_request(device_id.clone()).await;
    let decision = match time::timeout(PAIR_TIMEOUT, rx).await {
        Ok(Ok(d)) => d,
        Ok(Err(_)) => {
            tracing::info!(device = %device_id, "pair request cancelled");
            PairDecision::Reject
        }
        Err(_elapsed) => {
            tracing::info!(device = %device_id, "pair request timed out");
            PairDecision::Reject
        }
    };

    let accept = decision == PairDecision::Accept;
    // Reply through the newest link for this device; the original socket may
    // have been replaced while the user was deciding.
    if !app.send_packet(&device_id, &pair_packet(accept)).await {
        tracing::warn!(device = %device_id, "no active control channel to send the pair reply");
    }

    match decision {
        PairDecision::Accept => {
            app.mark_paired(&device_id).await;
            tracing::info!(device = %device_id, "pairing accepted");
            let _ = app.events.send(Event::PairingAccepted { id: device_id });
        }
        PairDecision::Reject => {
            tracing::info!(device = %device_id, "pairing rejected");
            let _ = app.events.send(Event::PairingRejected {
                id: device_id,
                reason: "rejected or timed out".to_owned(),
            });
        }
    }
}
