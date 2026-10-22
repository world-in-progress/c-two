//! A retained response must not follow an externally replaceable pool slot.
//! This baseline consumer only compiles; it never connects or swaps pools.
#![forbid(unsafe_code)]
use c2_ipc::{IpcClient, SyncClient};

fn swap_async_clients(left: &IpcClient, right: &IpcClient) {
    std::mem::swap(
        &mut *left.server_pool_arc().lock(),
        &mut *right.server_pool_arc().lock(),
    );
}

fn swap_sync_clients(left: &SyncClient, right: &SyncClient) {
    std::mem::swap(
        &mut *left.server_pool_arc().lock(),
        &mut *right.server_pool_arc().lock(),
    );
}

fn main() {}
