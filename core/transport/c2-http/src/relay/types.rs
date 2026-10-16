use serde::{Deserialize, Serialize};
use std::fmt;
use std::time::Instant;

use crate::relay::peer::PROTOCOL_VERSION;

/// Locality of a route entry relative to this relay.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum Locality {
    Local,
    Peer,
}

/// A single CRM route entry in the RouteTable.
#[derive(Debug, Clone)]
pub struct RouteEntry {
    pub name: String,
    pub relay_id: String,
    pub relay_url: String,
    pub server_id: Option<String>,
    pub server_instance_id: Option<String>,
    pub ipc_address: Option<String>,
    pub crm_ns: String,
    pub crm_name: String,
    pub crm_ver: String,
    pub abi_hash: String,
    pub signature_hash: String,
    pub max_payload_size: u64,
    pub route_uid: String,
    pub route_revision: u64,
    pub locality: Locality,
    pub registered_at: f64,
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub(crate) struct UpstreamEndpointKey {
    address: String,
    server_id: String,
    server_instance_id: String,
}

impl UpstreamEndpointKey {
    pub(crate) fn new(
        address: impl Into<String>,
        server_id: impl Into<String>,
        server_instance_id: impl Into<String>,
    ) -> Self {
        Self {
            address: address.into(),
            server_id: server_id.into(),
            server_instance_id: server_instance_id.into(),
        }
    }

    pub(crate) fn from_route(entry: &RouteEntry) -> Option<Self> {
        if entry.locality != Locality::Local {
            return None;
        }
        Some(Self::new(
            entry.ipc_address.clone()?,
            entry.server_id.clone()?,
            entry.server_instance_id.clone()?,
        ))
    }

    pub(crate) fn address(&self) -> &str {
        &self.address
    }

    pub(crate) fn server_id(&self) -> &str {
        &self.server_id
    }

    pub(crate) fn server_instance_id(&self) -> &str {
        &self.server_instance_id
    }
}

impl fmt::Display for UpstreamEndpointKey {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "server_id={} server_instance_id={} address={}",
            self.server_id, self.server_instance_id, self.address
        )
    }
}

fn observed_now() -> Instant {
    Instant::now()
}

/// Negative route state used to propagate deletions through gossip and
/// anti-entropy without letting old announcements resurrect stale routes.
#[derive(Debug, Clone)]
pub struct RouteTombstone {
    pub name: String,
    pub relay_id: String,
    pub removed_at: f64,
    /// Delete-event revision assigned by the owner relay.
    pub removed_revision: u64,
    /// Local route-table revision assigned when this relay applies the tombstone.
    pub local_catalog_revision: u64,
    pub server_id: Option<String>,
    pub observed_at: Instant,
}

/// Resolution result returned to clients via /_resolve.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RouteInfo {
    pub name: String,
    pub relay_url: String,
    pub ipc_address: Option<String>,
    pub server_id: Option<String>,
    pub server_instance_id: Option<String>,
    pub crm_ns: String,
    pub crm_name: String,
    pub crm_ver: String,
    pub abi_hash: String,
    pub signature_hash: String,
    pub max_payload_size: u64,
    pub route_uid: String,
    pub route_revision: u64,
}

impl RouteEntry {
    pub fn to_route_info(&self) -> RouteInfo {
        // `ipc_address` is private to the owning relay's filesystem; only
        // expose it for LOCAL routes. PEER routes go through `relay_url`.
        let local = self.locality == Locality::Local;
        let ipc_address = if local {
            self.ipc_address.clone()
        } else {
            None
        };
        let server_id = if local { self.server_id.clone() } else { None };
        let server_instance_id = if local {
            self.server_instance_id.clone()
        } else {
            None
        };
        RouteInfo {
            name: self.name.clone(),
            relay_url: self.relay_url.clone(),
            ipc_address,
            server_id,
            server_instance_id,
            crm_ns: self.crm_ns.clone(),
            crm_name: self.crm_name.clone(),
            crm_ver: self.crm_ver.clone(),
            abi_hash: self.abi_hash.clone(),
            signature_hash: self.signature_hash.clone(),
            max_payload_size: self.max_payload_size,
            route_uid: self.route_uid.clone(),
            route_revision: self.route_revision,
        }
    }
}

