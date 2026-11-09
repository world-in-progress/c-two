//! Test-only IPC attestation for compare-remove relay control cases.
use std::path::Path;

use c2_config::{ClientIpcConfig, LocalEndpointContext};
use c2_ipc::IpcClient;

#[tokio::main(flavor = "current_thread")]
async fn main() {
    let mut args = std::env::args().skip(1);
    let address = args.next().expect("IPC address");
    let route_name = args.next().expect("route name");
    let root = args.next().expect("endpoint root or -");
    let context = if root == "-" {
        LocalEndpointContext::default_for_platform().unwrap()
    } else {
        LocalEndpointContext::with_unix_root(Path::new(&root)).unwrap()
    };
    let mut client = IpcClient::with_endpoint(
        context.endpoint(&address).unwrap(),
        ClientIpcConfig::default(),
    )
    .unwrap();
    client.connect().await.unwrap();
    let expected = client
        .route_contract(&route_name)
        .expect("registered route contract");
    let record = client
        .attest_route_for_registration(&expected)
        .await
        .unwrap();
    println!("{}", record.route_uid);
    client.close().await;
}
