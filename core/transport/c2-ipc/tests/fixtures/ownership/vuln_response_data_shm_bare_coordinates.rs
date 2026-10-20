//! R02 baseline: ResponseData::Shm hand-built from bare coordinates is
//! accepted by the public ResponseLease. Compiled only, never run.
#![forbid(unsafe_code)]

use std::sync::Arc;

use c2_ipc::{ResponseData, ResponseLease, ServerPoolState};
use parking_lot::Mutex;

fn main() {
    let server_pool: Arc<Mutex<Option<ServerPoolState>>> = Arc::new(Mutex::new(None));
    let mut lease = ResponseLease::new(
        ResponseData::Shm {
            seg_idx: 0,
            generation: 1,
            offset: 0,
            data_size: 4096,
            is_dedicated: false,
            allocation_id: 7,
        },
        Arc::clone(&server_pool),
    );
    let _ = lease.copy_bytes();
    let _ = lease.release();
}
