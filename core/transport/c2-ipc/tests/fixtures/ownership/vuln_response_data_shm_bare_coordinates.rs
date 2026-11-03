//! Only frame admission may mint a server response carrier.
#![forbid(unsafe_code)]
use c2_ipc::ServerShmResponse;
fn main() {
    let _ = ServerShmResponse::admit;
}
