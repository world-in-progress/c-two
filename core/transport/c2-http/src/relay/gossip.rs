use std::sync::Arc;

use crate::relay::peer::{PeerEnvelope, PeerMessage};
use crate::relay::state::RelayState;
use crate::relay::types::RouteEntry;

pub(crate) fn broadcast_force_leave(state: &Arc<RelayState>, relay_id: String) {
    let envelope = PeerEnvelope::new(state.relay_id(), PeerMessage::RelayForceLeave { relay_id });
    state
        .disseminator()
        .broadcast(envelope, &state.list_peers());
}

pub(crate) fn broadcast_route_announce(state: &Arc<RelayState>, entry: &RouteEntry) {
    let envelope = PeerEnvelope::new(
        state.relay_id(),
        PeerMessage::RouteAnnounce {
            name: entry.name.clone(),
            relay_id: entry.relay_id.clone(),
            relay_url: entry.relay_url.clone(),
            crm_ns: entry.crm_ns.clone(),
            crm_name: entry.crm_name.clone(),
            crm_ver: entry.crm_ver.clone(),
            abi_hash: entry.abi_hash.clone(),
            signature_hash: entry.signature_hash.clone(),
            max_payload_size: entry.max_payload_size,
            route_uid: entry.route_uid.clone(),
            registered_at: entry.registered_at,
        },
    );
    let peers = state.list_peers();
    state.disseminator().broadcast(envelope, &peers);
}

pub(crate) fn broadcast_route_withdraw(
    state: &Arc<RelayState>,
    entry: &RouteEntry,
    removed_at: f64,
) {
    let envelope = PeerEnvelope::new(
        state.relay_id(),
        PeerMessage::RouteWithdraw {
            name: entry.name.clone(),
            relay_id: entry.relay_id.clone(),
            removed_at,
        },
    );
    let peers = state.list_peers();
    state.disseminator().broadcast(envelope, &peers);
}
