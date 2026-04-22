use tokio::task::JoinHandle;

use crate::relay::peer::PeerEnvelope;
use crate::relay::types::PeerSnapshot;

/// Trait for broadcasting gossip messages to peers.
pub trait Disseminator: Send + Sync {
    /// Broadcast a message to all relevant peers.
    /// Returns a handle that can be awaited when delivery matters
    /// (e.g. shutdown leave). Callers may ignore it for fire-and-forget.
    fn broadcast(&self, envelope: PeerEnvelope, peers: &[PeerSnapshot]) -> Option<JoinHandle<()>>;
}

/// Full broadcast — sends to every Alive peer.
/// Suitable for clusters with <100 relays.
pub struct FullBroadcast {
    http_client: reqwest::Client,
}

impl FullBroadcast {
    pub fn new() -> Self {
        Self {
            http_client: crate::relay_client_builder()
                .timeout(std::time::Duration::from_secs(5))
            .build()
                .expect("c-two: failed to build reqwest Client for relay traffic"),
        }
    }

    async fn send_to_peer(client: &reqwest::Client, url: &str, envelope: &PeerEnvelope) {
        let endpoint = match &envelope.message {
            crate::relay::peer::PeerMessage::RouteAnnounce { .. } => "/_peer/announce",
            crate::relay::peer::PeerMessage::RouteWithdraw { .. } => "/_peer/announce",
            crate::relay::peer::PeerMessage::RelayJoin { .. } => "/_peer/join",
            crate::relay::peer::PeerMessage::RelayLeave { .. } => "/_peer/leave",
            crate::relay::peer::PeerMessage::Heartbeat { .. } => "/_peer/heartbeat",
            crate::relay::peer::PeerMessage::DigestExchange { .. } => "/_peer/digest",
            crate::relay::peer::PeerMessage::DigestDiff { .. } => "/_peer/digest",
            crate::relay::peer::PeerMessage::Unknown => return,
        };
        let full_url = format!("{url}{endpoint}");
        let _ = client.post(&full_url).json(envelope).send().await;
    }
}

impl Disseminator for FullBroadcast {
    fn broadcast(&self, envelope: PeerEnvelope, peers: &[PeerSnapshot]) -> Option<JoinHandle<()>> {
        use crate::relay::types::PeerStatus;
        let alive: Vec<String> = peers
            .iter()
            .filter(|p| p.status == PeerStatus::Alive)
            .map(|p| p.url.clone())
            .collect();

        if alive.is_empty() {
            return None;
        }

        let client = self.http_client.clone();
        Some(tokio::spawn(async move {
            let futs: Vec<_> = alive
                .iter()
                .map(|url| Self::send_to_peer(&client, url, &envelope))
                .collect();
            futures::future::join_all(futs).await;
        }))
    }
}
