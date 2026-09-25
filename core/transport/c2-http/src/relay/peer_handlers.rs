//! HTTP handlers for `/_peer/*` mesh gossip endpoints.

use std::sync::Arc;

use axum::{Json, extract::State, http::StatusCode, response::IntoResponse};

use crate::relay::authority::{RouteAuthority, RouteCommand};
use crate::relay::peer::{
    DigestDiffEntry, PROTOCOL_VERSION, PeerEnvelope, PeerMessage, ValidatedDigestDiffEntry,
    validate_route_state_envelope,
};
use crate::relay::state::RelayState;
use crate::relay::types::*;

fn current_full_sync_envelope(state: &RelayState) -> FullSyncEnvelope {
    let mut snapshot = state.full_snapshot();
    snapshot
        .tombstones
        .retain(|tombstone| !tombstone.relay_id.is_empty());
    if !snapshot
        .peers
        .iter()
        .any(|peer| peer.relay_id == state.relay_id())
    {
        snapshot.peers.push(PeerSnapshot {
            relay_id: state.relay_id().to_string(),
            url: state.config().effective_advertise_url(),
            route_count: state.local_route_count(),
            status: PeerStatus::Alive,
        });
    }
    FullSyncEnvelope {
        protocol_version: PROTOCOL_VERSION,
        snapshot: FullSyncSnapshot::from_internal(snapshot),
    }
}

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

fn trusted_peer(state: &RelayState, sender_relay_id: &str) -> bool {
    sender_relay_id != state.relay_id() && state.peer_is_alive(sender_relay_id)
}

fn known_peer(state: &RelayState, sender_relay_id: &str) -> bool {
    sender_relay_id != state.relay_id() && state.has_peer(sender_relay_id)
}

fn valid_peer_relay_id(relay_id: &str) -> bool {
    c2_config::validate_relay_id(relay_id).is_ok()
}

fn valid_peer_relay_url(url: &str) -> bool {
    crate::relay::route_table::valid_relay_url(url)
}

