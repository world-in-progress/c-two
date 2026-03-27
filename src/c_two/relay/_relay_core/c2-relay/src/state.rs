//! Shared application state for the relay server.

use std::sync::Arc;
use tokio::sync::RwLock;

use c2_ipc::IpcClient;

/// Shared relay state, held in axum's state extractor.
pub struct RelayState {
    pub client: Arc<RwLock<IpcClient>>,
}

impl RelayState {
    pub fn new(client: IpcClient) -> Self {
        Self {
            client: Arc::new(RwLock::new(client)),
        }
    }
}

impl Clone for RelayState {
    fn clone(&self) -> Self {
        Self {
            client: self.client.clone(),
        }
    }
}
