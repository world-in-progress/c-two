use serde::{Deserialize, Serialize};
use std::collections::HashSet;
use std::fmt;
use std::net::SocketAddr;
use std::time::Instant;

use crate::relay::peer::PROTOCOL_VERSION;

const MAX_DIRECT_ADDRS: usize = 16;

/// An authenticated mesh endpoint and its advertised connection hints.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct IrohPeerAddr {
    pub endpoint_id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub bridge_url: Option<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub direct_addrs: Vec<String>,
}

/// The address advertised for a relay in the mesh.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PeerAddress {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub http_url: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub iroh: Option<IrohPeerAddr>,
}

impl PeerAddress {
    pub fn http(url: impl Into<String>) -> Self {
        Self {
            http_url: Some(url.into()),
            iroh: None,
        }
    }

    pub(crate) fn iroh(endpoint: iroh::EndpointAddr, bridge_url: Option<String>) -> Self {
        Self {
            http_url: None,
            iroh: Some(IrohPeerAddr {
                endpoint_id: hex::encode(endpoint.id.as_bytes()),
                bridge_url,
                direct_addrs: endpoint
                    .ip_addrs()
                    .filter(|addr| is_dialable_direct_addr(addr))
                    .take(MAX_DIRECT_ADDRS)
                    .map(ToString::to_string)
                    .collect(),
            }),
        }
    }

    pub fn is_valid(&self) -> bool {
        (self.http_url.is_some() || self.iroh.is_some())
            && self.http_url.as_deref().is_none_or(crate::valid_relay_url)
            && self.iroh.as_ref().is_none_or(|address| {
                address.endpoint_id().is_some()
                    && address
                        .bridge_url
                        .as_deref()
                        .is_none_or(crate::valid_relay_url)
                    && address.direct_addrs.len() <= MAX_DIRECT_ADDRS
                    && address
                        .direct_addrs
                        .iter()
                        .all(|value| value.parse::<SocketAddr>().is_ok())
            })
    }

    pub fn display_target(&self) -> String {
        self.http_url.clone().unwrap_or_else(|| {
            format!(
                "iroh://{}",
                self.endpoint_id_str().expect("validated peer address")
            )
        })
    }

    pub fn to_iroh_endpoint_addr(
        &self,
        fallback_bridge_url: Option<&str>,
    ) -> Option<iroh::EndpointAddr> {
        let address = self.iroh.as_ref()?;
        let mut endpoint = iroh::EndpointAddr::new(address.endpoint_id()?);
        if let Some(bridge) = address.bridge_url.as_deref().or(fallback_bridge_url) {
            endpoint = endpoint.with_relay_url(bridge.parse().ok()?);
        }
        for direct in &address.direct_addrs {
            endpoint = endpoint.with_ip_addr(direct.parse().ok()?);
        }
        Some(endpoint)
    }

    pub(crate) fn same_identity(&self, other: &Self) -> bool {
        self.http_url == other.http_url && self.endpoint_id_str() == other.endpoint_id_str()
    }

    pub(crate) fn endpoint_id(&self) -> Option<iroh::EndpointId> {
        self.iroh.as_ref()?.endpoint_id()
    }

    pub(crate) fn endpoint_id_str(&self) -> Option<&str> {
        self.iroh
            .as_ref()
            .map(|address| address.endpoint_id.as_str())
    }
}

impl IrohPeerAddr {
    fn endpoint_id(&self) -> Option<iroh::EndpointId> {
        if !valid_route_digest_hash(&self.endpoint_id) {
            return None;
        }
        self.endpoint_id.parse().ok()
    }
}

fn is_dialable_direct_addr(addr: &SocketAddr) -> bool {
    addr.port() != 0 && !addr.ip().to_canonical().is_unspecified()
}

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
    pub address: PeerAddress,
    pub server_id: Option<String>,
    pub server_instance_id: Option<String>,
    pub ipc_address: Option<String>,
    pub crm_ns: String,
    pub crm_name: String,
    pub crm_ver: String,
    pub abi_hash: String,
    pub signature_hash: String,
    pub max_payload_size: u64,
    pub route_uid: Option<String>,
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
    pub server_id: Option<String>,
    pub observed_at: Instant,
}

