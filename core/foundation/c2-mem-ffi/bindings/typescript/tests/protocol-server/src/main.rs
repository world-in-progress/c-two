//! Real c2-server peer for the Node IPC 15 / ABI 7 gate. No Python or FastDB.
use c2_config::{ConfigResolver, ConfigSources, LocalEndpointOptions};
use c2_server::{
    ConcurrencyMode, CrmCallback, CrmError, ResponseMeta, RouteBuildSpec, SchedulerLimits, Server,
    ServerIpcConfig,
};
use serde_json::json;
use std::{collections::HashMap, io::Write, sync::Arc, time::Duration};
use tokio::io::{AsyncBufReadExt, BufReader};

struct Echo;
impl CrmCallback for Echo {
    fn invoke(
        &self,
        _: &str,
        method: u16,
        request: Vec<u8>,
        _: Arc<c2_server::ResponsePool>,
    ) -> Result<ResponseMeta, CrmError> {
        let response = if method == 1 {
            let mut result = (request.len() as u32).to_le_bytes().to_vec();
            result.extend([
                request.first().copied().unwrap_or(0),
                request.last().copied().unwrap_or(0),
            ]);
            result
        } else {
            request
        };
        Ok(ResponseMeta::Inline(response))
    }
}
fn emit(value: serde_json::Value) {
    println!("{value}");
    std::io::stdout().flush().unwrap();
}
#[tokio::main(flavor = "current_thread")]
async fn main() {
    let args: Vec<_> = std::env::args().skip(1).collect();
    let context = ConfigResolver::resolve_local_endpoint(
        LocalEndpointOptions {
            unix_root: (args[0] != "-").then(|| args[0].clone().into()),
        },
        ConfigSources::empty(),
    )
    .unwrap();
    let endpoint = context.endpoint(&args[1]).unwrap();
    let server = Arc::new(Server::new_with_endpoint(endpoint, ServerIpcConfig::default()).unwrap());
    let route = server
        .build_route(
            RouteBuildSpec {
                name: "echo".into(),
                crm_ns: "test.p8a".into(),
                crm_name: "Echo".into(),
                crm_ver: "0.1.0".into(),
                abi_hash: "a".repeat(64),
                signature_hash: "b".repeat(64),
                method_names: vec!["echo".into(), "size".into()],
                access_map: HashMap::new(),
                concurrency_mode: ConcurrencyMode::Parallel,
                limits: SchedulerLimits::default(),
            },
            Arc::new(Echo),
        )
        .unwrap();
    server
        .commit_reserved_route(server.reserve_route(route).await.unwrap())
        .await
        .unwrap();
    let running = server.clone();
    let task = tokio::spawn(async move { running.run().await });
    server
        .wait_until_ready(Duration::from_secs(10))
        .await
        .unwrap();
    emit(json!({"kind": "ready"}));
    let mut lines = BufReader::new(tokio::io::stdin()).lines();
    while let Some(line) = lines.next_line().await.unwrap() {
        if line == "quit" {
            break;
        }
        if line == "stats" {
            emit(
                json!({"kind": "stats", "allocations": server.response_pool_arc().read().stats().alloc_count}),
            );
        }
    }
    server
        .shutdown_and_wait(Duration::from_secs(10))
        .await
        .unwrap();
    task.await.unwrap().unwrap();
    emit(json!({"kind": "stopped"}));
}
