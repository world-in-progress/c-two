//! Periodic background tasks for relay mesh protocol.
//!
//! Five loops: heartbeat, failure detection, anti-entropy,
//! dead-peer probe, and seed retry. All respect CancellationToken
//! for clean shutdown.

use std::sync::Arc;
use std::time::Duration;

use tokio::task::JoinHandle;
use tokio_util::sync::CancellationToken;

use crate::relay::authority::{RouteAuthority, RouteCommand};
use crate::relay::peer::{
    DigestEntry, PeerEnvelope, PeerMessage, ValidatedDigestDiffEntry, validate_route_state_envelope,
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

    if !config.seed_retry_interval.is_zero() && !config.seeds.is_empty() {
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::relay::route_table::{TombstoneGcEntry, TombstoneGcReason};

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
                });
                let _ = RouteAuthority::new(&state).execute(RouteCommand::RemovePeerRoutes {
                    relay_id: relay_id.clone(),
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
        if let Ok(resp) = client.post(&url).json(&envelope).send().await {
            if let Ok(resp_env) = resp.json::<PeerEnvelope>().await {
                let Ok(validated) = validate_route_state_envelope(resp_env, Some(peer_id.as_str()))
                else {
                    continue;
                };
                let Some(entries) = validated.into_digest_diff_entries() else {
                    continue;
                };
                for diff_entry in entries {
                    apply_digest_diff(&state, peer_id, diff_entry);
                }
            }
        }
    }
}

fn apply_digest_diff(state: &RelayState, peer_id: &str, diff_entry: ValidatedDigestDiffEntry) {
    match diff_entry {
        ValidatedDigestDiffEntry::Active(active) => {
            let entry = active.into();
            let _ = RouteAuthority::new(state).execute(RouteCommand::AnnouncePeer {
                sender_relay_id: peer_id.to_string(),
                entry,
            });
        }
        ValidatedDigestDiffEntry::Deleted(deleted) => {
            let crate::relay::peer::ValidatedDigestDiffDeleted {
                name,
                relay_id,
                removed_at,
            } = deleted;
            let _ = RouteAuthority::new(state).execute(RouteCommand::WithdrawPeer {
                sender_relay_id: peer_id.to_string(),
                name,
                relay_id,
                removed_at,
            });
        }
    }
}

/// Build a digest from the current route table.
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

/// Probe dead peers with health checks; recover and exchange digest on success.
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
                // Peer is back — mark alive
                state.with_route_table_mut(|rt| {
                    if let Some(p) = rt.get_peer_mut(&relay_id) {
                        p.status = PeerStatus::Alive;
                        p.last_heartbeat = std::time::Instant::now();
                    }
                });

                // Immediate digest exchange with recovered peer
                let digest = build_digest(&state);
                let envelope =
                    PeerEnvelope::new(state.relay_id(), PeerMessage::DigestExchange { digest });
                let digest_url = peer_endpoint_url(&url, "/_peer/digest");
                if let Ok(resp) = client.post(&digest_url).json(&envelope).send().await {
                    if let Ok(resp_env) = resp.json::<PeerEnvelope>().await {
                        let Ok(validated) =
                            validate_route_state_envelope(resp_env, Some(relay_id.as_str()))
                        else {
                            continue;
                        };
                        let Some(entries) = validated.into_digest_diff_entries() else {
                            continue;
                        };
                        for diff_entry in entries {
                            apply_digest_diff(&state, &relay_id, diff_entry);
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

    let client = crate::relay_client_builder_with_proxy(state.config().use_proxy)
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
            let join_url = peer_endpoint_url(seed_url, "/_peer/join");
            let envelope = PeerEnvelope::new(
                state.relay_id(),
                PeerMessage::RelayJoin {
                    relay_id: state.relay_id().to_string(),
                    url: state.config().effective_advertise_url(),
                },
            );
            if let Ok(resp) = client.post(&join_url).json(&envelope).send().await {
                if let Ok(envelope) = resp.json::<FullSyncEnvelope>().await {
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
    }
}