/// Resolution result returned to clients via /_resolve.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RouteInfo {
    pub name: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub relay_url: Option<String>,
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub proxy_via_anchor: bool,
    pub ipc_address: Option<String>,
    pub server_id: Option<String>,
    pub server_instance_id: Option<String>,
    pub crm_ns: String,
    pub crm_name: String,
    pub crm_ver: String,
    pub abi_hash: String,
    pub signature_hash: String,
    pub max_payload_size: u64,
}

impl RouteEntry {
    pub fn to_route_info(&self) -> RouteInfo {
        // `ipc_address` is private to the owning relay's filesystem; only
        // expose it for LOCAL routes.
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
            relay_url: self.address.http_url.clone(),
            proxy_via_anchor: false,
            ipc_address,
            server_id,
            server_instance_id,
            crm_ns: self.crm_ns.clone(),
            crm_name: self.crm_name.clone(),
            crm_ver: self.crm_ver.clone(),
            abi_hash: self.abi_hash.clone(),
            signature_hash: self.signature_hash.clone(),
            max_payload_size: self.max_payload_size,
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

/// One registration as observed in this relay's mesh view.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct RouteRegistrationInfo {
    pub relay_id: String,
    pub relay_url: String,
    pub registered_at: f64,
    pub relay_status: PeerStatus,
}

#[derive(Debug, Clone)]
pub enum RouteResolutionError {
    Unavailable(Vec<RouteRegistrationInfo>),
    Conflict(Vec<RouteRegistrationInfo>),
}

/// Information about a peer relay.
#[derive(Debug, Clone)]
pub struct PeerInfo {
    pub relay_id: String,
    pub address: PeerAddress,
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
#[serde(deny_unknown_fields)]
pub struct PeerSnapshot {
    pub relay_id: String,
    pub address: PeerAddress,
    pub route_count: u32,
    pub status: PeerStatus,
}

pub type RouteDigestHash = String;

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct FullSyncEnvelope {
    pub protocol_version: u32,
    pub sender_relay_id: String,
    pub snapshot: FullSyncSnapshot,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct FullSyncSnapshot {
    pub routes: Vec<FullSyncRoute>,
    #[serde(default)]
    pub tombstones: Vec<FullSyncTombstone>,
    pub peers: Vec<PeerSnapshot>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct FullSyncRoute {
    pub name: String,
    pub relay_id: String,
    pub address: PeerAddress,
    pub crm_ns: String,
    pub crm_name: String,
    pub crm_ver: String,
    pub abi_hash: String,
    pub signature_hash: String,
    pub max_payload_size: u64,
    pub registered_at: f64,
    pub hash: RouteDigestHash,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct FullSyncTombstone {
    pub name: String,
    pub relay_id: String,
    pub removed_at: f64,
    pub hash: RouteDigestHash,
}

#[derive(Debug, Clone)]
pub struct ValidatedFullSync {
    sender_relay_id: String,
    snapshot: FullSync,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FullSyncValidationError {
    UnsupportedRouteSnapshotVersion { got: u32, supported: u32 },
    InvalidSender { sender_relay_id: String },
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
            address: entry.address.clone(),
            crm_ns: entry.crm_ns.clone(),
            crm_name: entry.crm_name.clone(),
            crm_ver: entry.crm_ver.clone(),
            abi_hash: entry.abi_hash.clone(),
            signature_hash: entry.signature_hash.clone(),
            max_payload_size: entry.max_payload_size,
            registered_at: entry.registered_at,
            hash: route_entry_digest_hash(entry),
        }
    }

    pub(crate) fn to_peer_route_entry(&self) -> RouteEntry {
        RouteEntry {
            name: self.name.clone(),
            relay_id: self.relay_id.clone(),
            address: self.address.clone(),
            server_id: None,
            server_instance_id: None,
            ipc_address: None,
            crm_ns: self.crm_ns.clone(),
            crm_name: self.crm_name.clone(),
            crm_ver: self.crm_ver.clone(),
            abi_hash: self.abi_hash.clone(),
            signature_hash: self.signature_hash.clone(),
            max_payload_size: self.max_payload_size,
            route_uid: None,
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
            hash: tombstone_digest_hash(tombstone),
        }
    }

    pub(crate) fn to_tombstone(&self) -> RouteTombstone {
        RouteTombstone {
            name: self.name.clone(),
            relay_id: self.relay_id.clone(),
            removed_at: self.removed_at,
            server_id: None,
            observed_at: observed_now(),
        }
    }
}

impl ValidatedFullSync {
    pub(crate) fn into_inner(self) -> FullSync {
        self.snapshot
    }

    pub(crate) fn sender_relay_id(&self) -> &str {
        &self.sender_relay_id
    }

    pub(crate) fn peers(&self) -> &[PeerSnapshot] {
        &self.snapshot.peers
    }
}

impl TryFrom<FullSyncEnvelope> for ValidatedFullSync {
    type Error = FullSyncValidationError;

    fn try_from(envelope: FullSyncEnvelope) -> Result<Self, Self::Error> {
        if envelope.protocol_version != PROTOCOL_VERSION {
            return Err(FullSyncValidationError::UnsupportedRouteSnapshotVersion {
                got: envelope.protocol_version,
                supported: PROTOCOL_VERSION,
            });
        }
        if !valid_wire_relay_id(&envelope.sender_relay_id) {
            return Err(FullSyncValidationError::InvalidSender {
                sender_relay_id: envelope.sender_relay_id,
            });
        }

        let mut seen_peers = HashSet::new();
        for peer in &envelope.snapshot.peers {
            if !valid_wire_relay_id(&peer.relay_id)
                || !peer.address.is_valid()
                || !seen_peers.insert(peer.relay_id.as_str())
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

        Ok(Self {
            sender_relay_id: envelope.sender_relay_id,
            snapshot: FullSync {
                routes,
                tombstones,
                peers: envelope.snapshot.peers,
            },
        })
    }
}

fn valid_full_sync_route(route: &FullSyncRoute) -> bool {
    crate::relay::route_table::valid_route_name(&route.name)
        && valid_wire_relay_id(&route.relay_id)
        && route.address.is_valid()
        && crate::relay::route_table::valid_crm_tag(&route.crm_ns, &route.crm_name, &route.crm_ver)
        && c2_contract::validate_contract_hash("abi_hash", &route.abi_hash).is_ok()
        && c2_contract::validate_contract_hash("signature_hash", &route.signature_hash).is_ok()
        && route.max_payload_size > 0
        && route.registered_at.is_finite()
        && valid_route_digest_hash(&route.hash)
}

fn valid_full_sync_tombstone(tombstone: &FullSyncTombstone) -> bool {
    crate::relay::route_table::valid_route_name(&tombstone.name)
        && valid_wire_relay_id(&tombstone.relay_id)
        && tombstone.removed_at.is_finite()
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
        &entry.address,
        &entry.crm_ns,
        &entry.crm_name,
        &entry.crm_ver,
        &entry.abi_hash,
        &entry.signature_hash,
        entry.max_payload_size,
        entry.registered_at,
    )
}

pub(crate) fn tombstone_digest_hash(tombstone: &RouteTombstone) -> RouteDigestHash {
    deleted_route_digest_hash(&tombstone.name, &tombstone.relay_id, tombstone.removed_at)
}

#[allow(clippy::too_many_arguments)]
pub(crate) fn active_route_digest_hash(
    name: &str,
    relay_id: &str,
    address: &PeerAddress,
    crm_ns: &str,
    crm_name: &str,
    crm_ver: &str,
    abi_hash: &str,
    signature_hash: &str,
    max_payload_size: u64,
    registered_at: f64,
) -> RouteDigestHash {
    stable_route_digest_hash(&[
        "active",
        name,
        relay_id,
        address.http_url.as_deref().unwrap_or_default(),
        address.endpoint_id_str().unwrap_or_default(),
        crm_ns,
        crm_name,
        crm_ver,
        abi_hash,
        signature_hash,
        &max_payload_size.to_string(),
        &registered_at.to_bits().to_string(),
    ])
}

pub(crate) fn deleted_route_digest_hash(
    name: &str,
    relay_id: &str,
    removed_at: f64,
) -> RouteDigestHash {
    stable_route_digest_hash(&["deleted", name, relay_id, &removed_at.to_bits().to_string()])
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
            address: PeerAddress::http("http://relay-a:8080"),
            server_id: Some("server-a".to_string()),
            server_instance_id: Some("instance-a".to_string()),
            ipc_address: Some("ipc://relay_a".to_string()),
            crm_ns: "test.ns".to_string(),
            crm_name: "Grid".to_string(),
            crm_ver: "0.1.0".to_string(),
            abi_hash: ABI_HASH.to_string(),
            signature_hash: SIGNATURE_HASH.to_string(),
            max_payload_size: 1024,
            route_uid: Some(format!("{name}-uid")),
            locality: Locality::Local,
            registered_at: 1000.0,
        }
    }

    #[test]
    fn mesh_address_validates_hints_and_rejects_unknown_fields() {
        let endpoint = iroh::SecretKey::from_bytes(&[11; 32]).public();
        let address = PeerAddress::iroh(
            iroh::EndpointAddr::new(endpoint)
                .with_ip_addr("[::]:7878".parse().unwrap())
                .with_ip_addr("127.0.0.1:7879".parse().unwrap()),
            Some("https://bridge.example:8443".into()),
        );
        assert!(address.is_valid());
        assert_eq!(
            address.display_target(),
            format!("iroh://{}", hex::encode(endpoint.as_bytes()))
        );
        assert_eq!(
            address.iroh.as_ref().unwrap().direct_addrs,
            ["127.0.0.1:7879"]
        );
        let dial = address
            .to_iroh_endpoint_addr(Some("https://fallback.example"))
            .unwrap();
        assert_eq!(dial.id, endpoint);
        assert_eq!(
            dial.relay_urls().next().unwrap().to_string(),
            "https://bridge.example:8443/"
        );
        let mut wire = serde_json::to_value(&address).unwrap();
        wire["iroh"]["bridge_url"] = Value::Null;
        let dial = serde_json::from_value::<PeerAddress>(wire.clone())
            .unwrap()
            .to_iroh_endpoint_addr(Some("https://fallback.example"))
            .unwrap();
        assert_eq!(
            dial.relay_urls().next().unwrap().to_string(),
            "https://fallback.example/"
        );
        for (field, invalid) in [
            (
                "endpoint_id",
                Value::String(hex::encode_upper(endpoint.as_bytes())),
            ),
            (
                "bridge_url",
                serde_json::json!("https://user@bridge.example"),
            ),
            ("direct_addrs", serde_json::json!(["bridge.example:8443"])),
            (
                "direct_addrs",
                serde_json::json!(vec!["127.0.0.1:7879"; MAX_DIRECT_ADDRS + 1]),
            ),
        ] {
            let mut invalid_wire = wire.clone();
            invalid_wire["iroh"][field] = invalid;
            assert!(
                !serde_json::from_value::<PeerAddress>(invalid_wire)
                    .unwrap()
                    .is_valid(),
                "{field}"
            );
        }
        wire["iroh"]["unknown"] = 1.into();
        assert!(serde_json::from_value::<PeerAddress>(wire).is_err());
        assert!(
            !serde_json::from_value::<PeerAddress>(serde_json::json!({}))
                .unwrap()
                .is_valid()
        );
    }

    #[test]
    fn route_digest_and_registration_identity_ignore_iroh_hints() {
        let endpoint = iroh::SecretKey::from_bytes(&[12; 32]).public();
        let mut entry = local_route("grid");
        entry.address = PeerAddress::iroh(iroh::EndpointAddr::new(endpoint), None);
        entry.locality = Locality::Peer;
        entry.route_uid = None;
        entry.server_id = None;
        entry.server_instance_id = None;
        entry.ipc_address = None;
        let mut table = crate::relay::route_table::RouteTable::new("receiver".into());
        assert!(table.register_route(entry.clone()));
        let hash = route_entry_digest_hash(&entry);
        let hints = entry.address.iroh.as_mut().unwrap();
        hints.bridge_url = Some("https://bridge.example".into());
        hints.direct_addrs.push("127.0.0.1:9410".into());
        assert_eq!(route_entry_digest_hash(&entry), hash);
        assert!(table.peer_entry_matches(&entry));

        entry.address.http_url = Some("https://relay.example".into());
        assert_ne!(route_entry_digest_hash(&entry), hash);
        assert!(!table.peer_entry_matches(&entry));
        entry.address.http_url = None;
        entry.address.iroh.as_mut().unwrap().endpoint_id =
            hex::encode(iroh::SecretKey::from_bytes(&[13; 32]).public().as_bytes());
        assert_ne!(route_entry_digest_hash(&entry), hash);
        assert!(!table.peer_entry_matches(&entry));
    }

    #[test]
    fn full_sync_envelope_rejects_old_empty_snapshot_before_merge() {
        let envelope = FullSyncEnvelope {
            protocol_version: PROTOCOL_VERSION - 1,
            sender_relay_id: "relay-b".into(),
            snapshot: FullSyncSnapshot {
                routes: Vec::new(),
                tombstones: Vec::new(),
                peers: vec![PeerSnapshot {
                    relay_id: "relay-b".to_string(),
                    address: PeerAddress::http("http://relay-b:8080"),
                    route_count: 0,
                    status: PeerStatus::Alive,
                }],
            },
        };

        assert!(matches!(
            ValidatedFullSync::try_from(envelope),
            Err(FullSyncValidationError::UnsupportedRouteSnapshotVersion { .. }),
        ));
    }

    #[test]
    fn full_sync_nine_rejects_other_versions_and_retired_fields() {
        let envelope = FullSyncEnvelope {
            protocol_version: 8,
            sender_relay_id: "relay-a".into(),
            snapshot: FullSyncSnapshot::from_internal(FullSync {
                routes: vec![local_route("grid")],
                tombstones: vec![RouteTombstone {
                    name: "removed".into(),
                    relay_id: "relay-a".into(),
                    removed_at: 1001.0,
                    server_id: None,
                    observed_at: Instant::now(),
                }],
                peers: Vec::new(),
            }),
        };
        assert!(matches!(
            ValidatedFullSync::try_from(envelope.clone()),
            Err(FullSyncValidationError::UnsupportedRouteSnapshotVersion {
                got: 8,
                supported: 9
            })
        ));
        let mut future = envelope.clone();
        future.protocol_version = 10;
        assert!(matches!(
            ValidatedFullSync::try_from(future),
            Err(FullSyncValidationError::UnsupportedRouteSnapshotVersion {
                got: 10,
                supported: 9
            })
        ));
        let mut current = serde_json::to_value(envelope).unwrap();
        current["protocol_version"] = 9.into();
        assert!(
            ValidatedFullSync::try_from(
                serde_json::from_value::<FullSyncEnvelope>(current.clone()).unwrap()
            )
            .is_ok()
        );
        for (collection, retired) in [
            ("routes", "route_uid"),
            ("routes", "route_revision"),
            ("tombstones", "removed_revision"),
        ] {
            let mut invalid = current.clone();
            invalid["snapshot"][collection][0][retired] = 1.into();
            assert!(serde_json::from_value::<FullSyncEnvelope>(invalid).is_err());
        }
    }

    #[test]
    fn full_sync_envelope_json_has_single_hash_bearing_wire_shape() {
        let internal = FullSync {
            routes: vec![local_route("grid")],
            tombstones: Vec::new(),
            peers: vec![PeerSnapshot {
                relay_id: "relay-a".to_string(),
                address: PeerAddress::http("http://relay-a:8080"),
                route_count: 1,
                status: PeerStatus::Alive,
            }],
        };
        let envelope = FullSyncEnvelope {
            protocol_version: PROTOCOL_VERSION,
            sender_relay_id: "relay-a".into(),
            snapshot: FullSyncSnapshot::from_internal(internal),
        };

        let value: Value = serde_json::to_value(&envelope).unwrap();
        assert!(value.get("protocol_version").is_some());
        assert_eq!(value["sender_relay_id"], "relay-a");
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
                address: PeerAddress::http("http://relay-a:8080"),
                route_count: 1,
                status: PeerStatus::Alive,
            }],
        };
        let mut snapshot = FullSyncSnapshot::from_internal(internal);
        snapshot.routes[0].name = "other".to_string();
        let envelope = FullSyncEnvelope {
            protocol_version: PROTOCOL_VERSION,
            sender_relay_id: "relay-a".into(),
            snapshot,
        };

        assert!(matches!(
            ValidatedFullSync::try_from(envelope),
            Err(FullSyncValidationError::InvalidRouteHash { .. }),
        ));
    }
}
