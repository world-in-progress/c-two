use serde::{Deserialize, Serialize};

pub const PROTOCOL_VERSION: u32 = 1;

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
        ipc_address: Option<String>,
        crm_ns: String,
        crm_ver: String,
        registered_at: f64,
    },
    /// A CRM route was unregistered.
    RouteWithdraw {
        name: String,
        relay_id: String,
    },
    /// A relay wants to join the mesh.
    RelayJoin {
        relay_id: String,
        url: String,
    },
    /// A relay is gracefully leaving the mesh.
    RelayLeave {
        relay_id: String,
    },
    /// Periodic heartbeat.
    Heartbeat {
        relay_id: String,
        route_count: u32,
    },
    /// Anti-entropy digest exchange (request or response).
    DigestExchange {
        digest: Vec<DigestEntry>,
    },
    /// Anti-entropy diff: entries the sender has that recipient was missing.
    DigestDiff {
        entries: Vec<DigestDiffEntry>,
        /// Routes the sender doesn't have but recipient does — should be deleted.
        extra: Vec<(String, String)>,
    },
    /// Unknown message type from a newer protocol version.
    #[serde(other)]
    Unknown,
}

/// One entry in a digest map.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DigestEntry {
    pub name: String,
    pub relay_id: String,
    pub hash: u64,
}

/// Route data sent to repair digest differences.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DigestDiffEntry {
    pub name: String,
    pub relay_id: String,
    pub relay_url: String,
    pub ipc_address: Option<String>,
    pub crm_ns: String,
    pub crm_ver: String,
    pub registered_at: f64,
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
                ipc_address: Some("ipc://grid".into()),
                crm_ns: "cc.demo".into(),
                crm_ver: "0.1.0".into(),
                registered_at: 1000.0,
            },
        );
        let json = serde_json::to_string(&env).unwrap();
        let decoded: PeerEnvelope = serde_json::from_str(&json).unwrap();
        match decoded.message {
            PeerMessage::RouteAnnounce { name, .. } => assert_eq!(name, "grid"),
            _ => panic!("wrong variant"),
        }
    }
}
