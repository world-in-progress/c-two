use serde::{Deserialize, Serialize};

use crate::relay::types::{
    Locality, RouteDigestHash, RouteEntry, active_route_digest_hash, deleted_route_digest_hash,
    valid_route_digest_hash, valid_wire_relay_id,
};

pub const PROTOCOL_VERSION: u32 = 6;
pub const ROUTE_HASH_PEER_VERSION: u32 = 6;

/// Envelope wrapping every peer-to-peer message.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PeerEnvelope {
    pub protocol_version: u32,
    pub sender_relay_id: String,
    pub message: PeerMessage,
}

/// Gossip message variants.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type")]
pub enum PeerMessage {
    /// A CRM route was registered or updated.
    RouteAnnounce {
        name: String,
        relay_id: String,
        relay_url: String,
        crm_ns: String,
        crm_name: String,
        crm_ver: String,
        abi_hash: String,
        signature_hash: String,
        max_payload_size: u64,
        route_uid: String,
        route_revision: u64,
        registered_at: f64,
    },
    /// A CRM route was unregistered.
    RouteWithdraw {
        name: String,
        relay_id: String,
        removed_at: f64,
        removed_revision: u64,
    },
    /// A relay wants to join the mesh.
    RelayJoin { relay_id: String, url: String },
    /// A relay is gracefully leaving the mesh.
    RelayLeave { relay_id: String },
    /// Periodic heartbeat.
    Heartbeat { relay_id: String, route_count: u32 },
    /// Anti-entropy digest exchange (request or response).
    DigestExchange { digest: Vec<DigestEntry> },
    /// Anti-entropy diff: entries the sender has that recipient was missing.
    DigestDiff { entries: Vec<DigestDiffEntry> },
    /// Unknown message type from a newer protocol version.
    #[serde(other)]
    Unknown,
}

/// One entry in a digest map.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DigestEntry {
    pub name: String,
    pub relay_id: String,
    #[serde(default)]
    pub deleted: bool,
    pub hash: RouteDigestHash,
}

/// Route state sent to repair digest differences.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "state")]
pub enum DigestDiffEntry {
    Active {
        name: String,
        relay_id: String,
        relay_url: String,
        crm_ns: String,
        crm_name: String,
        crm_ver: String,
        abi_hash: String,
        signature_hash: String,
        max_payload_size: u64,
        route_uid: String,
        route_revision: u64,
        registered_at: f64,
        hash: RouteDigestHash,
    },
    Deleted {
        name: String,
        relay_id: String,
        removed_at: f64,
        removed_revision: u64,
        hash: RouteDigestHash,
    },
}

impl PeerEnvelope {
    pub fn new(sender: &str, message: PeerMessage) -> Self {
        Self {
            protocol_version: PROTOCOL_VERSION,
            sender_relay_id: sender.to_string(),
            message,
        }
    }
}

#[derive(Debug, Clone)]
pub(crate) struct ValidatedRouteStateEnvelope {
    envelope: PeerEnvelope,
    digest_diff_entries: Option<Vec<ValidatedDigestDiffEntry>>,
}

#[derive(Debug, Clone)]
pub(crate) enum ValidatedDigestDiffEntry {
    Active(ValidatedDigestDiffActive),
    Deleted(ValidatedDigestDiffDeleted),
}

#[derive(Debug, Clone)]
pub(crate) struct ValidatedDigestDiffActive {
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
}

#[derive(Debug, Clone)]
pub(crate) struct ValidatedDigestDiffDeleted {
    pub name: String,
    pub relay_id: String,
    pub removed_at: f64,
    pub removed_revision: u64,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PeerRouteStateError {
    RouteStateProtocolTooOld { got: u32, min: u32 },
    UnsupportedRouteStateProtocol { got: u32, supported: u32 },
    InvalidSender { sender_relay_id: String },
    InvalidRouteAnnounce { reason: String },
    InvalidRouteWithdraw { reason: String },
    InvalidRelayLeave { reason: String },
    InvalidDigestEntry { reason: String },
    InvalidDigestDiff { reason: String },
}

impl ValidatedRouteStateEnvelope {
    pub(crate) fn into_envelope(self) -> PeerEnvelope {
        self.envelope
    }

