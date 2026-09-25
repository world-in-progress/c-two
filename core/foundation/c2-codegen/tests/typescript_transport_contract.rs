use c2_codegen::{
    ContractCodegenOptions, ContractCodegenTarget,
    compile_contract_artifacts as compile_admitted_contract_artifacts,
};
use c2_contract::ContractRelease;
use std::path::Path;
use std::process::Command;

const DESCRIPTOR: &str =
    include_str!("../../../../tests/fixtures/contracts/portable-release.contract.json");
const NO_PAYLOAD_DESCRIPTOR: &str =
    include_str!("../../../../tests/fixtures/contracts/portable-no-payload.contract.json");

fn generated_typescript() -> String {
    let release = ContractRelease::from_descriptor_json(DESCRIPTOR.as_bytes())
        .expect("portable release descriptor must remain admitted");
    let artifacts = compile_admitted_contract_artifacts(
        &release,
        ContractCodegenTarget::TypeScript,
        &ContractCodegenOptions::default(),
    )
    .expect("TypeScript codegen must succeed");
    String::from_utf8(
        artifacts
            .get("typescript/c_two_contract.ts")
            .expect("generated TypeScript contract module")
            .bytes()
            .to_vec(),
    )
    .expect("generated TypeScript must be UTF-8")
}

#[test]
fn generated_transport_declares_auditable_real_connection_modes() {
    let source = generated_typescript();

    for symbol in [
        "createIpcEncodedTransport",
        "createHttpRelayEncodedTransport",
        "createRelayAwareHttpEncodedTransport",
        "C2TransportObservation",
        "C2TransportObserver",
        "DirectIpc",
        "ExplicitRelay",
        "RelayAwareLocalIpc",
        "RelayAwareHttp",
    ] {
        assert!(
            source.contains(symbol),
            "generated transport is missing {symbol}"
        );
    }
}

#[test]
fn generated_http_calls_are_bound_to_resolved_route_tokens() {
    let source = generated_typescript();

    assert!(source.contains("routeUid"));
    assert!(source.contains("routeRevision"));
    assert!(source.contains("\"x-c2-route-uid\""));
    assert!(source.contains("\"x-c2-route-revision\""));
    assert!(source.contains("/_probe/"));
    assert!(
        source.contains("requestHeadersForResolvedRoute"),
        "HTTP data-plane calls must carry the exact route token returned by resolution"
    );
}

#[test]
fn relay_aware_local_ipc_requires_and_verifies_complete_route_facts() {
    let source = generated_typescript();

    for fact in [
        "ipcAddress",
        "serverId",
        "serverInstanceId",
        "expectedServerIdentity",
        "expectedRouteToken",
    ] {
        assert!(
            source.contains(fact),
            "relay-aware local IPC is missing verified fact {fact}"
        );
    }
    assert!(
        source.contains("same-path fallback denied"),
        "relay-aware local IPC failure must not fall back through the same route"
    );
}

#[test]
fn generated_fastdb_failures_preserve_the_frozen_outer_cause_fields() {
    let source = generated_typescript();

    for field in [
        "cause_owner",
        "fastdb_code",
        "fastdb_details_json",
        "fastdb_message",
        "fastdb_path",
        "fastdb_symbol",
    ] {
        assert!(
            source.contains(field),
            "generated FastDB adapter is missing outer cause field {field}"
        );
    }
    assert!(source.contains("C2PayloadAdapterError"));
    assert!(source.contains("error instanceof PayloadError"));
}

