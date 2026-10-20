//! R02 baseline: a safe CrmCallback publishes forged ResponseMeta::ShmAlloc
//! coordinates as a prepared response. Compiled only, never run.
#![forbid(unsafe_code)]

use std::sync::Arc;

use c2_mem::{MemPool, PoolConfig};
use c2_server::{CrmCallback, CrmError, PreparedShmResponse, RequestData, ResponseMeta};

struct ForgedPublisher;

impl CrmCallback for ForgedPublisher {
    fn invoke(
        &self,
        _route_name: &str,
        _method_idx: u16,
        _request: RequestData,
        _response_pool: Arc<c2_server::ResponsePool>,
    ) -> Result<ResponseMeta, CrmError> {
        let mut other_pool = MemPool::new(PoolConfig::default());
        let handle = other_pool.alloc_shm_handle(4096).unwrap();
        Ok(ResponseMeta::ShmAlloc(PreparedShmResponse::new(
            handle, 4096,
        )))
    }
}

fn main() {}
