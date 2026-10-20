//! A received request must not expose a pool that safe callers can replace.
//! Compile only: no connection is created and no backing is accessed.
#![forbid(unsafe_code)]
use c2_mem::MemPool;
use c2_server::Connection;

fn replace_peer(connection: &Connection, replacement: MemPool) {
    *connection.peer_pool_arc().unwrap().write() = replacement;
}

fn replace_admitted_peer(connection: &Connection, replacement: MemPool) {
    *connection
        .ensure_and_get_peer_pool(0, 1, 4096, false)
        .unwrap()
        .write() = replacement;
}

fn main() {}