    pub(crate) fn into_digest_diff_entries(self) -> Option<Vec<ValidatedDigestDiffEntry>> {
        self.digest_diff_entries
    }
}

impl From<ValidatedDigestDiffActive> for RouteEntry {
    fn from(active: ValidatedDigestDiffActive) -> Self {
        Self {
            name: active.name,
            relay_id: active.relay_id,
            relay_url: active.relay_url,
            server_id: None,
            server_instance_id: None,
            ipc_address: None,
            crm_ns: active.crm_ns,
            crm_name: active.crm_name,
            crm_ver: active.crm_ver,
            abi_hash: active.abi_hash,
            signature_hash: active.signature_hash,
            max_payload_size: active.max_payload_size,
            route_uid: active.route_uid,
            route_revision: active.route_revision,
            locality: Locality::Peer,
            registered_at: active.registered_at,
        }
    }
}

pub fn validate_route_state_envelope(
    envelope: PeerEnvelope,
    expected_sender: Option<&str>,
) -> Result<ValidatedRouteStateEnvelope, PeerRouteStateError> {
    if envelope.protocol_version < ROUTE_HASH_PEER_VERSION {
        return Err(PeerRouteStateError::RouteStateProtocolTooOld {
            got: envelope.protocol_version,
            min: ROUTE_HASH_PEER_VERSION,
        });
    }
    if envelope.protocol_version != PROTOCOL_VERSION {
        return Err(PeerRouteStateError::UnsupportedRouteStateProtocol {
            got: envelope.protocol_version,
            supported: PROTOCOL_VERSION,
        });
    }
    if !valid_wire_relay_id(&envelope.sender_relay_id) {
        return Err(PeerRouteStateError::InvalidSender {
            sender_relay_id: envelope.sender_relay_id,
        });
    }
    if let Some(expected) = expected_sender
        && envelope.sender_relay_id != expected
    {
        return Err(PeerRouteStateError::InvalidSender {
            sender_relay_id: envelope.sender_relay_id,
        });
    }

    let digest_diff_entries = match &envelope.message {
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
            if relay_id != &envelope.sender_relay_id
                || !valid_active_route_fields(
                    name,
                    relay_id,
                    relay_url,
                    crm_ns,
                    crm_name,
                    crm_ver,
                    abi_hash,
                    signature_hash,
                    *max_payload_size,
                    route_uid,
                    *route_revision,
                    *registered_at,
                )
            {
                return Err(PeerRouteStateError::InvalidRouteAnnounce {
                    reason: "invalid route announcement".to_string(),
                });
            }
            None
        }
        PeerMessage::RouteWithdraw {
            name,
            relay_id,
            removed_at,
            removed_revision,
        } => {
            if relay_id != &envelope.sender_relay_id
                || !valid_deleted_route_fields(name, relay_id, *removed_at, *removed_revision)
            {
                return Err(PeerRouteStateError::InvalidRouteWithdraw {
                    reason: "invalid route withdrawal".to_string(),
                });
            }
            None
        }
        PeerMessage::RelayLeave { relay_id } => {
            if relay_id != &envelope.sender_relay_id || !valid_wire_relay_id(relay_id) {
                return Err(PeerRouteStateError::InvalidRelayLeave {
                    reason: "invalid relay leave".to_string(),
                });
            }
            None
        }
        PeerMessage::DigestExchange { digest } => {
            validate_digest_entries(digest)?;
            None
        }
        PeerMessage::DigestDiff { entries } => Some(validate_digest_diff_entries(
            &envelope.sender_relay_id,
            entries,
        )?),
        PeerMessage::RelayJoin { .. } | PeerMessage::Heartbeat { .. } | PeerMessage::Unknown => {
            None
        }
    };

    Ok(ValidatedRouteStateEnvelope {
        envelope,
        digest_diff_entries,
    })
}

fn validate_digest_entries(entries: &[DigestEntry]) -> Result<(), PeerRouteStateError> {
    for entry in entries {
        if !crate::relay::route_table::valid_route_name(&entry.name)
            || !valid_wire_relay_id(&entry.relay_id)
            || !valid_route_digest_hash(&entry.hash)
        {
            return Err(PeerRouteStateError::InvalidDigestEntry {
                reason: format!(
                    "invalid digest entry for route {}/{}",
                    entry.name, entry.relay_id
                ),
            });
        }
    }
    Ok(())
}

