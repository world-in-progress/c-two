use std::sync::Arc;
use tokio::task::JoinHandle;

use crate::relay::peer::{PeerEnvelope, PeerMessage};
use crate::relay::peer_client::{CONTROL_TIMEOUT, PeerHttpClient};
use crate::relay::types::{PeerAddress, PeerSnapshot, PeerStatus};

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
    peer_client: Arc<PeerHttpClient>,
}

impl FullBroadcast {
    pub(super) fn new(peer_client: Arc<PeerHttpClient>) -> Self {
        Self { peer_client }
    }

    #[cfg(test)]
    pub(super) fn with_proxy_policy(use_proxy: bool) -> Self {
        let config = c2_config::RelayConfig {
            use_proxy,
            ..Default::default()
        };
        Self::new(Arc::new(
            PeerHttpClient::new(&config, None, None).expect("test peer client"),
        ))
    }

    async fn send_to_peer(client: &PeerHttpClient, address: &PeerAddress, envelope: &PeerEnvelope) {
        let endpoint = match &envelope.message {
            PeerMessage::RouteAnnounce { .. } | PeerMessage::RouteWithdraw { .. } => {
                "/_peer/announce"
            }
            PeerMessage::RelayJoin { .. } => "/_peer/join",
            PeerMessage::RelayLeave { .. } => "/_peer/leave",
            PeerMessage::RelayForceLeave { .. } => "/_peer/force-leave",
            PeerMessage::Heartbeat { .. } => "/_peer/heartbeat",
            PeerMessage::DigestExchange { .. } | PeerMessage::DigestDiff { .. } => "/_peer/digest",
        };
        let _ = tokio::time::timeout(
            CONTROL_TIMEOUT,
            client.post_control(address, endpoint, envelope),
        )
        .await;
    }
}

impl Disseminator for FullBroadcast {
    fn broadcast(&self, envelope: PeerEnvelope, peers: &[PeerSnapshot]) -> Option<JoinHandle<()>> {
        let alive: Vec<PeerAddress> = peers
            .iter()
            .filter(|p| p.status == PeerStatus::Alive)
            .map(|p| p.address.clone())
            .collect();

        if alive.is_empty() {
            return None;
        }

        let client = self.peer_client.clone();
        Some(tokio::spawn(async move {
            let futs: Vec<_> = alive
                .iter()
                .map(|address| Self::send_to_peer(&client, address, &envelope))
                .collect();
            futures::future::join_all(futs).await;
        }))
    }
}
