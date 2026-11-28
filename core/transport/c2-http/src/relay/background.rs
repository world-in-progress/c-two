//! Periodic background tasks for relay mesh protocol.
//!
//! Five loops: heartbeat, failure detection, anti-entropy,
//! dead-peer probe, and seed retry. All respect CancellationToken
//! for clean shutdown.

use std::sync::Arc;
use std::time::Duration;

use tokio::task::JoinHandle;
use tokio_util::sync::CancellationToken;

use crate::relay::authority::RouteAuthority;
use crate::relay::peer::{
    DigestEntry, PeerEnvelope, PeerMessage, PeerSender, ValidatedRouteStateEnvelope,
    validate_route_state_envelope,
};
use crate::relay::route_table::TombstoneGcEntry;
use crate::relay::state::RelayState;
use crate::relay::types::*;

/// Spawn all periodic background tasks based on config.
/// Returns handles so the caller can await them on shutdown.
pub fn spawn_background_tasks(
    state: Arc<RelayState>,
    cancel: CancellationToken,
) -> Vec<JoinHandle<()>> {
    let mut handles = Vec::new();
    let config = state.config();

    if !config.heartbeat_interval.is_zero() {
        let s = state.clone();
        let c = cancel.clone();
        handles.push(tokio::spawn(heartbeat_loop(s, c)));
    }

    if !config.heartbeat_interval.is_zero() {
        let s = state.clone();
        let c = cancel.clone();
        handles.push(tokio::spawn(failure_detection_loop(s, c)));
    }

    {
        let s = state.clone();
        let c = cancel.clone();
        handles.push(tokio::spawn(anti_entropy_loop(s, c)));
    }

    if !config.dead_peer_probe_interval.is_zero() {
        let s = state.clone();
        let c = cancel.clone();
        handles.push(tokio::spawn(dead_peer_probe_loop(s, c)));
    }

    if !config.seeds.is_empty() {
        let s = state.clone();
        let c = cancel.clone();
        handles.push(tokio::spawn(seed_retry_loop(s, c)));
    }

    let s = state.clone();
    let c = cancel.clone();
    handles.push(tokio::spawn(tombstone_gc_loop(s, c)));

    handles
}

/// Broadcast heartbeat to all peers at regular intervals.
async fn heartbeat_loop(state: Arc<RelayState>, cancel: CancellationToken) {
    let interval = state.config().heartbeat_interval;
    let mut ticker = tokio::time::interval(interval);
    ticker.tick().await; // skip immediate first tick

    loop {
        tokio::select! {
            _ = cancel.cancelled() => break,
            _ = ticker.tick() => {}
        }

        let route_count = state.local_route_count();
        let envelope = PeerEnvelope::new(
            state.relay_id(),
            PeerMessage::Heartbeat {
                relay_id: state.relay_id().to_string(),
                route_count,
            },
        );
        let peers = state.list_peers();
        state.disseminator().broadcast(envelope, &peers);
    }
}

async fn tombstone_gc_loop(state: Arc<RelayState>, cancel: CancellationToken) {
    let mut ticker = tokio::time::interval(Duration::from_secs(60));
    ticker.tick().await;

    loop {
        tokio::select! {
            _ = cancel.cancelled() => break,
            _ = ticker.tick() => {}
        }

        let retention = tombstone_retention(&state);
        let removed = state.gc_tombstones(retention);
        for tombstone in &removed {
            eprintln!("{}", tombstone_gc_detail_line(tombstone));
        }
        if !removed.is_empty() {
            eprintln!("[relay] GC removed {} route tombstones", removed.len());
        }
    }
}

fn tombstone_gc_detail_line(removed: &TombstoneGcEntry) -> String {
    let tombstone = &removed.tombstone;
    let server_id = tombstone.server_id.as_deref().unwrap_or("<none>");
    format!(
        "[relay] GC removed route tombstone: name={} relay_id={} server_id={} removed_at={} reason={}",
        tombstone.name,
        tombstone.relay_id,
        server_id,
        tombstone.removed_at,
        removed.reason.as_str()
    )
}