fn validate_digest_diff_entries(
    sender_relay_id: &str,
    entries: &[DigestDiffEntry],
) -> Result<Vec<ValidatedDigestDiffEntry>, PeerRouteStateError> {
    let mut validated = Vec::with_capacity(entries.len());
    for entry in entries {
        let expected_hash = route_digest_hash_for_diff_entry(entry)?;
        match entry {
            DigestDiffEntry::Active {
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
                hash,
            } => {
                if relay_id != sender_relay_id
                    || !valid_active_route_fields(
                        name,
                        relay_id,
                        relay_url,
                        crm_ns,
                        crm_name,
                        crm_ver,
                        abi_hash,
                        signature_hash,
                        *max_payload_size,
                        route_uid,
                        *route_revision,
                        *registered_at,
                    )
                    || hash != &expected_hash
                {
                    return Err(PeerRouteStateError::InvalidDigestDiff {
                        reason: format!("invalid active diff for route {name}/{relay_id}"),
                    });
                }
                validated.push(ValidatedDigestDiffEntry::Active(
                    ValidatedDigestDiffActive {
                        name: name.clone(),
                        relay_id: relay_id.clone(),
                        relay_url: relay_url.clone(),
                        crm_ns: crm_ns.clone(),
                        crm_name: crm_name.clone(),
                        crm_ver: crm_ver.clone(),
                        abi_hash: abi_hash.clone(),
                        signature_hash: signature_hash.clone(),
                        max_payload_size: *max_payload_size,
                        route_uid: route_uid.clone(),
                        route_revision: *route_revision,
                        registered_at: *registered_at,
                    },
                ));
            }
            DigestDiffEntry::Deleted {
                name,
                relay_id,
                removed_at,
                removed_revision,
                hash,
            } => {
                if relay_id != sender_relay_id
                    || !valid_deleted_route_fields(name, relay_id, *removed_at, *removed_revision)
                    || hash != &expected_hash
                {
                    return Err(PeerRouteStateError::InvalidDigestDiff {
                        reason: format!("invalid deleted diff for route {name}/{relay_id}"),
                    });
                }
                validated.push(ValidatedDigestDiffEntry::Deleted(
                    ValidatedDigestDiffDeleted {
                        name: name.clone(),
                        relay_id: relay_id.clone(),
                        removed_at: *removed_at,
                        removed_revision: *removed_revision,
                    },
                ));
            }
        }
    }
    Ok(validated)
}

pub fn route_digest_hash_for_diff_entry(
    entry: &DigestDiffEntry,
) -> Result<RouteDigestHash, PeerRouteStateError> {
    match entry {
        DigestDiffEntry::Active {
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
            ..
        } => {
            if !valid_active_route_fields(
                name,
                relay_id,
                relay_url,
                crm_ns,
                crm_name,
                crm_ver,
                abi_hash,
                signature_hash,
                *max_payload_size,
                route_uid,
                *route_revision,
                *registered_at,
            ) {
                return Err(PeerRouteStateError::InvalidDigestDiff {
                    reason: format!("invalid active diff for route {name}/{relay_id}"),
                });
            }
            Ok(active_route_digest_hash(
                name,
                relay_id,
                relay_url,
                crm_ns,
                crm_name,
                crm_ver,
                abi_hash,
                signature_hash,
                *max_payload_size,
                route_uid,
                *route_revision,
                *registered_at,
            ))
        }
        DigestDiffEntry::Deleted {
            name,
            relay_id,
            removed_at,
            removed_revision,
            ..
        } => {
            if !valid_deleted_route_fields(name, relay_id, *removed_at, *removed_revision) {
                return Err(PeerRouteStateError::InvalidDigestDiff {
                    reason: format!("invalid deleted diff for route {name}/{relay_id}"),
                });
            }
            Ok(deleted_route_digest_hash(
                name,
                relay_id,
                *removed_at,
                *removed_revision,
            ))
        }
    }
}

#[allow(clippy::too_many_arguments)]
fn valid_active_route_fields(
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
) -> bool {
    crate::relay::route_table::valid_route_name(name)
        && valid_wire_relay_id(relay_id)
        && crate::relay::route_table::valid_relay_url(relay_url)
        && crate::relay::route_table::valid_crm_tag(crm_ns, crm_name, crm_ver)
        && c2_contract::validate_contract_hash("abi_hash", abi_hash).is_ok()
        && c2_contract::validate_contract_hash("signature_hash", signature_hash).is_ok()
        && max_payload_size > 0
        && c2_contract::validate_call_route_key("route_uid", route_uid).is_ok()
        && route_revision > 0
        && registered_at.is_finite()
}

