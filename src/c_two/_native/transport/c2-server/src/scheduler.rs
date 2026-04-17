use std::collections::HashMap;
use tokio::sync::RwLock;

/// Method access level (from CRM contract `@cc.read` / `@cc.write`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AccessLevel {
    Read,
    Write,
}

/// Concurrency mode for a CRM.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ConcurrencyMode {
    /// No locking — all methods run concurrently.
    Parallel,
    /// Single-threaded — one method at a time.
    Exclusive,
    /// Reader-writer lock — reads concurrent, writes exclusive.
    ReadParallel,
}

/// Per-CRM scheduler controlling method execution concurrency.
///
/// The scheduler acquires the appropriate lock *before* `spawn_blocking` and
/// holds it until the blocking task completes. This ensures the concurrency
/// invariant is maintained across the async/blocking boundary.
pub struct Scheduler {
    mode: ConcurrencyMode,
    rw_lock: RwLock<()>,
    /// method_idx → access level
    access_map: HashMap<u16, AccessLevel>,
}

impl Scheduler {
    pub fn new(mode: ConcurrencyMode, access_map: HashMap<u16, AccessLevel>) -> Self {
        Self {
            mode,
            rw_lock: RwLock::new(()),
            access_map,
        }
    }

    /// Execute a CRM method under the appropriate concurrency guard.
    ///
    /// `f` runs inside `spawn_blocking` (Python GIL acquisition happens there).
    /// The lock guard is held on the async task until `spawn_blocking` returns,
    /// preventing concurrent writes while a CRM method executes.
    pub async fn execute<F, R>(&self, method_idx: u16, f: F) -> R
    where
        F: FnOnce() -> R + Send + 'static,
        R: Send + 'static,
    {
        match self.mode {
            ConcurrencyMode::Parallel => {
                tokio::task::spawn_blocking(f).await.unwrap()
            }
            ConcurrencyMode::Exclusive => {
                let _guard = self.rw_lock.write().await;
                tokio::task::spawn_blocking(f).await.unwrap()
            }
            ConcurrencyMode::ReadParallel => {
                let access = self
                    .access_map
                    .get(&method_idx)
                    .copied()
                    .unwrap_or(AccessLevel::Write);
                match access {
                    AccessLevel::Read => {
                        let _guard = self.rw_lock.read().await;
                        tokio::task::spawn_blocking(f).await.unwrap()
                    }
                    AccessLevel::Write => {
                        let _guard = self.rw_lock.write().await;
                        tokio::task::spawn_blocking(f).await.unwrap()
                    }
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicU32, Ordering};
    use std::sync::Arc;

    fn empty_map() -> HashMap<u16, AccessLevel> {
        HashMap::new()
    }

    fn read_write_map() -> HashMap<u16, AccessLevel> {
        let mut m = HashMap::new();
        m.insert(0, AccessLevel::Read);
        m.insert(1, AccessLevel::Write);
        m
    }

    #[tokio::test]
    async fn parallel_allows_concurrent_execution() {
        let sched = Arc::new(Scheduler::new(ConcurrencyMode::Parallel, empty_map()));
        let counter = Arc::new(AtomicU32::new(0));
        let barrier = Arc::new(tokio::sync::Barrier::new(3));

        let mut handles = Vec::new();
        for _ in 0..3 {
            let s = Arc::clone(&sched);
            let c = Arc::clone(&counter);
            let b = Arc::clone(&barrier);
            handles.push(tokio::spawn(async move {
                s.execute(0, move || {
                    c.fetch_add(1, Ordering::SeqCst);
                    // Use std::thread blocking to wait on the tokio barrier
                    // via a oneshot channel pattern.
                    let rt = tokio::runtime::Handle::current();
                    rt.block_on(b.wait());
                })
                .await;
            }));
        }
        for h in handles {
            h.await.unwrap();
        }
        // All 3 tasks reached the barrier, proving concurrent execution.
        assert_eq!(counter.load(Ordering::SeqCst), 3);
    }

    #[tokio::test]
    async fn exclusive_serializes_execution() {
        let sched = Arc::new(Scheduler::new(ConcurrencyMode::Exclusive, empty_map()));
        let max_concurrent = Arc::new(AtomicU32::new(0));
        let active = Arc::new(AtomicU32::new(0));

        let mut handles = Vec::new();
        for _ in 0..5 {
            let s = Arc::clone(&sched);
            let mc = Arc::clone(&max_concurrent);
            let ac = Arc::clone(&active);
            handles.push(tokio::spawn(async move {
                s.execute(0, move || {
                    let cur = ac.fetch_add(1, Ordering::SeqCst) + 1;
                    mc.fetch_max(cur, Ordering::SeqCst);
                    std::thread::sleep(std::time::Duration::from_millis(10));
                    ac.fetch_sub(1, Ordering::SeqCst);
                })
                .await;
            }));
        }
        for h in handles {
            h.await.unwrap();
        }
        assert_eq!(max_concurrent.load(Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn read_parallel_allows_concurrent_reads() {
        let sched = Arc::new(Scheduler::new(
            ConcurrencyMode::ReadParallel,
            read_write_map(),
        ));
        let counter = Arc::new(AtomicU32::new(0));
        let barrier = Arc::new(tokio::sync::Barrier::new(3));

        let mut handles = Vec::new();
        for _ in 0..3 {
            let s = Arc::clone(&sched);
            let c = Arc::clone(&counter);
            let b = Arc::clone(&barrier);
            // method_idx=0 → Read
            handles.push(tokio::spawn(async move {
                s.execute(0, move || {
                    c.fetch_add(1, Ordering::SeqCst);
                    let rt = tokio::runtime::Handle::current();
                    rt.block_on(b.wait());
                })
                .await;
            }));
        }
        for h in handles {
            h.await.unwrap();
        }
        assert_eq!(counter.load(Ordering::SeqCst), 3);
    }

    #[tokio::test]
    async fn read_parallel_serializes_writes() {
        let sched = Arc::new(Scheduler::new(
            ConcurrencyMode::ReadParallel,
            read_write_map(),
        ));
        let max_concurrent = Arc::new(AtomicU32::new(0));
        let active = Arc::new(AtomicU32::new(0));

        let mut handles = Vec::new();
        for _ in 0..5 {
            let s = Arc::clone(&sched);
            let mc = Arc::clone(&max_concurrent);
            let ac = Arc::clone(&active);
            // method_idx=1 → Write
            handles.push(tokio::spawn(async move {
                s.execute(1, move || {
                    let cur = ac.fetch_add(1, Ordering::SeqCst) + 1;
                    mc.fetch_max(cur, Ordering::SeqCst);
                    std::thread::sleep(std::time::Duration::from_millis(10));
                    ac.fetch_sub(1, Ordering::SeqCst);
                })
                .await;
            }));
        }
        for h in handles {
            h.await.unwrap();
        }
        assert_eq!(max_concurrent.load(Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn unknown_method_idx_defaults_to_write() {
        let sched = Arc::new(Scheduler::new(
            ConcurrencyMode::ReadParallel,
            read_write_map(),
        ));
        let max_concurrent = Arc::new(AtomicU32::new(0));
        let active = Arc::new(AtomicU32::new(0));

        let mut handles = Vec::new();
        for _ in 0..5 {
            let s = Arc::clone(&sched);
            let mc = Arc::clone(&max_concurrent);
            let ac = Arc::clone(&active);
            // method_idx=99 → not in map → defaults to Write
            handles.push(tokio::spawn(async move {
                s.execute(99, move || {
                    let cur = ac.fetch_add(1, Ordering::SeqCst) + 1;
                    mc.fetch_max(cur, Ordering::SeqCst);
                    std::thread::sleep(std::time::Duration::from_millis(10));
                    ac.fetch_sub(1, Ordering::SeqCst);
                })
                .await;
            }));
        }
        for h in handles {
            h.await.unwrap();
        }
        // Unknown methods default to Write → serialized → max 1 concurrent
        assert_eq!(max_concurrent.load(Ordering::SeqCst), 1);
    }
}
