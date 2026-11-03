//! Compile only: a read projection must not expose the original mutable lock.
#![forbid(unsafe_code)]

fn replace(server: &c2_server::Server) -> c2_mem::MemPool {
    let pool = server.response_pool_arc();
    let read = pool.read();
    let raw: &parking_lot::RwLock<c2_mem::MemPool> = parking_lot::RwLockReadGuard::rwlock(&read);
    drop(read);
    let mut write = raw.write();
    let replacement = c2_mem::MemPool::new(write.config().clone()).unwrap();
    std::mem::replace(&mut *write, replacement)
}

fn main() {}
