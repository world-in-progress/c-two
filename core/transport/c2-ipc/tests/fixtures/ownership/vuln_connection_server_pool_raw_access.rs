//! A peer mapping cache must not expose raw coordinate access or its pool.
#![forbid(unsafe_code)]
use c2_ipc::ServerPoolState;
fn raw(state: &ServerPoolState) {
    let _ = ServerPoolState::copy_response;
    let _ = &state.pool;
}
fn main() {}
