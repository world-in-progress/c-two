//! HTTP handlers for `/_peer/*` mesh gossip endpoints.

use std::sync::Arc;

use axum::{extract::State, http::StatusCode, response::IntoResponse, Json};

use crate::relay::peer::{PeerEnvelope, PeerMessage, PROTOCOL_VERSION};
use crate::relay::state::RelayState;
use crate::relay::types::*;

fn check_protocol_version(envelope: &PeerEnvelope) -> bool {
    if envelope.protocol_version > PROTOCOL_VERSION {
        eprintln!(
            "[relay] Ignoring message from {} with protocol_version {}",
            envelope.sender_relay_id, envelope.protocol_version
        );
        return false;
    }
    true
}

/// POST /_peer/announce — receive RouteAnnounce or RouteWithdraw
pub async fn handle_peer_announce(
    State(state): State<Arc<RelayState>>,
    Json(envelope): Json<PeerEnvelope>,
) -> impl IntoResponse {
    if !check_protocol_version(&envelope) {
        return StatusCode::OK;
    }
    match envelope.message {
        PeerMessage::RouteAnnounce {
            name, relay_id, relay_url, ipc_address: _,
            crm_ns, crm_ver, registered_at,
        } => {
            if relay_id == state.relay_id() {
                return StatusCode::OK;
            }
            // Defense-in-depth: ipc_address is a private local path on the
            // sending relay; never store it in our PEER routes regardless of
            // what the wire said.
            state.register_peer_route(RouteEntry {
                name, relay_id, relay_url,
                ipc_address: None,
                crm_ns, crm_ver,
                locality: Locality::Peer,
                registered_at,
            });
        }
        PeerMessage::RouteWithdraw { name, relay_id } => {
            state.unregister_peer_route(&name, &relay_id);
        }
        _ => {
            if matches!(envelope.message, PeerMessage::Unknown) {
                eprintln!("[relay] Ignoring unknown message type from {}", envelope.sender_relay_id);
            }
        }
    }
    StatusCode::OK
}

/// POST /_peer/join — a new relay wants to join
pub async fn handle_peer_join(
    State(state): State<Arc<RelayState>>,
    Json(envelope): Json<PeerEnvelope>,
) -> impl IntoResponse {
    if !check_protocol_version(&envelope) {
        return (StatusCode::OK, Json(serde_json::json!({"status": "ignored"}))).into_response();
    }
    if let PeerMessage::RelayJoin { relay_id, url } = envelope.message {
        if relay_id == state.relay_id() {
            return (StatusCode::OK, Json(serde_json::json!({"status": "self"}))).into_response();
        }
        state.register_peer(PeerInfo {
            relay_id: relay_id.clone(),
            url,
            route_count: 0,
            last_heartbeat: std::time::Instant::now(),
            status: PeerStatus::Alive,
        });
        let mut snapshot = state.full_snapshot();
        // Strip ipc_address from every route before sending — these paths
        // are local to this relay's filesystem and cannot be reached by the
        // joiner directly; they must use relay_url instead.
        for entry in snapshot.routes.iter_mut() {
            entry.ipc_address = None;
        }
        // Include ourselves so the joiner can add us as a peer.
        snapshot.peers.push(PeerSnapshot {
            relay_id: state.relay_id().to_string(),
            url: state.config().effective_advertise_url(),
            route_count: state.local_route_count(),
            status: PeerStatus::Alive,
        });
        Json(snapshot).into_response()
    } else {
        StatusCode::BAD_REQUEST.into_response()
    }
}

/// GET /_peer/sync — return full route table + peer list
pub async fn handle_peer_sync(
    State(state): State<Arc<RelayState>>,
) -> impl IntoResponse {
    Json(state.full_snapshot())
}

/// POST /_peer/heartbeat — periodic liveness check
pub async fn handle_peer_heartbeat(
    State(state): State<Arc<RelayState>>,
    Json(envelope): Json<PeerEnvelope>,
) -> impl IntoResponse {
    if !check_protocol_version(&envelope) {
        return StatusCode::OK;
    }
    if let PeerMessage::Heartbeat { relay_id, route_count } = envelope.message {
        state.with_route_table_mut(|rt| {
            if let Some(peer) = rt.get_peer_mut(&relay_id) {
                peer.last_heartbeat = std::time::Instant::now();
                peer.route_count = route_count;
                if peer.status == PeerStatus::Dead || peer.status == PeerStatus::Suspect {
                    peer.status = PeerStatus::Alive;
                }
            }
        });
    }
    StatusCode::OK
}

/// POST /_peer/leave — graceful departure
pub async fn handle_peer_leave(
    State(state): State<Arc<RelayState>>,
    Json(envelope): Json<PeerEnvelope>,
) -> impl IntoResponse {
    if !check_protocol_version(&envelope) {
        return StatusCode::OK;
    }
    if let PeerMessage::RelayLeave { relay_id } = envelope.message {
        if relay_id == state.relay_id() {
            return StatusCode::OK;
        }
        state.remove_routes_by_relay(&relay_id);
        state.unregister_peer(&relay_id);
    }
    StatusCode::OK
}

/// POST /_peer/digest — anti-entropy digest exchange
pub async fn handle_peer_digest(
    State(state): State<Arc<RelayState>>,
    Json(envelope): Json<PeerEnvelope>,
) -> impl IntoResponse {
    if !check_protocol_version(&envelope) {
        return StatusCode::OK.into_response();
    }
    match envelope.message {
        PeerMessage::DigestExchange { digest } => {
            let our_digest = state.route_digest();

            let peer_map: std::collections::HashMap<(String, String), u64> = digest
                .iter()
                .map(|d| ((d.name.clone(), d.relay_id.clone()), d.hash))
                .collect();

            // Collect all our routes once (O(1) call, not O(n)).
            let all_routes = state.with_route_table(|rt| rt.list_routes());

            let mut diff_entries = Vec::new();
            for (key, our_hash) in &our_digest {
                match peer_map.get(key) {
                    Some(peer_hash) if peer_hash == our_hash => {}
                    _ => {
                        // We have this route and peer doesn't, or hashes differ.
                        if let Some(entry) = all_routes.iter()
                            .find(|e| e.name == key.0 && e.relay_id == key.1)
                        {
                            // Strip ipc_address — it's a local-only path on
                            // this relay's filesystem.
                            diff_entries.push(crate::relay::peer::DigestDiffEntry {
                                name: entry.name.clone(),
                                relay_id: entry.relay_id.clone(),
                                relay_url: entry.relay_url.clone(),
                                ipc_address: None,
                                crm_ns: entry.crm_ns.clone(),
                                crm_ver: entry.crm_ver.clone(),
                                registered_at: entry.registered_at,
                            });
                        }
                    }
                }
            }

            // Anti-entropy is additive only — never tell peers to delete
            // routes. Deletions propagate via explicit RouteWithdraw gossip.
            let response = PeerEnvelope::new(
                state.relay_id(),
                PeerMessage::DigestDiff {
                    entries: diff_entries,
                    extra: Vec::new(),
                },
            );
            Json(response).into_response()
        }
        PeerMessage::DigestDiff { entries, extra } => {
            for diff in entries {
                // Defense-in-depth: scrub ipc_address from incoming peer
                // routes — that path belongs to the sender's local filesystem.
                let mut entry: RouteEntry = diff.into();
                entry.ipc_address = None;
                state.register_peer_route(entry);
            }
            for (name, relay_id) in extra {
                state.unregister_peer_route(&name, &relay_id);
            }
            StatusCode::OK.into_response()
        }
        _ => StatusCode::BAD_REQUEST.into_response(),
    }
}