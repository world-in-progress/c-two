//! Periodic background tasks for relay mesh protocol.
//!
//! Five loops: heartbeat, failure detection, anti-entropy,
//! dead-peer probe, and seed retry. All respect CancellationToken
//! for clean shutdown.

use std::sync::Arc;
use std::time::Duration;

use tokio::task::JoinHandle;
use tokio_util::sync::CancellationToken;

use crate::relay::peer::{DigestEntry, PeerEnvelope, PeerMessage};
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

    if !config.seed_retry_interval.is_zero() && !config.seeds.is_empty() {
        let s = state.clone();
        let c = cancel.clone();
        handles.push(tokio::spawn(seed_retry_loop(s, c)));
    }

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

/// Detect failed peers: Alive→Suspect→Dead with route removal.
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
                    if let Some(p) = rt.get_peer_mut(&relay_id) {
                        p.status = PeerStatus::Dead;
                    }
                    rt.remove_routes_by_relay(&relay_id);
                });
            } else if status == PeerStatus::Alive && elapsed > suspect_deadline {
                state.with_route_table_mut(|rt| {
                    if let Some(p) = rt.get_peer_mut(&relay_id) {
                        p.status = PeerStatus::Suspect;
                    }
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

    let client = crate::relay_client_builder()
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
        let (_peer_id, peer_url) = &alive[idx];

        let digest = build_digest(&state);
        let envelope = PeerEnvelope::new(
            state.relay_id(),
            PeerMessage::DigestExchange { digest },
        );

        let url = format!("{peer_url}/_peer/digest");
        if let Ok(resp) = client.post(&url).json(&envelope).send().await {
            if let Ok(resp_env) = resp.json::<PeerEnvelope>().await {
                if let PeerMessage::DigestDiff { entries, extra: _ } = resp_env.message {
                    for diff_entry in entries {
                        // Defense-in-depth: scrub ipc_address from incoming
                        // peer routes — the path is local to the sender.
                        let mut entry: crate::relay::types::RouteEntry =
                            diff_entry.into();
                        entry.ipc_address = None;
                        state.register_peer_route(entry);
                    }
                }
            }
        }
    }
}

/// Build a digest from the current route table.
fn build_digest(state: &RelayState) -> Vec<DigestEntry> {
    state
        .route_digest()
        .into_iter()
        .map(|((name, relay_id), hash)| DigestEntry {
            name,
            relay_id,
            hash,
        })
        .collect()
}

/// Probe dead peers with health checks; recover and exchange digest on success.
async fn dead_peer_probe_loop(state: Arc<RelayState>, cancel: CancellationToken) {
    let interval = state.config().dead_peer_probe_interval;
    let mut ticker = tokio::time::interval(interval);
    ticker.tick().await;

    let client = crate::relay_client_builder()
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
            let health_url = format!("{url}/health");
            let probe_ok = match client.get(&health_url).send().await {
                Ok(resp) => resp.status().is_success(),
                Err(_) => false,
            };
            if probe_ok {
                // Peer is back — mark alive
                state.with_route_table_mut(|rt| {
                    if let Some(p) = rt.get_peer_mut(&relay_id) {
                        p.status = PeerStatus::Alive;
                        p.last_heartbeat = std::time::Instant::now();
                    }
                });

                // Immediate digest exchange with recovered peer
                let digest = build_digest(&state);
                let envelope = PeerEnvelope::new(
                    state.relay_id(),
                    PeerMessage::DigestExchange { digest },
                );
                let digest_url = format!("{url}/_peer/digest");
                if let Ok(resp) = client.post(&digest_url).json(&envelope).send().await {
                    if let Ok(resp_env) = resp.json::<PeerEnvelope>().await {
                        if let PeerMessage::DigestDiff { entries, extra: _ } = resp_env.message {
                            for diff_entry in entries {
                                let mut entry: crate::relay::types::RouteEntry =
                                    diff_entry.into();
                                entry.ipc_address = None;
                                state.register_peer_route(entry);
                            }
                        }
                    }
                }
            }
        }
    }
}

/// Retry seed nodes when no alive peers exist.
async fn seed_retry_loop(state: Arc<RelayState>, cancel: CancellationToken) {
    let interval = state.config().seed_retry_interval;
    let seeds = state.config().seeds.clone();
    let mut ticker = tokio::time::interval(interval);
    ticker.tick().await;

    let client = crate::relay_client_builder()
        .timeout(Duration::from_secs(5))
            .build()
        .expect("c-two: failed to build reqwest Client for relay traffic");

    loop {
        tokio::select! {
            _ = cancel.cancelled() => break,
            _ = ticker.tick() => {}
        }

        // Only retry if no alive peers
        let has_alive = state.with_route_table(|rt| !rt.alive_peers().is_empty());
        if has_alive {
            continue;
        }

        for seed_url in &seeds {
            let join_url = format!("{seed_url}/_peer/join");
            let envelope = PeerEnvelope::new(
                state.relay_id(),
                PeerMessage::RelayJoin {
                    relay_id: state.relay_id().to_string(),
                    url: state.config().effective_advertise_url(),
                },
            );
            if let Ok(resp) = client.post(&join_url).json(&envelope).send().await {
                if let Ok(snapshot) = resp.json::<FullSync>().await {
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
    }
}