fn tombstone_retention(state: &RelayState) -> Duration {
    const MIN_RETENTION: Duration = Duration::from_secs(600);
    let alive_peer_count = state.with_route_table(|rt| rt.alive_peers().len()).max(1);
    let rounds = alive_peer_count.saturating_mul(2).min(u32::MAX as usize) as u32;
    let anti_entropy_window = c2_config::RELAY_ANTI_ENTROPY_INTERVAL
        .checked_mul(rounds)
        .unwrap_or(Duration::MAX);
    MIN_RETENTION.max(anti_entropy_window)
}

/// Detect failed peers: Alive→Suspect→Dead while their routes remain held.
async fn failure_detection_loop(state: Arc<RelayState>, cancel: CancellationToken) {
    let hb = state.config().heartbeat_interval;
    let check_interval = hb * 2;
    let miss_threshold = state.config().heartbeat_miss_threshold;
    let suspect_deadline = hb * miss_threshold;
    let dead_deadline = suspect_deadline * 2;
    let mut ticker = tokio::time::interval(check_interval);
    ticker.tick().await;

    loop {
        tokio::select! {
            _ = cancel.cancelled() => break,
            _ = ticker.tick() => {}
        }

        let peer_ids: Vec<(String, std::time::Instant, PeerStatus)> =
            state.with_route_table(|rt| {
                rt.list_peers()
                    .iter()
                    .map(|p| (p.relay_id.clone(), p.last_heartbeat, p.status))
                    .collect()
            });

        let now = std::time::Instant::now();
        for (relay_id, last_hb, status) in peer_ids {
            let elapsed = now.duration_since(last_hb);
            if status == PeerStatus::Suspect && elapsed > dead_deadline {
                state.with_route_table_mut(|rt| {
                    rt.set_peer_status(&relay_id, PeerStatus::Dead);
                });
            } else if status == PeerStatus::Alive && elapsed > suspect_deadline {
                state.with_route_table_mut(|rt| {
                    rt.set_peer_status(&relay_id, PeerStatus::Suspect);
                });
            }
        }
    }
}

/// Round-robin anti-entropy digest exchange with one alive peer per tick.
async fn anti_entropy_loop(state: Arc<RelayState>, cancel: CancellationToken) {
    let mut ticker = tokio::time::interval(c2_config::RELAY_ANTI_ENTROPY_INTERVAL);
    ticker.tick().await;
    let mut round_robin_idx: usize = 0;

    loop {
        tokio::select! {
            _ = cancel.cancelled() => return,
            _ = ticker.tick() => {}
        }

        let peer = state.with_route_table(|rt| {
            let alive = rt.alive_peers();
            if alive.is_empty() {
                return None;
            }
            let peer = alive[round_robin_idx % alive.len()];
            Some((peer.relay_id.clone(), peer.address.clone()))
        });
        if let Some((peer_id, address)) = peer {
            round_robin_idx = round_robin_idx.wrapping_add(1);
            tokio::select! {
                _ = cancel.cancelled() => return,
                _ = exchange_digest(&state, &peer_id, &address) => {}
            }
        }
    }
}

async fn exchange_digest(state: &RelayState, peer_id: &str, address: &PeerAddress) {
    let digest = state
        .route_digest()
        .into_iter()
        .map(|((name, relay_id, deleted), hash)| DigestEntry {
            name,
            relay_id,
            deleted,
            hash,
        })
        .collect();
    let envelope = PeerEnvelope::new(state.relay_id(), PeerMessage::DigestExchange { digest });
    let Some(response) = state
        .peer_client()
        .exchange_control(address, "/_peer/digest", &envelope)
        .await
    else {
        return;
    };
    let Ok(ValidatedRouteStateEnvelope::DigestDiff(entries)) =
        validate_route_state_envelope(response, Some(peer_id))
    else {
        return;
    };
    let sender = PeerSender {
        relay_id: peer_id,
        endpoint_id: address.endpoint_id(),
    };
    let authority = RouteAuthority::new(state);
    for entry in entries {
        let _ = authority.apply_digest_diff(sender, entry);
    }
}

fn join_envelope(state: &RelayState) -> PeerEnvelope {
    PeerEnvelope::new(
        state.relay_id(),
        PeerMessage::RelayJoin {
            relay_id: state.relay_id().to_string(),
            address: state.local_peer_address(),
        },
    )
}

