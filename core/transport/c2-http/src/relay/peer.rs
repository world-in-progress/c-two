use serde::{Deserialize, Serialize};

use crate::relay::types::{
    Locality, RouteDigestHash, RouteEntry, valid_route_digest_hash, valid_wire_relay_id,
};

pub const PROTOCOL_VERSION: u32 = 7;

/// Envelope wrapping every peer-to-peer message.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PeerEnvelope {
    pub protocol_version: u32,
    pub sender_relay_id: String,
    pub message: PeerMessage,
}

/// Gossip message variants.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", deny_unknown_fields)]
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

        registered_at: f64,
    },
    /// A CRM route was unregistered.
    RouteWithdraw {
        name: String,
        relay_id: String,
        removed_at: f64,
    },
    /// A relay wants to join the mesh.
    RelayJoin { relay_id: String, url: String },
    /// A relay is gracefully leaving the mesh.
    RelayLeave { relay_id: String },
    /// A deployer asks peers that cannot reach a relay to forget it.
    RelayForceLeave { relay_id: String },
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
#[serde(tag = "state", deny_unknown_fields)]
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

        registered_at: f64,
    },
    Deleted {
        name: String,
        relay_id: String,
        removed_at: f64,
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

#[derive(Debug)]
pub(crate) enum ValidatedRouteStateEnvelope {
    Message(PeerEnvelope),
    DigestDiff(Vec<ValidatedDigestDiffEntry>),
}

#[derive(Debug)]
pub(crate) enum ValidatedDigestDiffEntry {
    Active(RouteEntry),
    Deleted {
        name: String,
        relay_id: String,
        removed_at: f64,
    },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PeerRouteStateError {
    UnsupportedRouteStateProtocol { got: u32, supported: u32 },
    InvalidSender { sender_relay_id: String },
    InvalidRouteAnnounce { reason: String },
    InvalidRouteWithdraw { reason: String },
    InvalidRelayLeave { reason: String },
    InvalidRelayForceLeave { reason: String },
    InvalidDigestEntry { reason: String },
    InvalidDigestDiff { reason: String },
}

pub fn validate_route_state_envelope(
    mut envelope: PeerEnvelope,
    expected_sender: Option<&str>,
) -> Result<ValidatedRouteStateEnvelope, PeerRouteStateError> {
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

    match &mut envelope.message {
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

            registered_at,
        } => {
            if relay_id.as_str() != envelope.sender_relay_id
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
                    *registered_at,
                )
            {
                return Err(PeerRouteStateError::InvalidRouteAnnounce {
                    reason: "invalid route announcement".to_string(),
                });
            }
        }
        PeerMessage::RouteWithdraw {
            name,
            relay_id,
            removed_at,
        } => {
            if relay_id.as_str() != envelope.sender_relay_id
                || !valid_deleted_route_fields(name, relay_id, *removed_at)
            {
                return Err(PeerRouteStateError::InvalidRouteWithdraw {
                    reason: "invalid route withdrawal".to_string(),
                });
            }
        }
        PeerMessage::RelayLeave { relay_id } => {
            if relay_id.as_str() != envelope.sender_relay_id || !valid_wire_relay_id(relay_id) {
                return Err(PeerRouteStateError::InvalidRelayLeave {
                    reason: "invalid relay leave".to_string(),
                });
            }
        }
        PeerMessage::RelayForceLeave { relay_id } => {
            if relay_id.as_str() == envelope.sender_relay_id || !valid_wire_relay_id(relay_id) {
                return Err(PeerRouteStateError::InvalidRelayForceLeave {
                    reason: "invalid relay force-leave".to_string(),
                });
            }
        }
        PeerMessage::DigestExchange { digest } => {
            validate_digest_entries(digest)?;
        }
        PeerMessage::DigestDiff { entries } => {
            return validate_digest_diff_entries(
                &envelope.sender_relay_id,
                std::mem::take(entries),
            )
            .map(ValidatedRouteStateEnvelope::DigestDiff);
        }
        PeerMessage::RelayJoin { .. } | PeerMessage::Heartbeat { .. } | PeerMessage::Unknown => {}
    };

    Ok(ValidatedRouteStateEnvelope::Message(envelope))
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
    entries: Vec<DigestDiffEntry>,
) -> Result<Vec<ValidatedDigestDiffEntry>, PeerRouteStateError> {
    let mut validated = Vec::with_capacity(entries.len());
    for entry in entries {
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

                registered_at,
            } => {
                if relay_id != sender_relay_id
                    || !valid_active_route_fields(
                        &name,
                        &relay_id,
                        &relay_url,
                        &crm_ns,
                        &crm_name,
                        &crm_ver,
                        &abi_hash,
                        &signature_hash,
                        max_payload_size,
                        registered_at,
                    )
                {
                    return Err(PeerRouteStateError::InvalidDigestDiff {
                        reason: format!("invalid active diff for route {name}/{relay_id}"),
                    });
                }
                validated.push(ValidatedDigestDiffEntry::Active(RouteEntry {
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

                    route_uid: None,
                    locality: Locality::Peer,
                    registered_at,
                }));
            }
            DigestDiffEntry::Deleted {
                name,
                relay_id,
                removed_at,
            } => {
                if relay_id != sender_relay_id
                    || !valid_deleted_route_fields(&name, &relay_id, removed_at)
                {
                    return Err(PeerRouteStateError::InvalidDigestDiff {
                        reason: format!("invalid deleted diff for route {name}/{relay_id}"),
                    });
                }
                validated.push(ValidatedDigestDiffEntry::Deleted {
                    name,
                    relay_id,
                    removed_at,
                });
            }
        }
    }
    Ok(validated)
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

    registered_at: f64,
) -> bool {
    crate::relay::route_table::valid_route_name(name)
        && valid_wire_relay_id(relay_id)
        && crate::relay::route_table::valid_relay_url(relay_url)
        && crate::relay::route_table::valid_crm_tag(crm_ns, crm_name, crm_ver)
        && c2_contract::validate_contract_hash("abi_hash", abi_hash).is_ok()
        && c2_contract::validate_contract_hash("signature_hash", signature_hash).is_ok()
        && max_payload_size > 0
        && registered_at.is_finite()
}

