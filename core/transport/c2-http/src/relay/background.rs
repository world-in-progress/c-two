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
    DigestEntry, PeerEnvelope, PeerMessage, ValidatedRouteStateEnvelope,
    validate_route_state_envelope,
};
use crate::relay::route_table::TombstoneGcEntry;
use crate::relay::state::RelayState;
use crate::relay::types::*;
use crate::relay::url::peer_endpoint_url;

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

    if !config.anti_entropy_interval.is_zero() {
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
    let anti_entropy_window = state
        .config()
        .anti_entropy_interval
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
    let interval = state.config().anti_entropy_interval;
    let mut ticker = tokio::time::interval(interval);
    ticker.tick().await;
    let mut round_robin_idx: usize = 0;

    let client = crate::relay_client_builder_with_proxy(state.config().use_proxy)
        .timeout(Duration::from_secs(5))
        .build()
        .expect("c-two: failed to build reqwest Client for relay traffic");

    loop {
        tokio::select! {
            _ = cancel.cancelled() => break,
            _ = ticker.tick() => {}
        }

        let alive: Vec<(String, String)> = state.with_route_table(|rt| {
            rt.alive_peers()
                .iter()
                .map(|p| (p.relay_id.clone(), p.url.clone()))
                .collect()
        });
        if alive.is_empty() {
            continue;
        }

        let idx = round_robin_idx % alive.len();
        round_robin_idx = round_robin_idx.wrapping_add(1);
        let (peer_id, peer_url) = &alive[idx];

        let digest = build_digest(&state);
        let envelope = PeerEnvelope::new(state.relay_id(), PeerMessage::DigestExchange { digest });

        let url = peer_endpoint_url(peer_url, "/_peer/digest");
        if let Ok(resp) = client.post(&url).json(&envelope).send().await
            && let Ok(resp_env) = resp.json::<PeerEnvelope>().await
        {
            let Ok(ValidatedRouteStateEnvelope::DigestDiff(entries)) =
                validate_route_state_envelope(resp_env, Some(peer_id.as_str()))
            else {
                continue;
            };
            for diff_entry in entries {
                let _ = RouteAuthority::new(&state).apply_digest_diff(peer_id, diff_entry);
            }
        }
    }
}

fn build_digest(state: &RelayState) -> Vec<DigestEntry> {
    state
        .route_digest()
        .into_iter()
        .map(|((name, relay_id, deleted), hash)| DigestEntry {
            name,
            relay_id,
            deleted,
            hash,
        })
        .collect()
}

/// Probe dead peers with health checks; rejoin and exchange digest on recovery.
async fn dead_peer_probe_loop(state: Arc<RelayState>, cancel: CancellationToken) {
    let interval = state.config().dead_peer_probe_interval;
    let mut ticker = tokio::time::interval(interval);
    ticker.tick().await;

    let client = crate::relay_client_builder_with_proxy(state.config().use_proxy)
        .timeout(Duration::from_secs(5))
        .build()
        .expect("c-two: failed to build reqwest Client for relay traffic");

    loop {
        tokio::select! {
            _ = cancel.cancelled() => break,
            _ = ticker.tick() => {}
        }

        let dead: Vec<(String, String)> = state.with_route_table(|rt| {
            rt.dead_peers()
                .iter()
                .map(|p| (p.relay_id.clone(), p.url.clone()))
                .collect()
        });

        for (relay_id, url) in dead {
            let health_url = peer_endpoint_url(&url, "/health");
            let probe_ok = match client.get(&health_url).send().await {
                Ok(resp) => resp.status().is_success(),
                Err(_) => false,
            };
            if probe_ok {
                // The peer may have forgotten us through force-leave. Rejoin
                // before exchanging state; its snapshot is for seed bootstrap
                // only and must not be merged on this recovery path.
                let join = PeerEnvelope::new(
                    state.relay_id(),
                    PeerMessage::RelayJoin {
                        relay_id: state.relay_id().to_string(),
                        url: state.config().effective_advertise_url(),
                    },
                );
                let join_url = peer_endpoint_url(&url, "/_peer/join");
                if !matches!(client.post(&join_url).json(&join).send().await,
                    Ok(response) if response.status().is_success())
                {
                    continue;
                }
                // Peer is back — mark alive
                state.with_route_table_mut(|rt| {
                    rt.record_peer_probe_success(&relay_id);
                });

                // Immediate digest exchange with recovered peer
                let digest = build_digest(&state);
                let envelope =
                    PeerEnvelope::new(state.relay_id(), PeerMessage::DigestExchange { digest });
                let digest_url = peer_endpoint_url(&url, "/_peer/digest");
                if let Ok(resp) = client.post(&digest_url).json(&envelope).send().await
                    && let Ok(resp_env) = resp.json::<PeerEnvelope>().await
                {
                    let Ok(ValidatedRouteStateEnvelope::DigestDiff(entries)) =
                        validate_route_state_envelope(resp_env, Some(relay_id.as_str()))
                    else {
                        continue;
                    };
                    for diff_entry in entries {
                        let _ =
                            RouteAuthority::new(&state).apply_digest_diff(&relay_id, diff_entry);
                    }
                }
            }
        }
    }
}