/// POST /_peer/announce — receive RouteAnnounce or RouteWithdraw
pub async fn handle_peer_announce(
    State(state): State<Arc<RelayState>>,
    Json(envelope): Json<PeerEnvelope>,
) -> impl IntoResponse {
    if !check_protocol_version(&envelope) {
        return StatusCode::OK;
    }
    let envelope = match validate_route_state_envelope(envelope, None) {
        Ok(validated) => validated.into_envelope(),
        Err(_) => return StatusCode::OK,
    };
    let sender_relay_id = envelope.sender_relay_id.clone();
    match envelope.message {
        PeerMessage::RouteAnnounce {
            name,
            relay_id,
            relay_url,
            crm_ns,
            crm_name,
            crm_ver,
            abi_hash,
            signature_hash,
            max_payload_size,
            route_uid,
            route_revision,
            registered_at,
        } => {
            // Peer wire data never carries owner-private fields; keep peer
            // route storage scrubbed as a second local invariant.
            let _ = RouteAuthority::new(&state).execute(RouteCommand::AnnouncePeer {
                sender_relay_id,
                entry: Box::new(RouteEntry {
                    name,
                    relay_id,
                    relay_url,
                    server_id: None,
                    server_instance_id: None,
                    ipc_address: None,
                    crm_ns,
                    crm_name,
                    crm_ver,
                    abi_hash,
                    signature_hash,
                    max_payload_size,
                    route_uid,
                    route_revision,
                    locality: Locality::Peer,
                    registered_at,
                }),
            });
        }
        PeerMessage::RouteWithdraw {
            name,
            relay_id,
            removed_at,
            removed_revision,
        } => {
            let _ = RouteAuthority::new(&state).execute(RouteCommand::WithdrawPeer {
                sender_relay_id,
                name,
                relay_id,
                removed_at,
                removed_revision,
            });
        }
        _ => {
            if matches!(envelope.message, PeerMessage::Unknown) {
                eprintln!(
                    "[relay] Ignoring unknown message type from {}",
                    envelope.sender_relay_id
                );
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
        return (
            StatusCode::OK,
            Json(serde_json::json!({"status": "ignored"})),
        )
            .into_response();
    }
    let sender_relay_id = envelope.sender_relay_id.clone();
    if let PeerMessage::RelayJoin { relay_id, url } = envelope.message {
        if relay_id != sender_relay_id {
            return (
                StatusCode::OK,
                Json(serde_json::json!({"status": "ignored"})),
            )
                .into_response();
        }
        if !valid_peer_relay_id(&relay_id) || !valid_peer_relay_url(&url) {
            return (
                StatusCode::OK,
                Json(serde_json::json!({"status": "ignored"})),
            )
                .into_response();
        }
        if relay_id == state.relay_id() {
            return (StatusCode::OK, Json(serde_json::json!({"status": "self"}))).into_response();
        }
        if state.with_route_table_mut(|rt| rt.record_peer_join(relay_id, url)) {
            Json(current_full_sync_envelope(&state)).into_response()
        } else {
            (
                StatusCode::OK,
                Json(serde_json::json!({"status": "ignored"})),
            )
                .into_response()
        }
    } else {
        StatusCode::BAD_REQUEST.into_response()
    }
}

/// GET /_peer/sync — return full route table + peer list
pub async fn handle_peer_sync(State(state): State<Arc<RelayState>>) -> impl IntoResponse {
    Json(current_full_sync_envelope(&state))
}

/// POST /_peer/heartbeat — periodic liveness check
pub async fn handle_peer_heartbeat(
    State(state): State<Arc<RelayState>>,
    Json(envelope): Json<PeerEnvelope>,
) -> impl IntoResponse {
    if !check_protocol_version(&envelope) {
        return StatusCode::OK;
    }
    let envelope = match validate_route_state_envelope(envelope, None) {
        Ok(validated) => validated.into_envelope(),
        Err(_) => return StatusCode::OK,
    };
    let sender_relay_id = envelope.sender_relay_id.clone();
    if let PeerMessage::Heartbeat {
        relay_id,
        route_count,
    } = envelope.message
    {
        if !valid_peer_relay_id(&sender_relay_id)
            || !valid_peer_relay_id(&relay_id)
            || sender_relay_id != relay_id
            || !known_peer(&state, &sender_relay_id)
        {
            return StatusCode::OK;
        }
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
    let envelope = match validate_route_state_envelope(envelope, None) {
        Ok(validated) => validated.into_envelope(),
        Err(_) => return StatusCode::OK,
    };
    let sender_relay_id = envelope.sender_relay_id.clone();
    if let PeerMessage::RelayLeave { relay_id } = envelope.message {
        if !valid_peer_relay_id(&sender_relay_id)
            || !valid_peer_relay_id(&relay_id)
            || sender_relay_id != relay_id
            || !trusted_peer(&state, &sender_relay_id)
        {
            return StatusCode::OK;
        }
        let _ = RouteAuthority::new(&state).execute(RouteCommand::RemovePeerRoutes {
            relay_id: relay_id.clone(),
        });
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
    let sender_relay_id = envelope.sender_relay_id.clone();
    if !valid_peer_relay_id(&sender_relay_id) || !trusted_peer(&state, &sender_relay_id) {
        return StatusCode::OK.into_response();
    }
    let validated = match validate_route_state_envelope(envelope, Some(&sender_relay_id)) {
        Ok(validated) => validated,
        Err(_) => return StatusCode::OK.into_response(),
    };
    let digest_diff_entries = validated.clone().into_digest_diff_entries();
    let envelope = validated.into_envelope();
    match envelope.message {
        PeerMessage::DigestExchange { digest } => {
            let our_digest = state.route_digest();

            let peer_map: std::collections::HashMap<(String, String, bool), RouteDigestHash> =
                digest
                    .iter()
                    .map(|d| {
                        (
                            (d.name.clone(), d.relay_id.clone(), d.deleted),
                            d.hash.clone(),
                        )
                    })
                    .collect();

            let mut diff_entries = Vec::new();
            for (key, our_hash) in &our_digest {
                if key.1 != state.relay_id() {
                    continue;
                }
                match peer_map.get(key) {
                    Some(peer_hash) if peer_hash == our_hash => {}
                    _ => {
                        // We have this route and peer doesn't, or hashes differ.
                        if let Some(entry) = state.route_state_for_diff(&key.0, &key.1, key.2) {
                            diff_entries.push(entry);
                        }
                    }
                }
            }

            for peer_entry in digest {
                if peer_entry.relay_id == state.relay_id()
                    && !peer_entry.deleted
                    && state
                        .route_state_for_diff(&peer_entry.name, &peer_entry.relay_id, false)
                        .is_none()
                {
                    let tombstone = state
                        .authoritative_missing_tombstone(&peer_entry.name, &peer_entry.relay_id);
                    if let Some(tombstone) = tombstone {
                        let hash = tombstone_digest_hash(&tombstone);
                        diff_entries.push(DigestDiffEntry::Deleted {
                            name: tombstone.name,
                            relay_id: tombstone.relay_id,
                            removed_at: tombstone.removed_at,
                            removed_revision: tombstone.removed_revision,
                            hash,
                        });
                    }
                }
            }

            let response = PeerEnvelope::new(
                state.relay_id(),
                PeerMessage::DigestDiff {
                    entries: diff_entries,
                },
            );
            Json(response).into_response()
        }
        PeerMessage::DigestDiff { .. } => {
            let Some(entries) = digest_diff_entries else {
                return StatusCode::OK.into_response();
            };
            for diff in entries {
                match diff {
                    ValidatedDigestDiffEntry::Active(active) => {
                        let entry = active.into();
                        let _ = RouteAuthority::new(&state).execute(RouteCommand::AnnouncePeer {
                            sender_relay_id: sender_relay_id.clone(),
                            entry: Box::new(entry),
                        });
                    }
                    ValidatedDigestDiffEntry::Deleted(deleted) => {
                        let crate::relay::peer::ValidatedDigestDiffDeleted {
                            name,
                            relay_id,
                            removed_at,
                            removed_revision,
                        } = deleted;
                        let _ = RouteAuthority::new(&state).execute(RouteCommand::WithdrawPeer {
                            sender_relay_id: sender_relay_id.clone(),
                            name,
                            relay_id,
                            removed_at,
                            removed_revision,
                        });
                    }
                }
            }
            StatusCode::OK.into_response()
        }
        _ => StatusCode::BAD_REQUEST.into_response(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::body::to_bytes;
    use axum::response::IntoResponse;
    use c2_config::RelayConfig;

    const TEST_ABI_HASH: &str = "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef";
    const TEST_SIGNATURE_HASH: &str =
        "abcdef0123456789abcdef0123456789abcdef0123456789abcdef0123456789";

    struct NullDisseminator;

    impl crate::relay::disseminator::Disseminator for NullDisseminator {
        fn broadcast(
            &self,
            _envelope: PeerEnvelope,
            _peers: &[PeerSnapshot],
        ) -> Option<tokio::task::JoinHandle<()>> {
            None
        }
    }

    fn test_state() -> Arc<RelayState> {
        Arc::new(RelayState::new(
            Arc::new(RelayConfig {
                relay_id: "relay-a".into(),
                advertise_url: "http://relay-a:8080".into(),
                ..Default::default()
            }),
            Arc::new(NullDisseminator),
        ))
    }

    fn known_peer(state: &RelayState, relay_id: &str) {
        known_peer_with_status(state, relay_id, PeerStatus::Alive);
    }

    fn known_peer_with_status(state: &RelayState, relay_id: &str, status: PeerStatus) {
        state.register_peer(PeerInfo {
            relay_id: relay_id.into(),
            url: format!("http://{relay_id}:8080"),
            route_count: 0,
            last_heartbeat: std::time::Instant::now(),
            status,
        });
    }

    fn tombstone_count(state: &RelayState) -> usize {
        state.with_route_table(|rt| rt.list_tombstones().len())
    }

    fn test_hash() -> String {
        "0".repeat(64)
    }

    fn active_diff(
        name: &str,
        relay_id: &str,
        relay_url: &str,
        crm_name: &str,
    ) -> crate::relay::peer::DigestDiffEntry {
        let mut entry = crate::relay::peer::DigestDiffEntry::Active {
            name: name.into(),
            relay_id: relay_id.into(),
            relay_url: relay_url.into(),
            crm_ns: "test.ns".into(),
            crm_name: crm_name.into(),
            crm_ver: "0.1.0".into(),
            abi_hash: TEST_ABI_HASH.into(),
            signature_hash: TEST_SIGNATURE_HASH.into(),
            max_payload_size: 1024,
            route_uid: "grid-route-uid-0001".into(),
            route_revision: 1,
            registered_at: 1000.0,
            hash: String::new(),
        };
        let hash = crate::relay::peer::route_digest_hash_for_diff_entry(&entry).unwrap();
        if let crate::relay::peer::DigestDiffEntry::Active { hash: slot, .. } = &mut entry {
            *slot = hash;
        }
        entry
    }

    fn deleted_diff(
        name: &str,
        relay_id: &str,
        removed_at: f64,
    ) -> crate::relay::peer::DigestDiffEntry {
        let mut entry = crate::relay::peer::DigestDiffEntry::Deleted {
            name: name.into(),
            relay_id: relay_id.into(),
            removed_at,
            removed_revision: 1,
            hash: String::new(),
        };
        let hash = crate::relay::peer::route_digest_hash_for_diff_entry(&entry).unwrap();
        if let crate::relay::peer::DigestDiffEntry::Deleted { hash: slot, .. } = &mut entry {
            *slot = hash;
        }
        entry
    }

    fn announce_peer_route(state: &RelayState, entry: RouteEntry) {
        let sender_relay_id = entry.relay_id.clone();
        RouteAuthority::new(state)
            .execute(RouteCommand::AnnouncePeer {
                sender_relay_id,
                entry: Box::new(entry),
            })
            .unwrap();
    }

    #[tokio::test]
    async fn unknown_peer_announce_is_ignored() {
        let state = test_state();
        let envelope = PeerEnvelope::new(
            "relay-b",
            PeerMessage::RouteAnnounce {
                name: "grid".into(),
                relay_id: "relay-b".into(),
                relay_url: "http://relay-b:8080".into(),
                crm_ns: "test.ns".into(),
                crm_name: "Grid".into(),
                crm_ver: "0.1.0".into(),
                abi_hash: TEST_ABI_HASH.into(),
                signature_hash: TEST_SIGNATURE_HASH.into(),
                max_payload_size: 1024,
                route_uid: "grid-route-uid-0001".into(),
                route_revision: 1,
                registered_at: 1000.0,
            },
        );

        let response = handle_peer_announce(State(state.clone()), Json(envelope))
            .await
            .into_response();

        assert_eq!(response.status(), StatusCode::OK);
        assert!(state.resolve("grid").is_empty());
    }

    #[tokio::test]
    async fn announce_relay_id_must_match_sender() {
        let state = test_state();
        known_peer(&state, "relay-b");
        let envelope = PeerEnvelope::new(
            "relay-b",
            PeerMessage::RouteAnnounce {
                name: "grid".into(),
                relay_id: "relay-c".into(),
                relay_url: "http://relay-c:8080".into(),
                crm_ns: "test.ns".into(),
                crm_name: "Grid".into(),
                crm_ver: "0.1.0".into(),
                abi_hash: TEST_ABI_HASH.into(),
                signature_hash: TEST_SIGNATURE_HASH.into(),
                max_payload_size: 1024,
                route_uid: "grid-route-uid-0001".into(),
                route_revision: 1,
                registered_at: 1000.0,
            },
        );

        let response = handle_peer_announce(State(state.clone()), Json(envelope))
            .await
            .into_response();

        assert_eq!(response.status(), StatusCode::OK);
        assert!(state.resolve("grid").is_empty());
    }

    #[tokio::test]
    async fn announce_rejects_route_name_that_cannot_fit_wire_control() {
        let state = test_state();
        known_peer(&state, "relay-b");
        let long_name = "x".repeat(c2_contract::MAX_WIRE_TEXT_BYTES + 1);
        let envelope = PeerEnvelope::new(
            "relay-b",
            PeerMessage::RouteAnnounce {
                name: long_name,
                relay_id: "relay-b".into(),
                relay_url: "http://relay-b:8080".into(),
                crm_ns: "test.ns".into(),
                crm_name: "Grid".into(),
                crm_ver: "0.1.0".into(),
                abi_hash: TEST_ABI_HASH.into(),
                signature_hash: TEST_SIGNATURE_HASH.into(),
                max_payload_size: 1024,
                route_uid: "grid-route-uid-0001".into(),
                route_revision: 1,
                registered_at: 1000.0,
            },
        );

        let response = handle_peer_announce(State(state.clone()), Json(envelope))
            .await
            .into_response();

        assert_eq!(response.status(), StatusCode::OK);
        assert!(state.route_names().is_empty());
    }

    #[tokio::test]
    async fn announce_rejects_invalid_crm_tag_fields() {
        let state = test_state();
        known_peer(&state, "relay-b");
        let too_long = "x".repeat(c2_contract::MAX_WIRE_TEXT_BYTES + 1);

        for (idx, (crm_ns, crm_name, crm_ver)) in [
            ("", "Grid", "0.1.0"),
            ("test.ns", "", "0.1.0"),
            ("test.ns", "Grid", ""),
            ("test.ns", "Bad\0Grid", "0.1.0"),
            ("test.ns", too_long.as_str(), "0.1.0"),
        ]
        .into_iter()
        .enumerate()
        {
            let route_name = format!("grid-invalid-crm-{idx}");
            let envelope = PeerEnvelope::new(
                "relay-b",
                PeerMessage::RouteAnnounce {
                    name: route_name.clone(),
                    relay_id: "relay-b".into(),
                    relay_url: "http://relay-b:8080".into(),
                    crm_ns: crm_ns.into(),
                    crm_name: crm_name.into(),
                    crm_ver: crm_ver.into(),
                    abi_hash: TEST_ABI_HASH.into(),
                    signature_hash: TEST_SIGNATURE_HASH.into(),
                    max_payload_size: 1024,
                    route_uid: "grid-route-uid-0001".into(),
                    route_revision: 1,
                    registered_at: 1000.0,
                },
            );

            let response = handle_peer_announce(State(state.clone()), Json(envelope))
                .await
                .into_response();

            assert_eq!(response.status(), StatusCode::OK);
            assert!(
                state.resolve(&route_name).is_empty(),
                "invalid CRM tag should not be stored for {route_name}"
            );
        }
    }

    #[tokio::test]
    async fn announce_uses_known_peer_url_instead_of_message_url() {
        let state = test_state();
        known_peer(&state, "relay-b");
        let envelope = PeerEnvelope::new(
            "relay-b",
            PeerMessage::RouteAnnounce {
                name: "grid".into(),
                relay_id: "relay-b".into(),
                relay_url: "http://spoofed:8080".into(),
                crm_ns: "test.ns".into(),
                crm_name: "Grid".into(),
                crm_ver: "0.1.0".into(),
                abi_hash: TEST_ABI_HASH.into(),
                signature_hash: TEST_SIGNATURE_HASH.into(),
                max_payload_size: 1024,
                route_uid: "grid-route-uid-0001".into(),
                route_revision: 1,
                registered_at: 1000.0,
            },
        );

        let response = handle_peer_announce(State(state.clone()), Json(envelope))
            .await
            .into_response();

        assert_eq!(response.status(), StatusCode::OK);
        let routes = state.resolve("grid");
        assert_eq!(routes.len(), 1);
        assert_eq!(routes[0].relay_url, "http://relay-b:8080");
    }

    #[tokio::test]
    async fn dead_peer_announce_is_ignored() {
        let state = test_state();
        known_peer_with_status(&state, "relay-b", PeerStatus::Dead);
        let envelope = PeerEnvelope::new(
            "relay-b",
            PeerMessage::RouteAnnounce {
                name: "grid".into(),
                relay_id: "relay-b".into(),
                relay_url: "http://relay-b:8080".into(),
                crm_ns: "test.ns".into(),
                crm_name: "Grid".into(),
                crm_ver: "0.1.0".into(),
                abi_hash: TEST_ABI_HASH.into(),
                signature_hash: TEST_SIGNATURE_HASH.into(),
                max_payload_size: 1024,
                route_uid: "grid-route-uid-0001".into(),
                route_revision: 1,
                registered_at: 1000.0,
            },
        );

        let response = handle_peer_announce(State(state.clone()), Json(envelope))
            .await
            .into_response();

        assert_eq!(response.status(), StatusCode::OK);
        assert!(state.resolve("grid").is_empty());
        assert!(state.list_routes().is_empty());
    }

    #[tokio::test]
    async fn join_updates_existing_peer_url() {
        let state = test_state();
        known_peer(&state, "relay-b");
        let envelope = PeerEnvelope::new(
            "relay-b",
            PeerMessage::RelayJoin {
                relay_id: "relay-b".into(),
                url: "http://relay-b-new:8080".into(),
            },
        );

        let response = handle_peer_join(State(state.clone()), Json(envelope))
            .await
            .into_response();

        assert_eq!(response.status(), StatusCode::OK);
        assert_eq!(
            state
                .list_peers()
                .into_iter()
                .find(|peer| peer.relay_id == "relay-b")
                .unwrap()
                .url,
            "http://relay-b-new:8080"
        );
    }

    #[tokio::test]
    async fn join_updates_existing_peer_route_urls() {
        let state = test_state();
        known_peer(&state, "relay-b");
        announce_peer_route(
            &state,
            RouteEntry {
                name: "grid".into(),
                relay_id: "relay-b".into(),
                relay_url: "http://spoofed:8080".into(),
                server_id: None,
                server_instance_id: None,
                ipc_address: None,
                crm_ns: "test.ns".into(),
                crm_name: "Grid".into(),
                crm_ver: "0.1.0".into(),
                abi_hash: TEST_ABI_HASH.into(),
                signature_hash: TEST_SIGNATURE_HASH.into(),
                max_payload_size: 1024,
                route_uid: "grid-route-uid-0001".into(),
                route_revision: 1,
                locality: Locality::Peer,
                registered_at: 1000.0,
            },
        );
        assert_eq!(state.resolve("grid")[0].relay_url, "http://relay-b:8080");

        let envelope = PeerEnvelope::new(
            "relay-b",
            PeerMessage::RelayJoin {
                relay_id: "relay-b".into(),
                url: "http://relay-b-new:8080".into(),
            },
        );

        let response = handle_peer_join(State(state.clone()), Json(envelope))
            .await
            .into_response();

        assert_eq!(response.status(), StatusCode::OK);
        assert_eq!(
            state.resolve("grid")[0].relay_url,
            "http://relay-b-new:8080"
        );
    }

    #[tokio::test]
    async fn heartbeat_from_known_suspect_peer_marks_alive() {
        let state = test_state();
        known_peer_with_status(&state, "relay-b", PeerStatus::Suspect);
        let envelope = PeerEnvelope::new(
            "relay-b",
            PeerMessage::Heartbeat {
                relay_id: "relay-b".into(),
                route_count: 7,
            },
        );

        let response = handle_peer_heartbeat(State(state.clone()), Json(envelope))
            .await
            .into_response();

        assert_eq!(response.status(), StatusCode::OK);
        let peer = state
            .list_peers()
            .into_iter()
            .find(|peer| peer.relay_id == "relay-b")
            .unwrap();
        assert_eq!(peer.status, PeerStatus::Alive);
        assert_eq!(peer.route_count, 7);
    }

    #[tokio::test]
    async fn heartbeat_from_known_dead_peer_marks_alive() {
        let state = test_state();
        known_peer_with_status(&state, "relay-b", PeerStatus::Dead);
        let envelope = PeerEnvelope::new(
            "relay-b",
            PeerMessage::Heartbeat {
                relay_id: "relay-b".into(),
                route_count: 7,
            },
        );

        let response = handle_peer_heartbeat(State(state.clone()), Json(envelope))
            .await
            .into_response();

        assert_eq!(response.status(), StatusCode::OK);
        let peer = state
            .list_peers()
            .into_iter()
            .find(|peer| peer.relay_id == "relay-b")
            .unwrap();
        assert_eq!(peer.status, PeerStatus::Alive);
        assert_eq!(peer.route_count, 7);
    }

    #[tokio::test]
    async fn relay_leave_rejects_old_route_state_protocol_before_removing_routes() {
        let state = test_state();
        known_peer(&state, "relay-b");
        announce_peer_route(
            &state,
            RouteEntry {
                name: "grid".into(),
                relay_id: "relay-b".into(),
                relay_url: "http://relay-b:8080".into(),
                server_id: None,
                server_instance_id: None,
                ipc_address: None,
                crm_ns: "test.ns".into(),
                crm_name: "Grid".into(),
                crm_ver: "0.1.0".into(),
                abi_hash: TEST_ABI_HASH.into(),
                signature_hash: TEST_SIGNATURE_HASH.into(),
                max_payload_size: 1024,
                route_uid: "grid-route-uid-0001".into(),
                route_revision: 1,
                locality: Locality::Peer,
                registered_at: 1000.0,
            },
        );
        let mut envelope = PeerEnvelope::new(
            "relay-b",
            PeerMessage::RelayLeave {
                relay_id: "relay-b".into(),
            },
        );
        envelope.protocol_version = crate::relay::peer::ROUTE_HASH_PEER_VERSION - 1;

        let response = handle_peer_leave(State(state.clone()), Json(envelope))
            .await
            .into_response();

        assert_eq!(response.status(), StatusCode::OK);
        assert_eq!(state.resolve("grid").len(), 1);
        assert!(state.has_peer("relay-b"));
    }

    #[tokio::test]
    async fn route_withdraw_rejects_old_protocol_before_removing_route() {
        let state = test_state();
        known_peer(&state, "relay-b");
        announce_peer_route(
            &state,
            RouteEntry {
                name: "grid".into(),
                relay_id: "relay-b".into(),
                relay_url: "http://relay-b:8080".into(),
                server_id: None,
                server_instance_id: None,
                ipc_address: None,
                crm_ns: "test.ns".into(),
                crm_name: "Grid".into(),
                crm_ver: "0.1.0".into(),
                abi_hash: TEST_ABI_HASH.into(),
                signature_hash: TEST_SIGNATURE_HASH.into(),
                max_payload_size: 1024,
                route_uid: "grid-route-uid-0001".into(),
                route_revision: 1,
                locality: Locality::Peer,
                registered_at: 1000.0,
            },
        );
        let mut envelope = PeerEnvelope::new(
            "relay-b",
            PeerMessage::RouteWithdraw {
                name: "grid".into(),
                relay_id: "relay-b".into(),
                removed_at: 1001.0,
                removed_revision: 1,
            },
        );
        envelope.protocol_version = crate::relay::peer::ROUTE_HASH_PEER_VERSION - 1;

        let response = handle_peer_announce(State(state.clone()), Json(envelope))
            .await
            .into_response();

        assert_eq!(response.status(), StatusCode::OK);
        assert_eq!(state.resolve("grid").len(), 1);
        assert_eq!(tombstone_count(&state), 0);
    }

    #[tokio::test]
    async fn digest_exchange_rejects_old_protocol_before_creating_authoritative_tombstone() {
        let state = test_state();
        known_peer(&state, "relay-b");
        let mut envelope = PeerEnvelope::new(
            "relay-b",
            PeerMessage::DigestExchange {
                digest: vec![crate::relay::peer::DigestEntry {
                    name: "grid".into(),
                    relay_id: "relay-a".into(),
                    deleted: false,
                    hash: test_hash(),
                }],
            },
        );
        envelope.protocol_version = crate::relay::peer::ROUTE_HASH_PEER_VERSION - 1;

        let response = handle_peer_digest(State(state.clone()), Json(envelope))
            .await
            .into_response();

        assert_eq!(response.status(), StatusCode::OK);
        assert_eq!(tombstone_count(&state), 0);
    }

    #[tokio::test]
    async fn digest_diff_deleted_rejects_old_protocol_before_removing_route() {
        let state = test_state();
        known_peer(&state, "relay-b");
        announce_peer_route(
            &state,
            RouteEntry {
                name: "grid".into(),
                relay_id: "relay-b".into(),
                relay_url: "http://relay-b:8080".into(),
                server_id: None,
                server_instance_id: None,
                ipc_address: None,
                crm_ns: "test.ns".into(),
                crm_name: "Grid".into(),
                crm_ver: "0.1.0".into(),
                abi_hash: TEST_ABI_HASH.into(),
                signature_hash: TEST_SIGNATURE_HASH.into(),
                max_payload_size: 1024,
                route_uid: "grid-route-uid-0001".into(),
                route_revision: 1,
                locality: Locality::Peer,
                registered_at: 1000.0,
            },
        );
        let mut envelope = PeerEnvelope::new(
            "relay-b",
            PeerMessage::DigestDiff {
                entries: vec![deleted_diff("grid", "relay-b", 1001.0)],
            },
        );
        envelope.protocol_version = crate::relay::peer::ROUTE_HASH_PEER_VERSION - 1;

        let response = handle_peer_digest(State(state.clone()), Json(envelope))
            .await
            .into_response();

        assert_eq!(response.status(), StatusCode::OK);
        assert_eq!(state.resolve("grid").len(), 1);
        assert_eq!(tombstone_count(&state), 0);
    }

    #[tokio::test]
    async fn peer_withdraw_cannot_remove_local_route() {
        let state = test_state();
        known_peer(&state, "relay-b");
        state.with_route_table_mut(|rt| {
            rt.register_route(RouteEntry {
                name: "grid".into(),
                relay_id: "relay-a".into(),
                relay_url: "http://relay-a:8080".into(),
                server_id: Some("server-secret".into()),
                server_instance_id: Some("instance-secret".into()),
                ipc_address: Some("ipc://secret".into()),
                crm_ns: "test.ns".into(),
                crm_name: "Grid".into(),
                crm_ver: "0.1.0".into(),
                abi_hash: TEST_ABI_HASH.into(),
                signature_hash: TEST_SIGNATURE_HASH.into(),
                max_payload_size: 1024,
                route_uid: "grid-route-uid-0001".into(),
                route_revision: 1,
                locality: Locality::Local,
                registered_at: 1000.0,
            });
        });
        let envelope = PeerEnvelope::new(
            "relay-b",
            PeerMessage::RouteWithdraw {
                name: "grid".into(),
                relay_id: "relay-a".into(),
                removed_at: 1001.0,
                removed_revision: 1,
            },
        );

        let response = handle_peer_announce(State(state.clone()), Json(envelope))
            .await
            .into_response();

        assert_eq!(response.status(), StatusCode::OK);
        assert_eq!(state.resolve("grid").len(), 1);
    }

    #[tokio::test]
    async fn join_relay_id_must_match_sender() {
        let state = test_state();
        let envelope = PeerEnvelope::new(
            "relay-b",
            PeerMessage::RelayJoin {
                relay_id: "relay-c".into(),
                url: "http://relay-c:8080".into(),
            },
        );

        let response = handle_peer_join(State(state.clone()), Json(envelope))
            .await
            .into_response();

        assert_eq!(response.status(), StatusCode::OK);
        assert!(!state.has_peer("relay-c"));
    }

    #[tokio::test]
    async fn join_rejects_invalid_relay_id_or_url_without_mutating_peer_table() {
        for (sender, relay_id, url) in [
            ("bad/relay", "bad/relay", "http://bad-relay:8080"),
            ("relay-b", "relay-b", "not a url"),
        ] {
            let state = test_state();
            let envelope = PeerEnvelope::new(
                sender,
                PeerMessage::RelayJoin {
                    relay_id: relay_id.into(),
                    url: url.into(),
                },
            );

            let response = handle_peer_join(State(state.clone()), Json(envelope))
                .await
                .into_response();
            let status = response.status();
            let bytes = to_bytes(response.into_body(), usize::MAX).await.unwrap();
            let body: serde_json::Value = serde_json::from_slice(&bytes).unwrap();

            assert_eq!(status, StatusCode::OK);
            assert_eq!(
                body.get("status").and_then(serde_json::Value::as_str),
                Some("ignored")
            );
            assert!(
                !state.has_peer(relay_id),
                "invalid join mutated peer {relay_id}"
            );
        }
    }

    #[tokio::test]
    async fn digest_diff_entries_must_belong_to_sender() {
        let state = test_state();
        known_peer(&state, "relay-b");
        let envelope = PeerEnvelope::new(
            "relay-b",
            PeerMessage::DigestDiff {
                entries: vec![active_diff(
                    "grid",
                    "relay-c",
                    "http://relay-c:8080",
                    "Grid",
                )],
            },
        );

        let response = handle_peer_digest(State(state.clone()), Json(envelope))
            .await
            .into_response();

        assert_eq!(response.status(), StatusCode::OK);
        assert!(state.resolve("grid").is_empty());
    }

    #[tokio::test]
    async fn digest_diff_rejects_invalid_crm_tag_fields() {
        let state = test_state();
        known_peer(&state, "relay-b");
        let envelope = PeerEnvelope::new(
            "relay-b",
            PeerMessage::DigestDiff {
                entries: vec![crate::relay::peer::DigestDiffEntry::Active {
                    name: "grid".into(),
                    relay_id: "relay-b".into(),
                    relay_url: "http://relay-b:8080".into(),
                    crm_ns: "test.ns".into(),
                    crm_name: "Grid\nInjected".into(),
                    crm_ver: "0.1.0".into(),
                    abi_hash: TEST_ABI_HASH.into(),
                    signature_hash: TEST_SIGNATURE_HASH.into(),
                    max_payload_size: 1024,
                    route_uid: "grid-route-uid-0001".into(),
                    route_revision: 1,
                    registered_at: 1000.0,
                    hash: test_hash(),
                }],
            },
        );

        let response = handle_peer_digest(State(state.clone()), Json(envelope))
            .await
            .into_response();

        assert_eq!(response.status(), StatusCode::OK);
        assert!(state.resolve("grid").is_empty());
    }

    #[tokio::test]
    async fn digest_diff_uses_known_peer_url_instead_of_message_url() {
        let state = test_state();
        known_peer(&state, "relay-b");
        let envelope = PeerEnvelope::new(
            "relay-b",
            PeerMessage::DigestDiff {
                entries: vec![active_diff(
                    "grid",
                    "relay-b",
                    "http://spoofed:8080",
                    "Grid",
                )],
            },
        );

        let response = handle_peer_digest(State(state.clone()), Json(envelope))
            .await
            .into_response();

        assert_eq!(response.status(), StatusCode::OK);
        let routes = state.resolve("grid");
        assert_eq!(routes.len(), 1);
        assert_eq!(routes[0].relay_url, "http://relay-b:8080");
    }

    #[tokio::test]
    async fn dead_peer_digest_diff_is_ignored() {
        let state = test_state();
        known_peer_with_status(&state, "relay-b", PeerStatus::Dead);
        let envelope = PeerEnvelope::new(
            "relay-b",
            PeerMessage::DigestDiff {
                entries: vec![active_diff(
                    "grid",
                    "relay-b",
                    "http://relay-b:8080",
                    "Grid",
                )],
            },
        );

        let response = handle_peer_digest(State(state.clone()), Json(envelope))
            .await
            .into_response();

        assert_eq!(response.status(), StatusCode::OK);
        assert!(state.resolve("grid").is_empty());
        assert!(state.list_routes().is_empty());
    }

    #[tokio::test]
    async fn digest_exchange_only_advertises_sender_owned_routes() {
        let state = test_state();
        known_peer(&state, "relay-b");
        known_peer(&state, "relay-c");
        state.with_route_table_mut(|rt| {
            rt.register_route(RouteEntry {
                name: "local".into(),
                relay_id: "relay-a".into(),
                relay_url: "http://relay-a:8080".into(),
                server_id: Some("server-local".into()),
                server_instance_id: Some("instance-local".into()),
                ipc_address: Some("ipc://local".into()),
                crm_ns: "test.ns".into(),
                crm_name: "Local".into(),
                crm_ver: "0.1.0".into(),
                abi_hash: TEST_ABI_HASH.into(),
                signature_hash: TEST_SIGNATURE_HASH.into(),
                max_payload_size: 1024,
                route_uid: "grid-route-uid-0001".into(),
                route_revision: 1,
                locality: Locality::Local,
                registered_at: 1000.0,
            });
        });
        announce_peer_route(
            &state,
            RouteEntry {
                name: "remote".into(),
                relay_id: "relay-c".into(),
                relay_url: "http://spoofed:8080".into(),
                server_id: None,
                server_instance_id: None,
                ipc_address: None,
                crm_ns: "test.ns".into(),
                crm_name: "Remote".into(),
                crm_ver: "0.1.0".into(),
                abi_hash: TEST_ABI_HASH.into(),
                signature_hash: TEST_SIGNATURE_HASH.into(),
                max_payload_size: 1024,
                route_uid: "grid-route-uid-0001".into(),
                route_revision: 1,
                locality: Locality::Peer,
                registered_at: 1001.0,
            },
        );
        let envelope = PeerEnvelope::new("relay-b", PeerMessage::DigestExchange { digest: vec![] });

        let response = handle_peer_digest(State(state), Json(envelope))
            .await
            .into_response();
        let bytes = to_bytes(response.into_body(), usize::MAX).await.unwrap();
        let decoded: PeerEnvelope = serde_json::from_slice(&bytes).unwrap();

        match decoded.message {
            PeerMessage::DigestDiff { entries } => {
                assert_eq!(entries.len(), 1);
                match &entries[0] {
                    crate::relay::peer::DigestDiffEntry::Active { name, relay_id, .. } => {
                        assert_eq!(name, "local");
                        assert_eq!(relay_id, "relay-a");
                    }
                    other => panic!("expected active diff, got {other:?}"),
                }
            }
            _ => panic!("expected DigestDiff"),
        }
    }

    #[tokio::test]
    async fn digest_exchange_repairs_stale_peer_route_with_tombstone() {
        let state = test_state();
        known_peer(&state, "relay-b");
        let envelope = PeerEnvelope::new(
            "relay-b",
            PeerMessage::DigestExchange {
                digest: vec![crate::relay::peer::DigestEntry {
                    name: "grid".into(),
                    relay_id: "relay-a".into(),
                    deleted: false,
                    hash: test_hash(),
                }],
            },
        );

        let response = handle_peer_digest(State(state.clone()), Json(envelope))
            .await
            .into_response();
        let bytes = to_bytes(response.into_body(), usize::MAX).await.unwrap();
        let decoded: PeerEnvelope = serde_json::from_slice(&bytes).unwrap();

        match decoded.message {
            PeerMessage::DigestDiff { entries } => {
                assert_eq!(entries.len(), 1);
                match &entries[0] {
                    crate::relay::peer::DigestDiffEntry::Deleted { name, relay_id, .. } => {
                        assert_eq!(name, "grid");
                        assert_eq!(relay_id, "relay-a");
                    }
                    other => panic!("expected deleted diff, got {other:?}"),
                }
            }
            _ => panic!("expected DigestDiff"),
        }
        assert_eq!(tombstone_count(&state), 1);
    }

    #[tokio::test]
    async fn digest_exchange_rejects_wire_invalid_authoritative_missing_name() {
        let state = test_state();
        known_peer(&state, "relay-b");
        let long_name = "x".repeat(c2_contract::MAX_WIRE_TEXT_BYTES + 1);
        let envelope = PeerEnvelope::new(
            "relay-b",
            PeerMessage::DigestExchange {
                digest: vec![crate::relay::peer::DigestEntry {
                    name: long_name,
                    relay_id: "relay-a".into(),
                    deleted: false,
                    hash: test_hash(),
                }],
            },
        );

        let response = handle_peer_digest(State(state.clone()), Json(envelope))
            .await
            .into_response();

        assert_eq!(response.status(), StatusCode::OK);
        assert_eq!(tombstone_count(&state), 0);
    }

    #[tokio::test]
    async fn digest_diff_deleted_removes_sender_route() {
        let state = test_state();
        known_peer(&state, "relay-b");
        announce_peer_route(
            &state,
            RouteEntry {
                name: "grid".into(),
                relay_id: "relay-b".into(),
                relay_url: "http://relay-b:8080".into(),
                server_id: None,
                server_instance_id: None,
                ipc_address: None,
                crm_ns: "test.ns".into(),
                crm_name: "Grid".into(),
                crm_ver: "0.1.0".into(),
                abi_hash: TEST_ABI_HASH.into(),
                signature_hash: TEST_SIGNATURE_HASH.into(),
                max_payload_size: 1024,
                route_uid: "grid-route-uid-0001".into(),
                route_revision: 1,
                locality: Locality::Peer,
                registered_at: 1000.0,
            },
        );
        let envelope = PeerEnvelope::new(
            "relay-b",
            PeerMessage::DigestDiff {
                entries: vec![deleted_diff("grid", "relay-b", 1001.0)],
            },
        );

        let response = handle_peer_digest(State(state.clone()), Json(envelope))
            .await
            .into_response();

        assert_eq!(response.status(), StatusCode::OK);
        assert!(state.resolve("grid").is_empty());
        assert_eq!(tombstone_count(&state), 1);
    }

    #[tokio::test]
    async fn peer_sync_scrubs_local_owner_identity() {
        let state = test_state();
        state.with_route_table_mut(|rt| {
            rt.register_route(RouteEntry {
                name: "grid".into(),
                relay_id: "relay-a".into(),
                relay_url: "http://relay-a:8080".into(),
                server_id: Some("server-secret".into()),
                server_instance_id: Some("instance-secret".into()),
                ipc_address: Some("ipc://secret".into()),
                crm_ns: "test.ns".into(),
                crm_name: "Grid".into(),
                crm_ver: "0.1.0".into(),
                abi_hash: TEST_ABI_HASH.into(),
                signature_hash: TEST_SIGNATURE_HASH.into(),
                max_payload_size: 1024,
                route_uid: "grid-route-uid-0001".into(),
                route_revision: 1,
                locality: Locality::Local,
                registered_at: 1000.0,
            });
        });

        let response = handle_peer_sync(State(state)).await.into_response();
        let bytes = to_bytes(response.into_body(), usize::MAX).await.unwrap();
        let envelope: FullSyncEnvelope = serde_json::from_slice(&bytes).unwrap();

        assert_eq!(envelope.snapshot.routes.len(), 1);
        assert_eq!(envelope.snapshot.routes[0].name, "grid");
        assert!(envelope.snapshot.routes[0].hash.len() == 64);
        let value: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
        assert!(value["snapshot"]["routes"][0].get("server_id").is_none());
        assert!(value["snapshot"]["routes"][0].get("ipc_address").is_none());
    }

    #[tokio::test]
    async fn peer_sync_snapshot_can_be_merged_by_receiver() {
        let state = test_state();
        state.with_route_table_mut(|rt| {
            rt.register_route(RouteEntry {
                name: "grid".into(),
                relay_id: "relay-a".into(),
                relay_url: "http://relay-a:8080".into(),
                server_id: Some("server-grid".into()),
                server_instance_id: Some("instance-grid".into()),
                ipc_address: Some("ipc://grid".into()),
                crm_ns: "test.ns".into(),
                crm_name: "Grid".into(),
                crm_ver: "0.1.0".into(),
                abi_hash: TEST_ABI_HASH.into(),
                signature_hash: TEST_SIGNATURE_HASH.into(),
                max_payload_size: 1024,
                route_uid: "grid-route-uid-0001".into(),
                route_revision: 1,
                locality: Locality::Local,
                registered_at: 1000.0,
            });
        });

        let response = handle_peer_sync(State(state)).await.into_response();
        let bytes = to_bytes(response.into_body(), usize::MAX).await.unwrap();
        let envelope: FullSyncEnvelope = serde_json::from_slice(&bytes).unwrap();

        let mut receiver = crate::relay::route_table::RouteTable::new("relay-c".into());
        receiver.merge_validated_snapshot(ValidatedFullSync::try_from(envelope).unwrap());

        let resolved = receiver.resolve("grid");
        assert_eq!(resolved.len(), 1);
        assert_eq!(resolved[0].relay_url, "http://relay-a:8080");
        assert!(resolved[0].ipc_address.is_none());
    }
}