fn valid_deleted_route_fields(
    name: &str,
    relay_id: &str,
    removed_at: f64,
    removed_revision: u64,
) -> bool {
    crate::relay::route_table::valid_route_name(name)
        && valid_wire_relay_id(relay_id)
        && removed_at.is_finite()
        && removed_revision > 0
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn envelope_roundtrip_json() {
        let env = PeerEnvelope::new(
            "relay-a",
            PeerMessage::Heartbeat {
                relay_id: "relay-a".into(),
                route_count: 5,
            },
        );
        let json = serde_json::to_string(&env).unwrap();
        let decoded: PeerEnvelope = serde_json::from_str(&json).unwrap();
        assert_eq!(decoded.protocol_version, PROTOCOL_VERSION);
        assert_eq!(decoded.sender_relay_id, "relay-a");
        match decoded.message {
            PeerMessage::Heartbeat {
                relay_id,
                route_count,
            } => {
                assert_eq!(relay_id, "relay-a");
                assert_eq!(route_count, 5);
            }
            _ => panic!("wrong variant"),
        }
    }

    #[test]
    fn route_announce_roundtrip() {
        let env = PeerEnvelope::new(
            "relay-a",
            PeerMessage::RouteAnnounce {
                name: "grid".into(),
                relay_id: "relay-a".into(),
                relay_url: "http://relay-a:8080".into(),
                crm_ns: "cc.demo".into(),
                crm_name: "Grid".into(),
                crm_ver: "0.1.0".into(),
                abi_hash: "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef".into(),
                signature_hash: "abcdef0123456789abcdef0123456789abcdef0123456789abcdef0123456789"
                    .into(),
                max_payload_size: 1024,
                route_uid: "grid-route-uid-0001".into(),
                route_revision: 1,
                registered_at: 1000.0,
            },
        );
        let json = serde_json::to_string(&env).unwrap();
        let decoded: PeerEnvelope = serde_json::from_str(&json).unwrap();
        match decoded.message {
            PeerMessage::RouteAnnounce { name, crm_name, .. } => {
                assert_eq!(name, "grid");
                assert_eq!(crm_name, "Grid");
            }
            _ => panic!("wrong variant"),
        }
    }

    #[test]
    fn route_state_validator_rejects_old_route_announce() {
        let mut env = PeerEnvelope::new(
            "relay-a",
            PeerMessage::RouteAnnounce {
                name: "grid".into(),
                relay_id: "relay-a".into(),
                relay_url: "http://relay-a:8080".into(),
                crm_ns: "cc.demo".into(),
                crm_name: "Grid".into(),
                crm_ver: "0.1.0".into(),
                abi_hash: "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef".into(),
                signature_hash: "abcdef0123456789abcdef0123456789abcdef0123456789abcdef0123456789"
                    .into(),
                max_payload_size: 1024,
                route_uid: "grid-route-uid-0001".into(),
                route_revision: 1,
                registered_at: 1000.0,
            },
        );
        env.protocol_version = ROUTE_HASH_PEER_VERSION - 1;

        assert!(matches!(
            validate_route_state_envelope(env, Some("relay-a")),
            Err(PeerRouteStateError::RouteStateProtocolTooOld { .. }),
        ));
    }

    #[test]
    fn route_state_validator_rejects_digest_diff_hash_replay() {
        let mut active = DigestDiffEntry::Active {
            name: "grid".into(),
            relay_id: "relay-a".into(),
            relay_url: "http://relay-a:8080".into(),
            crm_ns: "cc.demo".into(),
            crm_name: "Grid".into(),
            crm_ver: "0.1.0".into(),
            abi_hash: "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef".into(),
            signature_hash: "abcdef0123456789abcdef0123456789abcdef0123456789abcdef0123456789"
                .into(),
            max_payload_size: 1024,
            route_uid: "grid-route-uid-0001".into(),
            route_revision: 1,
            registered_at: 1000.0,
            hash: String::new(),
        };
        let valid_hash = route_digest_hash_for_diff_entry(&active).unwrap();
        if let DigestDiffEntry::Active { hash, .. } = &mut active {
            *hash = valid_hash;
        }
        if let DigestDiffEntry::Active { name, .. } = &mut active {
            *name = "other".into();
        }
        let env = PeerEnvelope::new(
            "relay-a",
            PeerMessage::DigestDiff {
                entries: vec![active],
            },
        );

        assert!(matches!(
            validate_route_state_envelope(env, Some("relay-a")),
            Err(PeerRouteStateError::InvalidDigestDiff { .. }),
        ));
    }

    #[test]
    fn deleted_digest_diff_hash_binds_removed_revision() {
        let mut deleted = DigestDiffEntry::Deleted {
            name: "grid".into(),
            relay_id: "relay-a".into(),
            removed_at: 1001.0,
            removed_revision: 7,
            hash: String::new(),
        };
        let valid_hash = route_digest_hash_for_diff_entry(&deleted).unwrap();
        if let DigestDiffEntry::Deleted { hash, .. } = &mut deleted {
            *hash = valid_hash;
        }
        if let DigestDiffEntry::Deleted {
            removed_revision, ..
        } = &mut deleted
        {
            *removed_revision = 8;
        }
        let env = PeerEnvelope::new(
            "relay-a",
            PeerMessage::DigestDiff {
                entries: vec![deleted],
            },
        );

        assert!(matches!(
            validate_route_state_envelope(env, Some("relay-a")),
            Err(PeerRouteStateError::InvalidDigestDiff { .. }),
        ));
    }
}