fn valid_deleted_route_fields(name: &str, relay_id: &str, removed_at: f64) -> bool {
    crate::relay::route_table::valid_route_name(name)
        && valid_wire_relay_id(relay_id)
        && removed_at.is_finite()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn mesh_seven_rejects_six_and_future_envelopes() {
        assert_eq!(PROTOCOL_VERSION, 7);
        for protocol_version in [6, 8] {
            let mut envelope = PeerEnvelope::new(
                "relay-a",
                PeerMessage::Heartbeat {
                    relay_id: "relay-a".into(),
                    route_count: 0,
                },
            );
            envelope.protocol_version = protocol_version;
            assert!(matches!(
                validate_route_state_envelope(envelope, None),
                Err(PeerRouteStateError::UnsupportedRouteStateProtocol { got, supported: 7 })
                    if got == protocol_version
            ));
        }
    }

    #[test]
    fn mesh_seven_rejects_retired_fields_on_route_messages_and_diffs() {
        let active = serde_json::json!({
            "name": "grid", "relay_id": "relay-a", "relay_url": "http://relay-a:8080",
            "crm_ns": "test.ns", "crm_name": "Grid", "crm_ver": "0.1.0",
            "abi_hash": "a".repeat(64), "signature_hash": "b".repeat(64),
            "max_payload_size": 1024, "registered_at": 1000.0,
        });
        let deleted = serde_json::json!({
            "name": "grid", "relay_id": "relay-a", "removed_at": 1001.0,
        });
        for (fields, kind, diff, retired_fields) in [
            (
                active.clone(),
                "RouteAnnounce",
                false,
                vec!["route_uid", "route_revision"],
            ),
            (
                deleted.clone(),
                "RouteWithdraw",
                false,
                vec!["removed_revision"],
            ),
            (
                active,
                "Active",
                true,
                vec!["route_uid", "hash", "route_revision"],
            ),
            (deleted, "Deleted", true, vec!["hash", "removed_revision"]),
        ] {
            for retired in retired_fields {
                let mut message = fields.clone();
                message[if diff { "state" } else { "type" }] = kind.into();
                message[retired] = 1.into();
                if diff {
                    message = serde_json::json!({"type": "DigestDiff", "entries": [message]});
                }
                let envelope = serde_json::json!({
                    "protocol_version": 7, "sender_relay_id": "relay-a", "message": message,
                });
                assert!(
                    serde_json::from_value::<PeerEnvelope>(envelope).is_err(),
                    "{kind}.{retired}"
                );
            }
        }
    }

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

                registered_at: 1000.0,
            },
        );
        env.protocol_version = PROTOCOL_VERSION - 1;

        assert!(matches!(
            validate_route_state_envelope(env, Some("relay-a")),
            Err(PeerRouteStateError::UnsupportedRouteStateProtocol { .. }),
        ));
    }
}