#[test]
fn generated_transports_bind_resolved_paths_and_prevent_unsafe_replay() {
    let repository = Path::new(env!("CARGO_MANIFEST_DIR"))
        .ancestors()
        .nth(3)
        .expect("C-Two repository root");
    let fastdb_typescript = repository
        .parent()
        .expect("WorldInProgress root")
        .join("fastdb/ts/fastdb4ts");
    let tsc = fastdb_typescript.join("node_modules/typescript/bin/tsc");
    assert!(
        tsc.is_file(),
        "Task 9 requires the audited TypeScript compiler at {}",
        tsc.display()
    );

    let tempdir = tempfile::tempdir().expect("temporary TypeScript project");
    let generated = tempdir.path().join("generated");
    let release = ContractRelease::from_descriptor_json(NO_PAYLOAD_DESCRIPTOR.as_bytes())
        .expect("no-payload descriptor must remain admitted");
    compile_admitted_contract_artifacts(
        &release,
        ContractCodegenTarget::TypeScript,
        &ContractCodegenOptions::default(),
    )
    .expect("TypeScript codegen")
    .publish_new_tree(&generated)
    .expect("publish generated TypeScript");

    let fastdb_package = tempdir.path().join("node_modules/fastdb4ts");
    std::fs::create_dir_all(&fastdb_package).expect("FastDB unit stub package");
    std::fs::write(
        fastdb_package.join("package.json"),
        r#"{"type":"module","exports":{"./payload":{"types":"./payload.d.ts","import":"./payload.js"}}}"#,
    )
    .expect("FastDB unit stub manifest");
    std::fs::write(
        fastdb_package.join("payload.d.ts"),
        r#"export class Payload {}
export class PayloadError extends Error {
  readonly code: number;
  readonly symbol: string;
  readonly path: string;
  readonly detailsJson: string;
}"#,
    )
    .expect("FastDB unit stub types");
    std::fs::write(
        fastdb_package.join("payload.js"),
        r#"export class Payload {}
export class PayloadError extends Error {
  constructor(code, symbol, path, message, detailsJson) {
    super(message);
    this.code = code;
    this.symbol = symbol;
    this.path = path;
    this.detailsJson = detailsJson;
  }
}"#,
    )
    .expect("FastDB unit stub runtime");
    std::fs::write(tempdir.path().join("package.json"), r#"{"type":"module"}"#)
        .expect("unit project manifest");
    std::fs::write(
        tempdir.path().join("tsconfig.json"),
        serde_json::to_vec_pretty(&serde_json::json!({
            "compilerOptions": {
                "target": "ES2022",
                "module": "NodeNext",
                "moduleResolution": "NodeNext",
                "lib": ["ES2022", "DOM"],
                "strict": true,
                "rootDir": generated,
                "outDir": tempdir.path().join("dist"),
            },
            "include": [generated.join("typescript/**/*.ts")],
        }))
        .expect("TypeScript config JSON"),
    )
    .expect("TypeScript config");

    let compile = Command::new("node")
        .args([
            tsc.to_str().expect("UTF-8 tsc path"),
            "--project",
            tempdir
                .path()
                .join("tsconfig.json")
                .to_str()
                .expect("UTF-8 config path"),
        ])
        .current_dir(tempdir.path())
        .output()
        .expect("run TypeScript compiler");
    assert!(
        compile.status.success(),
        "transport contract fixture failed to compile:\nstdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&compile.stdout),
        String::from_utf8_lossy(&compile.stderr),
    );

    std::fs::write(
        tempdir.path().join("transport_contract.mjs"),
        r#"import assert from "node:assert/strict";
import {
  CONTRACT,
  createHttpRelayEncodedTransport,
  createRelayAwareHttpEncodedTransport,
} from "./dist/typescript/c_two_contract.js";

const anchor = "http://127.0.0.1:7357";
const routeName = "same-path";
let dataPlanePosts = 0;
const route = {
  name: routeName,
  relay_url: anchor,
  route_uid: "same-path-route-0001",
  route_revision: 1,
  ipc_address: "ipc://same_path_unreachable",
  server_id: "same-path-server",
  server_instance_id: "same-path-instance",
  crm_ns: CONTRACT.namespace,
  crm_name: CONTRACT.name,
  crm_ver: CONTRACT.version,
  abi_hash: CONTRACT.abiHash,
  signature_hash: CONTRACT.signatureHash,
  max_payload_size: 1048576,
};
const response = (status, body) => ({
  status,
  headers: { get() { return null; } },
  body: null,
  async arrayBuffer() {
    const bytes = new TextEncoder().encode(body);
    return bytes.buffer.slice(bytes.byteOffset, bytes.byteOffset + bytes.byteLength);
  },
  async text() { return body; },
});
const fetch = async (input, init) => {
  if (input.includes("/_resolve/")) {
    return response(200, JSON.stringify([route]));
  }
  if (init.method === "POST") {
    dataPlanePosts += 1;
  }
  throw new Error(`unexpected HTTP request ${init.method} ${input}`);
};
const transport = createRelayAwareHttpEncodedTransport(anchor, {
  fetch,
  ipc: {
    async connect() {
      throw new Error("real IPC connect did not open");
    },
  },
});
await assert.rejects(
  () => transport.call(routeName, CONTRACT, "ping", new Uint8Array()),
  /same-path fallback denied/,
);
assert.equal(dataPlanePosts, 0);
await transport.close();

const explicitAnchor = "http://127.0.0.1:7358";
const explicitRelay = "http://127.0.0.1:8358";
const explicitRequests = [];
const explicitRoute = {
  ...route,
  relay_url: explicitRelay,
  route_uid: "explicit-route-0001",
};
const explicitFetch = async (input, init) => {
  explicitRequests.push([input, init.method]);
  if (input.includes("/_resolve/")) {
    assert.ok(input.startsWith(explicitAnchor));
    return response(200, JSON.stringify([explicitRoute]));
  }
  if (input.includes("/_probe/")) {
    assert.ok(input.startsWith(explicitRelay));
    return response(200, "");
  }
  assert.ok(input.startsWith(explicitRelay));
  assert.equal(init.method, "POST");
  return response(200, "");
};
await createHttpRelayEncodedTransport(explicitAnchor, {
  fetch: explicitFetch,
}).call(routeName, CONTRACT, "ping", new Uint8Array());
assert.deepEqual(explicitRequests.map(([url, method]) => [
  new URL(url).origin,
  method,
]), [
  [explicitAnchor, "GET"],
  [explicitRelay, "GET"],
  [explicitRelay, "POST"],
]);

const retryAnchor = "http://127.0.0.1:7359";
const retryRelays = [
  "http://127.0.0.1:8359",
  "http://127.0.0.1:8360",
];
let retryResolveCount = 0;
let retryPosts = 0;
const retryFetch = async (input, init) => {
  if (input.includes("/_resolve/")) {
    const relayUrl = retryRelays[Math.min(retryResolveCount, 1)];
    retryResolveCount += 1;
    return response(200, JSON.stringify([{
      ...route,
      relay_url: relayUrl,
      route_uid: `retry-route-000${retryResolveCount}`,
    }]));
  }
  if (input.includes("/_probe/")) {
    return response(200, "");
  }
  retryPosts += 1;
  if (retryPosts === 1) {
    return response(502, JSON.stringify({
      version: 1,
      code: 702,
      name: "ResourceUnavailable",
      message: "upstream unavailable before dispatch",
      details: {
        route: routeName,
        dispatch_phase: "pre_dispatch",
      },
    }));
  }
  return response(200, "");
};
const retryTransport = createRelayAwareHttpEncodedTransport(retryAnchor, {
  fetch: retryFetch,
  maxAttempts: 2,
  routeCacheTtlMs: 0,
});
await retryTransport.call(routeName, CONTRACT, "ping", new Uint8Array());
assert.equal(retryPosts, 2);
await retryTransport.close();

let uncertainPosts = 0;
const uncertainFetch = async (input, init) => {
  if (input.includes("/_resolve/")) {
    return response(200, JSON.stringify([{
      ...route,
      relay_url: retryRelays[0],
      route_uid: "uncertain-route-0001",
    }]));
  }
  if (input.includes("/_probe/")) {
    return response(200, "");
  }
  uncertainPosts += 1;
  return response(502, JSON.stringify({
    version: 1,
    code: 702,
    name: "ResourceUnavailable",
    message: "upstream may have dispatched",
    details: {
      route: routeName,
      dispatch_phase: "dispatch_uncertain",
    },
  }));
};
const uncertainTransport = createRelayAwareHttpEncodedTransport(retryAnchor, {
  fetch: uncertainFetch,
  maxAttempts: 3,
  routeCacheTtlMs: 0,
});
await assert.rejects(
  () => uncertainTransport.call(routeName, CONTRACT, "ping", new Uint8Array()),
);
assert.equal(uncertainPosts, 1);
await uncertainTransport.close();
"#,
    )
    .expect("transport contract runner");
    let run = Command::new("node")
        .arg(tempdir.path().join("transport_contract.mjs"))
        .current_dir(tempdir.path())
        .output()
        .expect("run same-path denial fixture");
    assert!(
        run.status.success(),
        "transport contract fixture failed:\nstdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&run.stdout),
        String::from_utf8_lossy(&run.stderr),
    );
}