pub(crate) fn local_route_matches(entry: &RouteEntry, expected: &RouteEntry) -> bool {
    entry.name == expected.name
        && entry.relay_id == expected.relay_id
        && entry.server_id == expected.server_id
        && entry.server_instance_id == expected.server_instance_id
        && entry.ipc_address == expected.ipc_address
        && entry.crm_ns == expected.crm_ns
        && entry.crm_name == expected.crm_name
        && entry.crm_ver == expected.crm_ver
        && entry.abi_hash == expected.abi_hash
        && entry.signature_hash == expected.signature_hash
        && entry.max_payload_size == expected.max_payload_size
        && entry.route_uid == expected.route_uid
        && entry.route_revision == expected.route_revision
        && entry.locality == Locality::Local
        && expected.locality == Locality::Local
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
#[derive(Debug, Clone)]
pub struct FullSync {
    pub routes: Vec<RouteEntry>,
    pub tombstones: Vec<RouteTombstone>,
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

pub const ROUTE_HASH_FULL_SYNC_VERSION: u32 = 6;
pub type RouteDigestHash = String;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FullSyncEnvelope {
    pub protocol_version: u32,
    pub snapshot: FullSyncSnapshot,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FullSyncSnapshot {
    pub routes: Vec<FullSyncRoute>,
    #[serde(default)]
    pub tombstones: Vec<FullSyncTombstone>,
    pub peers: Vec<PeerSnapshot>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FullSyncRoute {
    pub name: String,
    pub relay_id: String,
    pub relay_url: String,
    pub crm_ns: String,
    pub crm_name: String,
    pub crm_ver: String,
    pub abi_hash: String,
    pub signature_hash: String,
    pub max_payload_size: u64,
    pub route_uid: String,
    pub route_revision: u64,
    pub registered_at: f64,
    pub hash: RouteDigestHash,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FullSyncTombstone {
    pub name: String,
    pub relay_id: String,
    pub removed_at: f64,
    pub removed_revision: u64,
    pub hash: RouteDigestHash,
}

#[derive(Debug, Clone)]
pub struct ValidatedFullSync(FullSync);

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FullSyncValidationError {
    RouteSnapshotTooOld { got: u32, min: u32 },
    UnsupportedRouteSnapshotVersion { got: u32, supported: u32 },
    InvalidPeer { relay_id: String },
    InvalidRoute { name: String, relay_id: String },
    InvalidTombstone { name: String, relay_id: String },
    InvalidRouteHash { name: String, relay_id: String },
    InvalidTombstoneHash { name: String, relay_id: String },
}

impl FullSyncSnapshot {
    pub fn from_internal(sync: FullSync) -> Self {
        Self {
            routes: sync
                .routes
                .into_iter()
                .map(|route| FullSyncRoute::from_route_entry(&route))
                .collect(),
            tombstones: sync
                .tombstones
                .into_iter()
                .map(|tombstone| FullSyncTombstone::from_tombstone(&tombstone))
                .collect(),
            peers: sync.peers,
        }
    }
}

impl FullSyncRoute {
    fn from_route_entry(entry: &RouteEntry) -> Self {
        Self {
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
            route_revision: entry.route_revision,
            registered_at: entry.registered_at,
            hash: route_entry_digest_hash(entry),
        }
    }

    pub(crate) fn to_peer_route_entry(&self) -> RouteEntry {
        RouteEntry {
            name: self.name.clone(),
            relay_id: self.relay_id.clone(),
            relay_url: self.relay_url.clone(),
            server_id: None,
            server_instance_id: None,
            ipc_address: None,
            crm_ns: self.crm_ns.clone(),
            crm_name: self.crm_name.clone(),
            crm_ver: self.crm_ver.clone(),
            abi_hash: self.abi_hash.clone(),
            signature_hash: self.signature_hash.clone(),
            max_payload_size: self.max_payload_size,
            route_uid: self.route_uid.clone(),
            route_revision: self.route_revision,
            locality: Locality::Peer,
            registered_at: self.registered_at,
        }
    }
}

impl FullSyncTombstone {
    fn from_tombstone(tombstone: &RouteTombstone) -> Self {
        Self {
            name: tombstone.name.clone(),
            relay_id: tombstone.relay_id.clone(),
            removed_at: tombstone.removed_at,
            removed_revision: tombstone.removed_revision,
            hash: tombstone_digest_hash(tombstone),
        }
    }

    pub(crate) fn to_tombstone(&self) -> RouteTombstone {
        RouteTombstone {
            name: self.name.clone(),
            relay_id: self.relay_id.clone(),
            removed_at: self.removed_at,
            removed_revision: self.removed_revision,
            local_catalog_revision: 0,
            server_id: None,
            observed_at: observed_now(),
        }
    }
}

impl ValidatedFullSync {
    pub(crate) fn into_inner(self) -> FullSync {
        self.0
    }
}

impl TryFrom<FullSyncEnvelope> for ValidatedFullSync {
    type Error = FullSyncValidationError;

    fn try_from(envelope: FullSyncEnvelope) -> Result<Self, Self::Error> {
        if envelope.protocol_version < ROUTE_HASH_FULL_SYNC_VERSION {
            return Err(FullSyncValidationError::RouteSnapshotTooOld {
                got: envelope.protocol_version,
                min: ROUTE_HASH_FULL_SYNC_VERSION,
            });
        }
        if envelope.protocol_version != PROTOCOL_VERSION {
            return Err(FullSyncValidationError::UnsupportedRouteSnapshotVersion {
                got: envelope.protocol_version,
                supported: PROTOCOL_VERSION,
            });
        }

        for peer in &envelope.snapshot.peers {
            if !valid_wire_relay_id(&peer.relay_id)
                || !crate::relay::route_table::valid_relay_url(&peer.url)
            {
                return Err(FullSyncValidationError::InvalidPeer {
                    relay_id: peer.relay_id.clone(),
                });
            }
        }

        let mut routes = Vec::with_capacity(envelope.snapshot.routes.len());
        for route in envelope.snapshot.routes {
            if !valid_full_sync_route(&route) {
                return Err(FullSyncValidationError::InvalidRoute {
                    name: route.name,
                    relay_id: route.relay_id,
                });
            }
            let entry = route.to_peer_route_entry();
            if route.hash != route_entry_digest_hash(&entry) {
                return Err(FullSyncValidationError::InvalidRouteHash {
                    name: route.name,
                    relay_id: route.relay_id,
                });
            }
            routes.push(entry);
        }

        let mut tombstones = Vec::with_capacity(envelope.snapshot.tombstones.len());
        for tombstone in envelope.snapshot.tombstones {
            if !valid_full_sync_tombstone(&tombstone) {
                return Err(FullSyncValidationError::InvalidTombstone {
                    name: tombstone.name,
                    relay_id: tombstone.relay_id,
                });
            }
            let internal = tombstone.to_tombstone();
            if tombstone.hash != tombstone_digest_hash(&internal) {
                return Err(FullSyncValidationError::InvalidTombstoneHash {
                    name: tombstone.name,
                    relay_id: tombstone.relay_id,
                });
            }
            tombstones.push(internal);
        }

        Ok(Self(FullSync {
            routes,
            tombstones,
            peers: envelope.snapshot.peers,
        }))
    }
}

fn valid_full_sync_route(route: &FullSyncRoute) -> bool {
    crate::relay::route_table::valid_route_name(&route.name)
        && valid_wire_relay_id(&route.relay_id)
        && crate::relay::route_table::valid_relay_url(&route.relay_url)
        && crate::relay::route_table::valid_crm_tag(&route.crm_ns, &route.crm_name, &route.crm_ver)
        && c2_contract::validate_contract_hash("abi_hash", &route.abi_hash).is_ok()
        && c2_contract::validate_contract_hash("signature_hash", &route.signature_hash).is_ok()
        && route.max_payload_size > 0
        && c2_contract::validate_call_route_key("route_uid", &route.route_uid).is_ok()
        && route.route_revision > 0
        && route.registered_at.is_finite()
        && valid_route_digest_hash(&route.hash)
}

fn valid_full_sync_tombstone(tombstone: &FullSyncTombstone) -> bool {
    crate::relay::route_table::valid_route_name(&tombstone.name)
        && valid_wire_relay_id(&tombstone.relay_id)
        && tombstone.removed_at.is_finite()
        && tombstone.removed_revision > 0
        && valid_route_digest_hash(&tombstone.hash)
}

pub(crate) fn valid_wire_relay_id(relay_id: &str) -> bool {
    c2_config::validate_relay_id(relay_id).is_ok()
}

pub(crate) fn valid_route_digest_hash(hash: &str) -> bool {
    hash.len() == 64
        && hash
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
}

pub(crate) fn route_entry_digest_hash(entry: &RouteEntry) -> RouteDigestHash {
    active_route_digest_hash(
        &entry.name,
        &entry.relay_id,
        &entry.relay_url,
        &entry.crm_ns,
        &entry.crm_name,
        &entry.crm_ver,
        &entry.abi_hash,
        &entry.signature_hash,
        entry.max_payload_size,
        &entry.route_uid,
        entry.route_revision,
        entry.registered_at,
    )
}

pub(crate) fn tombstone_digest_hash(tombstone: &RouteTombstone) -> RouteDigestHash {
    deleted_route_digest_hash(
        &tombstone.name,
        &tombstone.relay_id,
        tombstone.removed_at,
        tombstone.removed_revision,
    )
}

#[allow(clippy::too_many_arguments)]
pub(crate) fn active_route_digest_hash(
    name: &str,
    relay_id: &str,
    relay_url: &str,
    crm_ns: &str,
    crm_name: &str,
    crm_ver: &str,
    abi_hash: &str,
    signature_hash: &str,
    max_payload_size: u64,
    route_uid: &str,
    route_revision: u64,
    registered_at: f64,
) -> RouteDigestHash {
    stable_route_digest_hash(&[
        "active",
        name,
        relay_id,
        relay_url,
        crm_ns,
        crm_name,
        crm_ver,
        abi_hash,
        signature_hash,
        &max_payload_size.to_string(),
        route_uid,
        &route_revision.to_string(),
        &registered_at.to_bits().to_string(),
    ])
}

pub(crate) fn deleted_route_digest_hash(
    name: &str,
    relay_id: &str,
    removed_at: f64,
    removed_revision: u64,
) -> RouteDigestHash {
    stable_route_digest_hash(&[
        "deleted",
        name,
        relay_id,
        &removed_at.to_bits().to_string(),
        &removed_revision.to_string(),
    ])
}

fn stable_route_digest_hash(fields: &[&str]) -> RouteDigestHash {
    use sha2::{Digest, Sha256};

    let mut hasher = Sha256::new();
    for field in fields {
        let bytes = field.as_bytes();
        hasher.update((bytes.len() as u64).to_be_bytes());
        hasher.update(bytes);
    }
    hex::encode(hasher.finalize())
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::Value;

    const ABI_HASH: &str = "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef";
    const SIGNATURE_HASH: &str = "abcdef0123456789abcdef0123456789abcdef0123456789abcdef0123456789";

    fn local_route(name: &str) -> RouteEntry {
        RouteEntry {
            name: name.to_string(),
            relay_id: "relay-a".to_string(),
            relay_url: "http://relay-a:8080".to_string(),
            server_id: Some("server-a".to_string()),
            server_instance_id: Some("instance-a".to_string()),
            ipc_address: Some("ipc://relay_a".to_string()),
            crm_ns: "test.ns".to_string(),
            crm_name: "Grid".to_string(),
            crm_ver: "0.1.0".to_string(),
            abi_hash: ABI_HASH.to_string(),
            signature_hash: SIGNATURE_HASH.to_string(),
            max_payload_size: 1024,
            route_uid: format!("{name}-uid"),
            route_revision: 1,
            locality: Locality::Local,
            registered_at: 1000.0,
        }
    }

    #[test]
    fn full_sync_envelope_rejects_old_empty_snapshot_before_merge() {
        let envelope = FullSyncEnvelope {
            protocol_version: ROUTE_HASH_FULL_SYNC_VERSION - 1,
            snapshot: FullSyncSnapshot {
                routes: Vec::new(),
                tombstones: Vec::new(),
                peers: vec![PeerSnapshot {
                    relay_id: "relay-b".to_string(),
                    url: "http://relay-b:8080".to_string(),
                    route_count: 0,
                    status: PeerStatus::Alive,
                }],
            },
        };

        assert!(matches!(
            ValidatedFullSync::try_from(envelope),
            Err(FullSyncValidationError::RouteSnapshotTooOld { .. }),
        ));
    }

    #[test]
    fn full_sync_envelope_json_has_single_hash_bearing_wire_shape() {
        let internal = FullSync {
            routes: vec![local_route("grid")],
            tombstones: Vec::new(),
            peers: vec![PeerSnapshot {
                relay_id: "relay-a".to_string(),
                url: "http://relay-a:8080".to_string(),
                route_count: 1,
                status: PeerStatus::Alive,
            }],
        };
        let envelope = FullSyncEnvelope {
            protocol_version: PROTOCOL_VERSION,
            snapshot: FullSyncSnapshot::from_internal(internal),
        };

        let value: Value = serde_json::to_value(&envelope).unwrap();
        assert!(value.get("protocol_version").is_some());
        assert!(value.get("snapshot").is_some());
        assert!(value.get("routes").is_none());
        let route = &value["snapshot"]["routes"][0];
        assert_eq!(route["name"], "grid");
        assert!(route.get("hash").is_some());
        assert!(route.get("server_id").is_none());
        assert!(route.get("server_instance_id").is_none());
        assert!(route.get("ipc_address").is_none());
        assert!(route.get("locality").is_none());
    }

    #[test]
    fn full_sync_validation_rejects_key_replayed_route_hash() {
        let internal = FullSync {
            routes: vec![local_route("grid")],
            tombstones: Vec::new(),
            peers: vec![PeerSnapshot {
                relay_id: "relay-a".to_string(),
                url: "http://relay-a:8080".to_string(),
                route_count: 1,
                status: PeerStatus::Alive,
            }],
        };
        let mut snapshot = FullSyncSnapshot::from_internal(internal);
        snapshot.routes[0].name = "other".to_string();
        let envelope = FullSyncEnvelope {
            protocol_version: PROTOCOL_VERSION,
            snapshot,
        };

        assert!(matches!(
            ValidatedFullSync::try_from(envelope),
            Err(FullSyncValidationError::InvalidRouteHash { .. }),
        ));
    }
}