async fn request_join(state: &RelayState, address: &PeerAddress) -> Option<ValidatedFullSync> {
    let response: FullSyncEnvelope = state
        .peer_client()
        .exchange_control(address, "/_peer/join", &join_envelope(state))
        .await?;
    ValidatedFullSync::try_from(response).ok()
}

/// Re-establish membership with a dead peer before applying its digest.
async fn recover_dead_peer(state: &RelayState, relay_id: &str, address: &PeerAddress) {
    let Some(snapshot) = request_join(state, address).await else {
        return;
    };
    let accepted = snapshot.sender_relay_id() == relay_id
        && snapshot
            .peers()
            .iter()
            .any(|peer| matches_recovered_peer(peer, relay_id, address));
    if accepted && state.with_route_table_mut(|rt| rt.record_peer_probe_success(relay_id, address))
    {
        exchange_digest(state, relay_id, address).await;
    }
}

fn matches_recovered_peer(peer: &PeerSnapshot, relay_id: &str, address: &PeerAddress) -> bool {
    peer.relay_id == relay_id
        && peer.status == PeerStatus::Alive
        && address
            .endpoint_id_str()
            .is_none_or(|endpoint_id| peer.address.endpoint_id_str() == Some(endpoint_id))
}

async fn dead_peer_probe_loop(state: Arc<RelayState>, cancel: CancellationToken) {
    let mut ticker = tokio::time::interval(state.config().dead_peer_probe_interval);
    ticker.tick().await;
    loop {
        tokio::select! {
            _ = cancel.cancelled() => return,
            _ = ticker.tick() => {}
        }

        let dead: Vec<_> = state.with_route_table(|rt| {
            rt.dead_peers()
                .iter()
                .map(|peer| (peer.relay_id.clone(), peer.address.clone()))
                .collect()
        });
        for (relay_id, address) in dead {
            tokio::select! {
                _ = cancel.cancelled() => return,
                _ = recover_dead_peer(&state, &relay_id, &address) => {}
            }
        }
    }
}

fn seed_address(seed: &str, bridge_url: Option<&str>) -> Option<PeerAddress> {
    let address = if seed.starts_with("iroh://") {
        let seed = c2_config::IrohSeedAddr::parse(seed).ok()?;
        PeerAddress {
            http_url: None,
            iroh: Some(IrohPeerAddr {
                endpoint_id: hex::encode(seed.endpoint_id),
                bridge_url: bridge_url.map(ToOwned::to_owned),
                direct_addrs: seed.direct_addr.iter().map(ToString::to_string).collect(),
            }),
        }
    } else {
        PeerAddress::http(seed)
    };
    address.is_valid().then_some(address)
}

async fn join_seed(state: &RelayState, address: &PeerAddress) -> bool {
    let Some(snapshot) = request_join(state, address).await else {
        return false;
    };
    let sender_relay_id = snapshot.sender_relay_id();
    if sender_relay_id == state.relay_id()
        || !snapshot.peers().iter().any(|peer| {
            peer.status == PeerStatus::Alive
                && peer.relay_id == sender_relay_id
                && peer.address.endpoint_id_str() == address.endpoint_id_str()
        })
        || !state.merge_snapshot(snapshot)
    {
        return false;
    }

    state
        .disseminator()
        .broadcast(join_envelope(state), &state.list_peers());
    true
}