/// Join seeds immediately, then retry while no alive peers exist.
async fn seed_retry_loop(state: Arc<RelayState>, cancel: CancellationToken) {
    let interval = state.config().seed_retry_interval;
    let seeds = state.config().seeds.clone();
    let client = crate::relay_client_builder_with_proxy(state.config().use_proxy)
        .timeout(Duration::from_secs(5))
        .build()
        .expect("c-two: failed to build reqwest Client for relay traffic");

    loop {
        // The first join and later retries use the same path.
        let has_alive = state.with_route_table(|rt| !rt.alive_peers().is_empty());
        if !has_alive {
            for seed_url in &seeds {
                let join_url = peer_endpoint_url(seed_url, "/_peer/join");
                let envelope = PeerEnvelope::new(
                    state.relay_id(),
                    PeerMessage::RelayJoin {
                        relay_id: state.relay_id().to_string(),
                        url: state.config().effective_advertise_url(),
                    },
                );
                if let Ok(resp) = client.post(&join_url).json(&envelope).send().await
                    && let Ok(envelope) = resp.json::<FullSyncEnvelope>().await
                {
                    let Ok(snapshot) = ValidatedFullSync::try_from(envelope) else {
                        continue;
                    };
                    state.merge_snapshot(snapshot);
                    let peers = state.list_peers();
                    let announce = PeerEnvelope::new(
                        state.relay_id(),
                        PeerMessage::RelayJoin {
                            relay_id: state.relay_id().to_string(),
                            url: state.config().effective_advertise_url(),
                        },
                    );
                    state.disseminator().broadcast(announce, &peers);
                    break;
                }
            }
        }
        if interval.is_zero() {
            break;
        }
        tokio::select! {
            _ = cancel.cancelled() => break,
            _ = tokio::time::sleep(interval) => {}
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::relay::route_table::{TombstoneGcEntry, TombstoneGcReason};

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
                anti_entropy_interval: Duration::ZERO,
                dead_peer_probe_interval: Duration::ZERO,
                seed_retry_interval: Duration::ZERO,
                ..Default::default()
            }),
            Arc::new(crate::relay::test_support::NoopDisseminator),
        ));
        let cancel = CancellationToken::new();
        let mut handles = spawn_background_tasks(state, cancel.clone());
        tokio::time::timeout(Duration::from_secs(2), handles.remove(0))
            .await
            .unwrap()
            .unwrap();
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
            url: "http://relay-peer:8080".into(),
            route_count: 1,
            last_heartbeat: std::time::Instant::now() - Duration::from_secs(1),
            status: PeerStatus::Alive,
        });
        state.with_route_table_mut(|table| {
            assert!(table.register_route(crate::relay::types::RouteEntry {
                name: "grid".into(),
                relay_id: "relay-peer".into(),
                relay_url: "http://relay-peer:8080".into(),
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
