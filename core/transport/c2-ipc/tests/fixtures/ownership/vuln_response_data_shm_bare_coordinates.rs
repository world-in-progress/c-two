//! R02 baseline: ResponseData::Shm hand-built from bare coordinates is
//! accepted by the public ResponseLease. Compiled only, never run.
#![forbid(unsafe_code)]

use std::sync::Arc;

use c2_ipc::{ResponseData, ResponseLease, ServerPoolState, ServerShmResponse};
use parking_lot::Mutex;

fn main() {
    let server_pool: Arc<Mutex<Option<ServerPoolState>>> = Arc::new(Mutex::new(None));
    let mut lease = ResponseLease::new(ResponseData::Shm(ServerShmResponse::admit(
        server_pool,
        0,
        1,
        0,
        4096,
        false,
        7,
    )));
    let _ = lease.copy_bytes();
    let _ = lease.release();
}
