//! R02 baseline: a safe CrmCallback publishes forged ResponseMeta::ShmAlloc
//! coordinates as a prepared response. Compiled only, never run.
#![forbid(unsafe_code)]

use std::sync::Arc;

use c2_mem::{MemPool, PoolConfig};
use c2_server::{CrmCallback, CrmError, RequestData, ResponseMeta};

struct ForgedPublisher;

impl CrmCallback for ForgedPublisher {
    fn invoke(
        &self,
        _route_name: &str,
        _method_idx: u16,
        _request: RequestData,
        _response_pool: Arc<parking_lot::RwLock<MemPool>>,
    ) -> Result<ResponseMeta, CrmError> {
        Ok(ResponseMeta::ShmAlloc {
            seg_idx: 0,
            generation: 1,
            offset: 0,
            data_size: 4096,
            is_dedicated: false,
            allocation_id: 11,
        })
    }
}

fn main() {
    let callback: Box<dyn CrmCallback> = Box::new(ForgedPublisher);
    let pool = Arc::new(parking_lot::RwLock::new(MemPool::new(PoolConfig::default())));
    let meta = callback
        .invoke("forged", 0, RequestData::Inline(Vec::new()), pool)
        .expect("safe publication of bare coordinates");
    let _ = matches!(meta, ResponseMeta::ShmAlloc { .. });
}
