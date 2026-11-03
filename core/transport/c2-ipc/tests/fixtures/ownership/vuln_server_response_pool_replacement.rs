//! Compile only: callback allocation cannot replace the advertised response pool.
#![forbid(unsafe_code)]

use c2_mem::MemPool;
use c2_server::Server;

fn replace(server: &Server) {
    let pool = server.response_pool_arc();
    let mut guard = pool.write();
    let replacement = MemPool::new(guard.config().clone()).unwrap();
    let _advertised_pool = std::mem::replace(&mut *guard, replacement);
}

fn main() {}