/// Join seeds immediately, then retry while no alive peers exist.
async fn seed_retry_loop(state: Arc<RelayState>, cancel: CancellationToken) {
    let interval = state.config().seed_retry_interval;
    let local_address = state.local_peer_address();
    let bridge_url = local_address
        .iroh
        .as_ref()
        .and_then(|address| address.bridge_url.as_deref());
    let seeds: Vec<_> = state
        .config()
        .seeds
        .iter()
        .filter_map(|seed| seed_address(seed, bridge_url))
        .collect();

    loop {
        let has_alive = state.with_route_table(|rt| !rt.alive_peers().is_empty());
        if !has_alive {
            for address in &seeds {
                let joined = tokio::select! {
                    _ = cancel.cancelled() => return,
                    result = join_seed(&state, address) => result,
                };
                if joined {
                    break;
                }
            }
        }
        if interval.is_zero() {
            return;
        }
        tokio::select! {
            _ = cancel.cancelled() => return,
            _ = tokio::time::sleep(interval) => {}
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::relay::peer::PROTOCOL_VERSION;
    use crate::relay::route_table::{TombstoneGcEntry, TombstoneGcReason};

    #[test]
    fn dead_probe_rejects_a_different_iroh_endpoint() {
        let address = |key: u8| PeerAddress {
            http_url: None,
            iroh: Some(IrohPeerAddr {
                endpoint_id: hex::encode(
                    iroh::SecretKey::from_bytes(&[key; 32]).public().as_bytes(),
                ),
                bridge_url: None,
                direct_addrs: Vec::new(),
            }),
        };
        let original = address(1);
        let mut peer = PeerSnapshot {
            relay_id: "target".into(),
            address: address(2),
            route_count: 0,
            status: PeerStatus::Alive,
        };
        assert!(!matches_recovered_peer(&peer, "target", &original));
        peer.address = original.clone();
        assert!(matches_recovered_peer(&peer, "target", &original));
        peer.status = PeerStatus::Dead;
        assert!(!matches_recovered_peer(&peer, "target", &original));
    }

    #[tokio::test]
    async fn dead_probe_requires_200_with_current_snapshot_and_target_relay() {
        use axum::{Json, Router, http::StatusCode, routing::post};

        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = PeerAddress::http(format!("http://{}", listener.local_addr().unwrap()));
        let target = "relay-probe-target";
        let snapshot = |version, sender_relay_id: &str, peer_relay_id: &str| {
            serde_json::json!({
                "protocol_version": version,
                "sender_relay_id": sender_relay_id,
                "snapshot": {
                    "routes": [],
                    "peers": [{
                        "relay_id": peer_relay_id, "address": address, "status": "Alive", "route_count": 0
                    }]
                }
            })
        };
        let replies = Arc::new(parking_lot::Mutex::new(std::collections::VecDeque::from([
            (StatusCode::OK, serde_json::json!({"status": "ignored"})),
            (
                StatusCode::CREATED,
                snapshot(PROTOCOL_VERSION, target, target),
            ),
            (
                StatusCode::OK,
                snapshot(PROTOCOL_VERSION - 1, target, target),
            ),
            (
                StatusCode::OK,
                snapshot(PROTOCOL_VERSION, "relay-someone-else", "relay-someone-else"),
            ),
            // Impostor at the old address whose snapshot still lists target as Alive.
            (
                StatusCode::OK,
                snapshot(PROTOCOL_VERSION, "relay-impostor", target),
            ),
            (StatusCode::OK, {
                let mut body = snapshot(PROTOCOL_VERSION, target, target);
                body["snapshot"]["peers"][0]["status"] = serde_json::json!("Dead");
                body
            }),
        ])));
        let app = Router::new().route(
            "/_peer/join",
            post({
                let replies = replies.clone();
                move || async move {
                    let (status, body) = replies
                        .lock()
                        .pop_front()
                        .expect("one response per attempt");
                    (status, Json(body))
                }
            }),
        );
        let server = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        let state = crate::relay::test_support::test_state_for_client();
        state.register_peer(PeerInfo {
            relay_id: target.into(),
            address: address.clone(),
            route_count: 3,
            last_heartbeat: std::time::Instant::now(),
            status: PeerStatus::Dead,
        });
        for _ in 0..6 {
            recover_dead_peer(&state, target, &address).await;
            assert_eq!(state.list_peers()[0].status, PeerStatus::Dead);
        }
        assert!(replies.lock().is_empty());
        server.abort();
        assert!(server.await.unwrap_err().is_cancelled());
    }

    #[tokio::test]
    async fn seed_bootstrap_runs_once_when_retries_are_disabled() {
        use std::sync::atomic::{AtomicUsize, Ordering};

        let joins = Arc::new(AtomicUsize::new(0));
        let received = joins.clone();
        let app = axum::Router::new().route(
            "/_peer/join",
            axum::routing::post(move || {
                let received = received.clone();
                async move {
                    received.fetch_add(1, Ordering::SeqCst);
                    axum::Json(serde_json::json!({"status": "ignored"}))
                }
            }),
        );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let seed = format!("http://{}", listener.local_addr().unwrap());
        let server = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        let state = Arc::new(RelayState::new(
            Arc::new(c2_config::RelayConfig {
                seeds: vec![seed],
                heartbeat_interval: Duration::ZERO,
                dead_peer_probe_interval: Duration::ZERO,
                seed_retry_interval: Duration::ZERO,
                ..Default::default()
            }),
            Arc::new(crate::relay::test_support::NoopDisseminator),
        ));
        let cancel = CancellationToken::new();
        let handles = spawn_background_tasks(state, cancel.clone());
        // Anti-entropy is always spawned (fixed 60s interval, first real tick
        // after a full interval), so the one-shot seed join is observed by its
        // effect rather than by task-list position.
        tokio::time::timeout(Duration::from_secs(2), async {
            while joins.load(Ordering::SeqCst) < 1 {
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("seed bootstrap must join exactly once");
        assert_eq!(joins.load(Ordering::SeqCst), 1);
        cancel.cancel();
        for handle in handles {
            handle.await.unwrap();
        }
        server.abort();
        let _ = server.await;
    }

    #[test]
    fn tombstone_gc_log_message_names_removed_route() {
        let removed = TombstoneGcEntry {
            tombstone: RouteTombstone {
                name: "grid".into(),
                relay_id: "relay-a".into(),
                removed_at: 2000.0,
                server_id: Some("server-grid".into()),
                observed_at: std::time::Instant::now(),
            },
            reason: TombstoneGcReason::RetentionExpired,
        };

        let line = tombstone_gc_detail_line(&removed);

        assert!(line.contains("[relay] GC removed route tombstone:"));
        assert!(line.contains("name=grid"));
        assert!(line.contains("relay_id=relay-a"));
        assert!(line.contains("server_id=server-grid"));
        assert!(line.contains("removed_at=2000"));
        assert!(line.contains("reason=retention-expired"));
    }
    #[tokio::test]
    async fn failure_detection_keeps_dead_peer_routes_and_digest() {
        // ADR 0004: "联系不上不等于已经没了。"
        let state = Arc::new(RelayState::new(
            Arc::new(c2_config::RelayConfig {
                relay_id: "relay-local".into(),
                heartbeat_interval: Duration::from_millis(5),
                heartbeat_miss_threshold: 1,
                ..Default::default()
            }),
            Arc::new(crate::relay::test_support::NoopDisseminator),
        ));
        state.register_peer(crate::relay::types::PeerInfo {
            relay_id: "relay-peer".into(),
            address: PeerAddress::http("http://relay-peer:8080"),
            route_count: 1,
            last_heartbeat: std::time::Instant::now() - Duration::from_secs(1),
            status: PeerStatus::Alive,
        });
        state.with_route_table_mut(|table| {
            assert!(table.register_route(crate::relay::types::RouteEntry {
                name: "grid".into(),
                relay_id: "relay-peer".into(),
                address: PeerAddress::http("http://relay-peer:8080"),
                server_id: None,
                server_instance_id: None,
                ipc_address: None,
                crm_ns: "test.echo".into(),
                crm_name: "Echo".into(),
                crm_ver: "0.1.0".into(),
                abi_hash: crate::relay::test_support::TEST_ABI_HASH.into(),
                signature_hash: crate::relay::test_support::TEST_SIGNATURE_HASH.into(),
                max_payload_size: 1024,
                route_uid: None,
                locality: crate::relay::types::Locality::Peer,
                registered_at: 1000.0,
            }))
        });
        let cancel = CancellationToken::new();
        let task = tokio::spawn(failure_detection_loop(state.clone(), cancel.clone()));
        tokio::time::timeout(Duration::from_secs(2), async {
            while !state.with_route_table(|table| {
                table.get_peer("relay-peer").unwrap().status == PeerStatus::Dead
            }) {
                tokio::task::yield_now().await;
            }
        })
        .await
        .unwrap();
        assert_eq!(state.list_routes().len(), 1);
        assert!(
            state
                .route_digest()
                .contains_key(&("grid".into(), "relay-peer".into(), false))
        );
        cancel.cancel();
        task.await.unwrap();
    }
}
