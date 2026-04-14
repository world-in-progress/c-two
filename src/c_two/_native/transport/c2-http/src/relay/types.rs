use serde::{Deserialize, Serialize};
use std::time::Instant;

/// Locality of a route entry relative to this relay.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum Locality {
    Local,
    Peer,
}

/// A single CRM route entry in the RouteTable.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RouteEntry {
    pub name: String,
    pub relay_id: String,
    pub relay_url: String,
    pub ipc_address: Option<String>,
    pub icrm_ns: String,
    pub icrm_ver: String,
    pub locality: Locality,
    pub registered_at: f64,
}

/// Resolution result returned to clients via /_resolve.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RouteInfo {
    pub name: String,
    pub relay_url: String,
    pub ipc_address: Option<String>,
    pub icrm_ns: String,
    pub icrm_ver: String,
}

impl RouteEntry {
    pub fn to_route_info(&self) -> RouteInfo {
        RouteInfo {
            name: self.name.clone(),
            relay_url: self.relay_url.clone(),
            ipc_address: self.ipc_address.clone(),
            icrm_ns: self.icrm_ns.clone(),
            icrm_ver: self.icrm_ver.clone(),
        }
    }
}

/// Peer relay status.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum PeerStatus {
    Alive,
    Suspect,
    Dead,
}

/// Information about a peer relay.
#[derive(Debug, Clone)]
pub struct PeerInfo {
    pub relay_id: String,
    pub url: String,
    pub route_count: u32,
    pub last_heartbeat: Instant,
    pub status: PeerStatus,
}

/// Full sync snapshot for join protocol.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FullSync {
    pub routes: Vec<RouteEntry>,
    pub peers: Vec<PeerSnapshot>,
}

/// Serializable peer info (Instant is not serializable).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PeerSnapshot {
    pub relay_id: String,
    pub url: String,
    pub route_count: u32,
    pub status: PeerStatus,
}

/// Convert a DigestDiffEntry into a PEER RouteEntry.
impl From<crate::relay::peer::DigestDiffEntry> for RouteEntry {
    fn from(d: crate::relay::peer::DigestDiffEntry) -> Self {
        Self {
            name: d.name, relay_id: d.relay_id, relay_url: d.relay_url,
            ipc_address: d.ipc_address, icrm_ns: d.icrm_ns, icrm_ver: d.icrm_ver,
            locality: Locality::Peer, registered_at: d.registered_at,
        }
    }
}
