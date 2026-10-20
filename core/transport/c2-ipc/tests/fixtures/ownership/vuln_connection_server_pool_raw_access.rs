//! R02 baseline (merged): Connection is publicly constructible and serves
//! bare-coordinate read/free; ServerPoolState's bare-coordinate methods and
//! public raw pool field still type-check on a borrowed state. Never run.
#![forbid(unsafe_code)]

use c2_ipc::ServerPoolState;
use c2_server::Connection;

#[allow(dead_code)]
fn raw_server_pool_access(state: &mut ServerPoolState) -> Result<(), String> {
    state.copy_response(0, 1, 0, 4096, false)?;
    state.release_response(0, 1, 0, 4096, false, 7)?;
    let _ = &state.pool;
    Ok(())
}

fn main() {
    let connection = Connection::new(41);
    connection.init_peer_shm("/forged-peer-prefix".to_string());
    let read = connection.read_peer_data(0, 1, 0, 4096, false);
    let freed = connection.free_peer_block(0, 1, 0, 4096, false, 7);
    let _ = (read.is_ok(), matches!(freed, c2_mem::FreeResult::Normal));
}
