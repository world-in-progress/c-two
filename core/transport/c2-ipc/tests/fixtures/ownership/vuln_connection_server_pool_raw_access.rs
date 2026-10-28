//! A connection's ServerPoolState must not expose bare-coordinate methods
//! or its raw pool. Retired Connection raw read/free methods have no probe
//! call sites. Compiled only, never run.
#![forbid(unsafe_code)]

use c2_ipc::ServerPoolState;

#[allow(dead_code)]
fn raw_server_pool_access(state: &mut ServerPoolState) -> Result<(), String> {
    state.copy_response(0, 1, 0, 4096, false)?;
    state.release_response(0, 1, 0, 4096, false, 7)?;
    let _ = &state.pool;
    Ok(())
}

fn main() {}
