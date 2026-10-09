//! Bounded Relay ownership of native clients, including unconfirmed cleanup.
//!
//! A slot is reserved before connecting. Unique-client cancellation hands the
//! client to this domain; published clients remain owned until confirmed close.
//! One observer retries native cleanup. Its runtime must outlive shutdown.
use std::ops::{Deref, DerefMut};
use std::sync::Arc;
use std::time::Duration;

use c2_ipc::{IpcClient, IpcError};
use parking_lot::Mutex;
use tokio::sync::Notify;

const MAX_CLIENT_OWNERS: usize = 1024;

struct Entry {
    client: Option<Arc<IpcClient>>,
    closing: bool,
}

struct Inner {
    entries: Vec<Option<Entry>>,
    closed: bool,
}

pub(crate) struct ClientLifecycle {
    inner: Mutex<Inner>,
    changed: Notify,
    drained: Notify,
}

pub(crate) struct ManagedClient {
    client: Option<IpcClient>,
    domain: Arc<ClientLifecycle>,
    slot: usize,
}

impl Deref for ManagedClient {
    type Target = IpcClient;
    fn deref(&self) -> &IpcClient {
        self.client.as_ref().expect("managed client published")
    }
}
impl DerefMut for ManagedClient {
    fn deref_mut(&mut self) -> &mut IpcClient {
        self.client.as_mut().expect("managed client published")
    }
}
impl ManagedClient {
    pub(crate) fn into_shared(mut self) -> Arc<IpcClient> {
        let client = Arc::new(self.client.take().expect("managed client published"));
        self.domain.publish(self.slot, client.clone(), false);
        client
    }
}
impl Drop for ManagedClient {
    fn drop(&mut self) {
        if let Some(client) = self.client.take() {
            self.domain.publish(self.slot, Arc::new(client), true);
        }
    }
}

impl ClientLifecycle {
    pub(crate) fn new() -> Arc<Self> {
        Arc::new(Self {
            inner: Mutex::new(Inner {
                entries: Vec::new(),
                closed: false,
            }),
            changed: Notify::new(),
            drained: Notify::new(),
        })
    }

    pub(crate) fn manage(
        self: &Arc<Self>,
        construct: impl FnOnce() -> IpcClient,
    ) -> Result<ManagedClient, IpcError> {
        let mut inner = self.inner.lock();
        if inner.closed {
            return Err(IpcError::Protocol(
                "relay native client admission closed".into(),
            ));
        }
        let slot = if let Some(slot) = inner.entries.iter().position(Option::is_none) {
            slot
        } else if inner.entries.len() < MAX_CLIENT_OWNERS {
            inner.entries.push(None);
            inner.entries.len() - 1
        } else {
            return Err(IpcError::Protocol(
                "relay native client owner capacity exhausted".into(),
            ));
        };
        inner.entries[slot] = Some(Entry {
            client: None,
            closing: false,
        });
        drop(inner);
        Ok(ManagedClient {
            client: Some(construct()),
            domain: self.clone(),
            slot,
        })
    }

    fn publish(&self, slot: usize, client: Arc<IpcClient>, closing: bool) {
        let mut inner = self.inner.lock();
        let closed = inner.closed;
        let entry = inner.entries[slot].as_mut().expect("reserved native owner");
        entry.client = Some(client);
        entry.closing = closing || closed;
        drop(inner);
        self.changed.notify_one();
    }

    pub(crate) fn close(&self, client: &Arc<IpcClient>) {
        let mut inner = self.inner.lock();
        for entry in inner.entries.iter_mut().flatten() {
            if entry
                .client
                .as_ref()
                .is_some_and(|owned| Arc::ptr_eq(owned, client))
            {
                entry.closing = true;
                self.changed.notify_one();
                return;
            }
        }
        // Production clients are always registered before connection I/O.
        // Test-only manually inserted clients have no native runtime work.
        debug_assert!(!client.is_connected(), "unregistered relay native client");
    }

    pub(crate) async fn run(&self) {
        loop {
            let notified = self.changed.notified();
            tokio::pin!(notified);
            notified.as_mut().enable();
            let work = {
                let inner = self.inner.lock();
                if inner.closed && inner.entries.iter().all(Option::is_none) {
                    self.drained.notify_waiters();
                    return;
                }
                inner
                    .entries
                    .iter()
                    .enumerate()
                    .filter_map(|(slot, entry)| {
                        let entry = entry.as_ref()?;
                        let client = entry.client.as_ref()?;
                        (entry.closing || !client.is_connected() || Arc::strong_count(client) == 1)
                            .then(|| (slot, client.clone()))
                    })
                    .collect::<Vec<_>>()
            };
            for (slot, client) in work {
                if client.close_shared_bounded(Duration::from_millis(50)).await {
                    self.inner.lock().entries[slot] = None;
                    self.drained.notify_waiters();
                }
                // false retains the exact owner and its retry observer.
            }
            tokio::select! {
                _ = notified => {},
                _ = tokio::time::sleep(Duration::from_millis(10)) => {},
            }
        }
    }

    pub(crate) async fn shutdown(&self) {
        {
            let mut inner = self.inner.lock();
            inner.closed = true;
            for entry in inner.entries.iter_mut().flatten() {
                entry.closing = true;
            }
        }
        self.changed.notify_one();
        loop {
            let notified = self.drained.notified();
            tokio::pin!(notified);
            notified.as_mut().enable();
            if self.inner.lock().entries.iter().all(Option::is_none) {
                return;
            }
            notified.await;
        }
    }

    #[cfg(test)]
    pub(crate) fn adopt_for_test(&self, client: &Arc<IpcClient>) {
        let mut inner = self.inner.lock();
        if inner.entries.iter().flatten().any(|entry| {
            entry
                .client
                .as_ref()
                .is_some_and(|owned| Arc::ptr_eq(owned, client))
        }) {
            return;
        }
        assert!(!inner.closed);
        let entry = Some(Entry {
            client: Some(client.clone()),
            closing: false,
        });
        if let Some(slot) = inner.entries.iter().position(Option::is_none) {
            inner.entries[slot] = entry;
        } else {
            assert!(inner.entries.len() < MAX_CLIENT_OWNERS);
            inner.entries.push(entry);
        }
    }

    #[cfg(test)]
    pub(crate) fn outstanding(&self) -> usize {
        self.inner.lock().entries.iter().flatten().count()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn native_owner_capacity_includes_unpublished_and_closing_clients() {
        let domain = ClientLifecycle::new();
        let mut reserved = Vec::new();
        for _ in 0..MAX_CLIENT_OWNERS {
            reserved.push(
                domain
                    .manage(|| IpcClient::new("ipc://not-connected"))
                    .unwrap(),
            );
        }
        assert!(
            domain
                .manage(|| IpcClient::new("ipc://not-connected"))
                .is_err()
        );
        assert!(
            domain
                .manage(|| panic!("exhausted ownership must reject before client allocation"))
                .is_err()
        );
        assert_eq!(domain.outstanding(), MAX_CLIENT_OWNERS);
        // Unique candidates (including failed acquisition) cannot lose their
        // cleanup observer on future cancellation; dropping transfers ownership.
        drop(reserved);
        assert_eq!(domain.outstanding(), MAX_CLIENT_OWNERS);
        let observer = {
            let domain = domain.clone();
            tokio::spawn(async move { domain.run().await })
        };
        tokio::time::timeout(Duration::from_secs(10), domain.shutdown())
            .await
            .unwrap();
        observer.await.unwrap();
        assert_eq!(domain.outstanding(), 0);
        assert!(
            domain
                .manage(|| IpcClient::new("ipc://not-connected"))
                .is_err()
        );
    }

    #[tokio::test]
    async fn shutdown_observes_reserved_candidate_until_ownership_is_published() {
        let domain = ClientLifecycle::new();
        let candidate = domain
            .manage(|| IpcClient::new("ipc://not-connected"))
            .unwrap();
        let observer = {
            let domain = domain.clone();
            tokio::spawn(async move { domain.run().await })
        };
        assert!(
            tokio::time::timeout(Duration::from_millis(20), domain.shutdown())
                .await
                .is_err()
        );
        assert_eq!(domain.outstanding(), 1);
        drop(candidate);
        tokio::time::timeout(Duration::from_secs(2), domain.shutdown())
            .await
            .unwrap();
        observer.await.unwrap();
        assert_eq!(domain.outstanding(), 0);
    }
}
