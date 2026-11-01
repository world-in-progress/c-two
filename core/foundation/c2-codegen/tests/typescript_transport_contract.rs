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

/// One compiled generated TypeScript transport, ready to run under Node.
struct CompiledTransportProject {
    directory: tempfile::TempDir,
}

impl CompiledTransportProject {
    fn compile() -> Self {
        let repository = Path::new(env!("CARGO_MANIFEST_DIR"))
            .ancestors()
            .nth(3)
            .expect("C-Two repository root");
        let tsc = repository
            .join("core/foundation/c2-mem-ffi/bindings/typescript/node_modules/typescript/bin/tsc");
        assert!(
            tsc.is_file(),
            "Task 9 requires the audited TypeScript compiler at {}",
            tsc.display()
        );

        let directory = tempfile::tempdir().expect("temporary TypeScript project");
        let tempdir = directory.path();
        let generated = tempdir.join("generated");
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

        std::fs::write(
            generated.join("typescript/injected_connection.ts"),
            r#"import { C2IpcTransportError, createIpcEncodedTransport, type C2IpcConnect } from "./c_two_contract.js";
const connect: C2IpcConnect = async () => ({
  write() {},
  readExactly(length) { return new Uint8Array(length); },
  close() {},
});
// @ts-expect-error A connection must be able to terminate pending reads/writes.
const missingClose: C2IpcConnect = () => ({ write() {}, readExactly(n: number) { return new Uint8Array(n); } });
createIpcEncodedTransport("ipc://typed-custom", { connect });
const mismatch = new C2IpcTransportError("version mismatch", { localProtocolVersion: 15, peerProtocolVersion: 16 });
const category: "protocol_version_mismatch" | null = mismatch.category;
const localVersion: number | null = mismatch.localProtocolVersion;
const peerVersion: number | null = mismatch.peerProtocolVersion;
// @ts-expect-error Version facts are supplied as one complete pair.
new C2IpcTransportError("incomplete version facts", { localProtocolVersion: 15 });
// @ts-expect-error Node factories belong to @c-two/c2-mem-ffi.
import { createNodeIpcConnect } from "./c_two_contract.js";
// @ts-expect-error Generated modules expose the generic connection contract.
import type { C2NodeIpcSocket } from "./c_two_contract.js";
// @ts-expect-error Generated modules expose the generic connection contract.
import type { C2NodeIpcNet } from "./c_two_contract.js";
// @ts-expect-error Generated modules expose the generic connection contract.
import type { C2NodeIpcConnectOptions } from "./c_two_contract.js";
"#,
        )
        .expect("typed custom connection consumer");

        let fastdb_package = tempdir.join("node_modules/fastdb4ts");
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
        std::fs::write(tempdir.join("package.json"), r#"{"type":"module"}"#)
            .expect("unit project manifest");
        std::fs::write(
            tempdir.join("tsconfig.json"),
            serde_json::to_vec_pretty(&serde_json::json!({
                "compilerOptions": {
                    "target": "ES2022",
                    "module": "NodeNext",
                    "moduleResolution": "NodeNext",
                    "lib": ["ES2022", "DOM"],
                    "strict": true,
                    "rootDir": generated,
                    "outDir": tempdir.join("dist"),
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
                    .join("tsconfig.json")
                    .to_str()
                    .expect("UTF-8 config path"),
            ])
            .current_dir(tempdir)
            .output()
            .expect("run TypeScript compiler");
        assert!(
            compile.status.success(),
            "transport contract fixture failed to compile:\nstdout:\n{}\nstderr:\n{}",
            String::from_utf8_lossy(&compile.stdout),
            String::from_utf8_lossy(&compile.stderr),
        );

        Self { directory }
    }

    fn root(&self) -> &Path {
        self.directory.path()
    }

    fn run_node_fixture(&self, name: &str, source: &str) {
        let path = self.root().join(name);
        std::fs::write(&path, source).expect("Node fixture source");
        let run = Command::new("node")
            .arg(&path)
            .current_dir(self.root())
            .output()
            .expect("run Node fixture");
        assert!(
            run.status.success(),
            "{name} failed:\nstdout:\n{}\nstderr:\n{}",
            String::from_utf8_lossy(&run.stdout),
            String::from_utf8_lossy(&run.stderr),
        );
    }
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
    assert!(source.contains("\"x-c2-route-uid\""));
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
    let project = CompiledTransportProject::compile();
    project.run_node_fixture(
        "transport_contract.mjs",
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
    );
}

#[test]
fn ipc_version_preamble_rejects_before_metadata_and_stays_terminal() {
    let project = CompiledTransportProject::compile();
    project.run_node_fixture(
        "ipc_version_preamble_contract.mjs",
        r#"import assert from 'node:assert/strict';
import test from 'node:test';
import {
  CONTRACT, C2IpcTransportError, createIpcEncodedTransport, createRelayAwareHttpEncodedTransport,
} from './dist/typescript/c_two_contract.js';

// Handwritten IPC15 records, independent of every production frame/handshake encoder.
// Each is LE bodylen=21, RID=0, exact direction flags, C2IP, kind, LE u32 version.
const HELLO15 = Buffer.from('1500000000000000000000000400000043324950010f000000', 'hex');
const ACCEPTED15 = Buffer.from('1500000000000000000000000600000043324950020f000000', 'hex');
const MISMATCH16 = Buffer.from('15000000000000000000000006000000433249500310000000', 'hex');
const CLIENT_METADATA = Buffer.from('0f000000000000000000000004000000000000', 'hex');
const ADDRESS = 'ipc://version_preamble';

// This peer only supplies handwritten bytes. It never closes the remote end to
// complete a rejection, and its close depends only on local cleanup.
function fixedPeer(reply, closeError) {
  let offset = 0;
  const peer = { writes: [], reads: [], connects: 0, closes: 0 };
  peer.connect = async () => {
    peer.connects += 1;
    return {
      async write(bytes) { peer.writes.push(Buffer.from(bytes)); },
      async readExactly(n) {
        peer.reads.push(n);
        const bytes = reply.subarray(offset, offset + n);
        offset += bytes.length;
        return bytes;
      },
      async close() {
        peer.closes += 1;
        if (closeError !== undefined) throw closeError;
      },
    };
  };
  return peer;
}
const prepare = (transport) => transport.prepare('manager', CONTRACT);
const captureError = (promise) => promise.then(() => undefined, (error) => error);
function assertMismatch(error, peerVersion) {
  assert.ok(error instanceof C2IpcTransportError, 'keep the existing IPC error class');
  assert.equal(error.category, 'protocol_version_mismatch');
  assert.equal(error.localProtocolVersion, 15);
  assert.equal(error.peerProtocolVersion, peerVersion);
}

test('ordinary IPC errors initialize null version facts', () => {
  const ordinary = new C2IpcTransportError('ordinary failure');
  assert.equal(ordinary.category, null);
  assert.equal(ordinary.localProtocolVersion, null);
  assert.equal(ordinary.peerProtocolVersion, null);
});

for (const [version, hex] of [
  [1, '15000000000000000000000006000000433249500301000000'],
  [13, '1500000000000000000000000600000043324950030d000000'],
  [16, '15000000000000000000000006000000433249500310000000'],
  [256, '15000000000000000000000006000000433249500300010000'],
  [4294967295, '150000000000000000000000060000004332495003ffffffff'],
]) {
  test(`fixed rejection reports local 15 and peer ${version} without remote EOF`, {timeout: 1000}, async () => {
    const peer = fixedPeer(Buffer.from(hex, 'hex'));
    const transport = createIpcEncodedTransport(ADDRESS, {connect: peer.connect});
    try {
      const error = await captureError(prepare(transport));
      assertMismatch(error, version);
      assert.deepEqual(peer.writes, [HELLO15]);
      assert.deepEqual(peer.reads, [16, 9], 'read the fixed envelope before the fixed body');
      assert.equal(peer.closes, 1, 'reject and close locally after the complete record');
      assert.equal(peer.connects, 1, 'a rejection must not reconnect');
    } finally { await transport.close(); }
  });
}

test('Hello is the only write while acceptance is pending, including with a private SHM pool', {timeout: 1000}, async () => {
  const peer = fixedPeer(MISMATCH16);
  const connect = peer.connect;
  let signalRead, releaseReply;
  const reading = new Promise((resolve) => { signalRead = resolve; });
  const replyReady = new Promise((resolve) => { releaseReply = resolve; });
  peer.connect = async () => {
    const connection = await connect();
    return {
      ...connection,
      memory: { prefix: '/private_before_acceptance' },
      async readExactly(n) {
        signalRead();
        await replyReady;
        return connection.readExactly(n);
      },
    };
  };
  const transport = createIpcEncodedTransport(ADDRESS, {
    connect: peer.connect,
  });
  const result = captureError(prepare(transport));
  try {
    await reading;
    assert.deepEqual(peer.writes, [HELLO15], 'SHM names and variable metadata wait for Accepted15');
  } finally {
    releaseReply();
    await result;
    await transport.close();
  }
});

test('local metadata encoding errors precede Hello and cannot be hidden by a peer rejection', async () => {
  const peer = fixedPeer(MISMATCH16);
  const connect = peer.connect;
  const transport = createIpcEncodedTransport(ADDRESS, {
    connect: async () => ({ ...await connect(), memory: { prefix: '/invalid\0prefix' } }),
  });
  try {
    const error = await captureError(prepare(transport));
    assert.ok(error instanceof C2IpcTransportError);
    assert.match(error.message, /client SHM prefix cannot contain control characters/);
    assert.equal(error.category, null);
    assert.equal(error.localProtocolVersion, null);
    assert.equal(error.peerProtocolVersion, null);
    assert.deepEqual(peer.writes, []);
    assert.deepEqual(peer.reads, []);
    assert.equal(peer.closes, 1);
  } finally { await transport.close(); }
});

for (const [label, hex] of [
  ['nonzero RID', '0c000000010000000000000006000000'],
  ['request flags', '0c000000000000000000000004000000'],
  ['missing handshake flag', '0c000000000000000000000002000000'],
  ['extra flags', '0c00000000000000000000000e000000'],
]) {
  test(`Accepted15 releases version-free metadata, then rejects metadata ACK ${label}`, async () => {
    const peer = fixedPeer(Buffer.concat([ACCEPTED15, Buffer.from(hex, 'hex')]));
    const transport = createIpcEncodedTransport(ADDRESS, {connect: peer.connect});
    try {
      const error = await captureError(prepare(transport));
      assert.ok(error instanceof C2IpcTransportError);
      assert.match(error.message, /metadata response has invalid request id or flags/);
      assert.equal(error.category, null);
      assert.equal(error.localProtocolVersion, null);
      assert.equal(error.peerProtocolVersion, null);
      assert.deepEqual(peer.writes, [HELLO15, CLIENT_METADATA]);
      assert.deepEqual(peer.reads, [16, 9, 16]);
      assert.equal(peer.closes, 1);
    } finally { await transport.close(); }
  });
}

for (const [label, hex, expectedReads] of [
  ['short envelope', '150000000000000000000000060000', [16]],
  ['wrong body length', '16000000000000000000000006000000433249500310000000', [16]],
  ['untrusted huge body length', 'ffffffff000000000000000006000000433249500310000000', [16]],
  ['nonzero RID', '15000000010000000000000006000000433249500310000000', [16]],
  ['request flags', '15000000000000000000000004000000433249500310000000', [16]],
  ['missing handshake flag', '15000000000000000000000002000000433249500310000000', [16]],
  ['extra flags', '1500000000000000000000000e000000433249500310000000', [16]],
  ['short body', '1500000000000000000000000600000043324950030f0000', [16, 9]],
  ['bad magic', '1500000000000000000000000600000043324951030f000000', [16, 9]],
  ['Hello response', '1500000000000000000000000600000043324950010f000000', [16, 9]],
  ['unknown kind', '1500000000000000000000000600000043324950040f000000', [16, 9]],
  ['zero version', '15000000000000000000000006000000433249500300000000', [16, 9]],
  ['Accepted with different version', '1500000000000000000000000600000043324950020e000000', [16, 9]],
  ['VersionMismatch with equal version', '1500000000000000000000000600000043324950030f000000', [16, 9]],
]) {
  test(`malformed preamble ${label} has no version facts`, async () => {
    const peer = fixedPeer(Buffer.from(hex, 'hex'));
    const transport = createIpcEncodedTransport(ADDRESS, {connect: peer.connect});
    try {
      const error = await captureError(prepare(transport));
      assert.ok(error instanceof C2IpcTransportError);
      assert.equal(error.category, null);
      assert.equal(error.localProtocolVersion, null);
      assert.equal(error.peerProtocolVersion, null);
      assert.deepEqual(peer.reads, expectedReads, 'invalid envelopes must not drive a body allocation/read');
      assert.deepEqual(peer.writes, [HELLO15], 'malformed responses cannot admit metadata');
      assert.equal(peer.closes, 1);
    } finally { await transport.close(); }
  });
}

test('fixed rejection survives a failed handshake close with the cleanup error as cause', async () => {
  const cleanup = new Error('local connection close failed');
  const peer = fixedPeer(MISMATCH16, cleanup);
  const transport = createIpcEncodedTransport(ADDRESS, {connect: peer.connect});
  try {
    const error = await captureError(prepare(transport));
    assertMismatch(error, 16);
    assert.equal(error.cause, cleanup);
    assert.equal(peer.closes, 1);
  } finally { await transport.close(); }
});

test('an existing handshake error keeps its identity when local close also fails', async () => {
  const primary = new C2IpcTransportError('read failed before a complete preamble');
  const cleanup = new Error('local read cleanup failed');
  let closes = 0;
  const transport = createIpcEncodedTransport(ADDRESS, {
    connect: async () => ({
      write() {},
      readExactly() { throw primary; },
      close() { closes += 1; throw cleanup; },
    }),
  });
  try {
    const error = await captureError(prepare(transport));
    assert.equal(error, primary);
    assert.equal(error.cause, cleanup);
    assert.equal(error.category, null);
    assert.equal(error.localProtocolVersion, null);
    assert.equal(error.peerProtocolVersion, null);
    assert.equal(closes, 1);
  } finally { await transport.close(); }
});

for (const withAlternative of [false, true]) {
  for (const failClose of [false, true]) {
    test(`relay local prepare keeps rejection terminal (alternative=${withAlternative}, close failure=${failClose})`, async () => {
      const cleanup = failClose ? new Error('relay local connection close failed') : undefined;
      const peer = fixedPeer(MISMATCH16, cleanup);
      const anchor = 'http://127.0.0.1:7357';
      const local = {
        name: 'manager', relay_url: anchor, ipc_address: ADDRESS,
        route_uid: 'local-route', server_id: 'server', server_instance_id: 'instance',
        crm_ns: CONTRACT.namespace, crm_name: CONTRACT.name, crm_ver: CONTRACT.version,
        abi_hash: CONTRACT.abiHash, signature_hash: CONTRACT.signatureHash, max_payload_size: 1048576,
      };
      const alternative = {...local, relay_url: 'http://127.0.0.1:8357', route_uid: 'other-route'};
      delete alternative.ipc_address;
      delete alternative.server_id;
      delete alternative.server_instance_id;
      let resolutions = 0;
      const http = [];
      const observations = [];
      const transport = createRelayAwareHttpEncodedTransport(anchor, {
        ipc: {connect: peer.connect}, maxAttempts: 3, routeCacheTtlMs: 0,
        observe: (value) => observations.push(value),
        fetch: async (url, init) => {
          const resolving = url.includes('/_resolve/');
          if (resolving) resolutions += 1;
          else http.push([url, init.method]);
          const body = resolving ? JSON.stringify(withAlternative ? [local, alternative] : [local]) : '';
          return {
            status: 200, headers: {get() { return null; }}, body: null,
            async text() { return body; },
            async arrayBuffer() { return new TextEncoder().encode(body).buffer; },
          };
        },
      });
      try {
        const error = await captureError(transport.call('manager', CONTRACT, 'ping', new Uint8Array()));
        assert.deepEqual(http, [], 'no HTTP probe or business fallback, even to a distinct route');
        assert.equal(resolutions, 1, 'do not exclude the local token and resolve/retry');
        assertMismatch(error, 16);
        assert.equal(error.cause, cleanup, 'relay prepare must preserve handshake cleanup cause');
        assert.equal(peer.connects, 1);
        assert.equal(peer.closes, 1);
        assert.deepEqual(peer.writes, [HELLO15]);
        assert.deepEqual(observations, [], 'no business call completed');
      } finally { await transport.close(); }
    });
  }
}
"#,
    );
}

#[test]
fn persistent_ipc_acquire_uses_authority_and_keeps_bound_identities() {
    let project = CompiledTransportProject::compile();
    project.run_node_fixture(
        "persistent_ipc_contract.mjs",
        r#"import assert from 'node:assert/strict';
import test from 'node:test';
import { C2IpcTransportError, createIpcEncodedTransport, createRelayAwareHttpEncodedTransport } from './dist/typescript/c_two_contract.js';

// Route catalog JSON/tag and FLAG_CTRL framing follow c2-wire's
// route_catalog_control.rs and c2-ipc client::send_control_unary_raw.
// Golden request hex was emitted by the Rust encode_route_*_request codecs.
const CONTRACT = {
  schema: 'c-two.contract.v2', namespace: 'test.persistent', name: 'Persistent',
  version: '0.1.0', descriptorSha256: 'd'.repeat(64),
  abiHash: 'a'.repeat(64), signatureHash: 'b'.repeat(64),
};
const wireContract = (name) => ({
  route_name: name, crm_ns: CONTRACT.namespace, crm_name: CONTRACT.name,
  crm_ver: CONTRACT.version, abi_hash: CONTRACT.abiHash, signature_hash: CONTRACT.signatureHash,
});
const record = (name, overrides = {}) => ({
  route_name: name, route_uid: `${name}-uid`,
  owner_server_id: 'server', owner_server_instance_id: 'instance',
  contract: wireContract(name), methods: [{name: 'ping', index: 0}],
  max_payload_size: 1048576, state: 'ready', state_reason: 'register_committed',
  ...overrides,
});
const u16 = (n) => { const b = Buffer.alloc(2); b.writeUInt16LE(n); return b; };
const u32 = (n) => { const b = Buffer.alloc(4); b.writeUInt32LE(n); return b; };
const u64 = (n) => { const b = Buffer.alloc(8); b.writeBigUInt64LE(BigInt(n)); return b; };
const text = (s) => Buffer.concat([Buffer.from([Buffer.byteLength(s)]), Buffer.from(s)]);
const frame = (id, flags, payload) => Buffer.concat([u32(12 + payload.length), u64(id), u32(flags), payload]);
const HELLO15 = Buffer.from('1500000000000000000000000400000043324950010f000000', 'hex');
const ACCEPTED15 = Buffer.from('1500000000000000000000000600000043324950020f000000', 'hex');
const handshake = (routes, instance, caps = 0) => Buffer.concat([
  text('/serverpool'), u16(caps), text('server'), text(instance), u16(routes.length),
  ...routes.map((r) => Buffer.concat([
    text(r.route_name), text(r.route_uid),
    text(r.contract.crm_ns), text(r.contract.crm_name), text(r.contract.crm_ver),
    text(r.contract.abi_hash), text(r.contract.signature_hash), u64(r.max_payload_size),
    u16(r.methods.length), ...r.methods.map((m) => Buffer.concat([text(m.name), u16(m.index)])),
  ])),
]);
function server() {
  const s = {
    routes: new Map([['manager', record('manager')]]), instance: 'instance',
    connections: [], lookups: [], lists: [], calls: [], callFlags: [], failCall: false,
    failRead: false, responseData: Buffer.alloc(0), chunkedReply: false, shmReply: false,
    ackShmRequest: false, ackShmResponse: false, clientHellos: [], clientHandshakes: [], clientCaps: 0,
    nextControl: undefined, corrupt: undefined, writesAfterClose: 0,
  };
  // The ACK models the native session intersection: a direction is
  // acknowledged only when this server enables it and the client offered it.
  s.serverCaps = () => 0
    | (s.ackShmRequest && (s.clientCaps & 8) !== 0 ? 8 : 0)
    | (s.ackShmResponse && (s.clientCaps & 16) !== 0 ? 16 : 0);
  s.connect = async () => {
    let buffered = Buffer.alloc(0);
    let handshakeStage = 0;
    let businessReply = false;
    let wakeRead, finishWrite;
    const feed = (bytes) => { buffered = Buffer.concat([buffered, bytes]); wakeRead?.(); };
    const c = {
      closed: 0,
      memory: s.memory,
      async write(data) {
        if (this.closed) { s.writesAfterClose += 1; throw new Error('write after close'); }
        const b = Buffer.from(data);
        assert.equal(b.readUInt32LE(0), b.length - 4);
        const id = b.readBigUInt64LE(4), flags = b.readUInt32LE(12);
        const payload = b.subarray(16);
        const reply = (tag, value) => {
          const bytes = Buffer.concat([Buffer.from([tag]), Buffer.from(JSON.stringify(value))]);
          feed(frame(id, 2 | 16, bytes));
        };
        if (flags === 4) {
          assert.equal(id, 0n);
          if (handshakeStage === 0) {
            assert.deepEqual(b, HELLO15);
            s.clientHellos.push(Buffer.from(b));
            handshakeStage = 1;
            feed(ACCEPTED15);
            return;
          }
          assert.equal(handshakeStage, 1, 'metadata follows exactly one Hello');
          assert.notDeepEqual(b, HELLO15, 'a repeated Hello is not metadata');
          s.clientHandshakes.push(Buffer.from(payload));
          s.clientCaps = payload.subarray(payload.length - 2).readUInt16LE(0);
          handshakeStage = 2;
          feed(frame(0n, 2 | 4, handshake([...s.routes.values()], s.instance, s.serverCaps())));
          return;
        }
        assert.equal(handshakeStage, 2, 'business and route control require both handshake stages');
        if (flags === 16 && payload[0] === 0x17) {
          if (s.failRelease) throw new Error('release write failed');
          s.responseReleases ??= [];
          for (let i = 0; i < payload[1]; i++) s.responseReleases.push(payload.readBigUInt64LE(2 + i * 8));
          return;
        }
        if (flags === 16) {
          const request = JSON.parse(payload.subarray(1).toString());
          if (payload[0] === 0x0e) {
            assert.deepEqual(request, { selector: {type: 'contract', expected: wireContract(request.selector.expected.route_name)} });
            s.lists.push(request);
            reply(0x0f, {routes: [...s.routes.values()].filter((r) => r.route_name === request.selector.expected.route_name)});
            return;
          }
          assert.equal(payload[0], 0x10);
          assert.deepEqual(request.expected, wireContract(request.expected.route_name));
          assert.deepEqual(Object.keys(request).sort(), ['expected', 'observed_route_uid']);
          s.lookups.push(request);
          if (s.corrupt) {
            feed(s.corrupt(id));
            s.corrupt = undefined;
            return;
          }
          if (s.nextControl) {
            const next = s.nextControl; s.nextControl = undefined;
            reply(next.tag, next.body); return;
          }
          const current = s.routes.get(request.expected.route_name);
          reply(0x11, current ? {
            status: request.observed_route_uid !== null && (request.observed_route_uid !== current.route_uid) ? 'stale' : 'ready', current,
          } : {status: 'not_found', route_name: request.expected.route_name});
        } else {
          assert.ok((flags & 128) !== 0);
          s.callFlags.push(flags);
          s.calls.push(payload);
          if ((flags & 64) !== 0) feed(frame(0n, 18, Buffer.concat([Buffer.from([0x17, 1]), u64(payload.readBigUInt64LE(15))])));
          if (s.failCall) { s.failCall = false; throw new Error('uncertain write'); }
          if (s.onCall) return await s.onCall({id, flags, feed, block: () => new Promise((resolve, reject) => { finishWrite = s.completeWriteOnClose ? resolve : () => reject(new Error('write interrupted by close')); })});
          if ((flags & 512) !== 0 && (flags & 1024) === 0) return;
          businessReply = true;
          const body = Buffer.concat([Buffer.from([0]), s.responseData]);
          if (s.chunkedReply) {
            for (let index = 0; index < 2; index += 1) {
              const data = s.responseData.subarray(index * 2, (index + 1) * 2);
              feed(frame(id, 2 | 256 | 512 | (index === 1 ? 1024 : 0), Buffer.concat([u64(s.responseData.length), u32(2), u32(index), data])));
            }
          } else if (s.shmReply) {
            const buddy = Buffer.concat([u16(0), u32(1), u32(0), u32(s.responseData.length), Buffer.from([0]), u64(s.responseAllocationId ?? 1n)]);
            feed(frame(id, 2 | 256 | 64, Buffer.concat([buddy, Buffer.from([0])])));
          } else {
            feed(frame(id, 2 | 256, body));
          }
        }
      },
      async readExactly(n) {
        while (buffered.length < n) {
          if (s.readFailure) { const error = s.readFailure; s.readFailure = undefined; throw error; }
          if (this.closed) throw new Error('connection closed');
          await new Promise((resolve) => { wakeRead = resolve; });
        }
        const b = buffered.subarray(0, n); buffered = buffered.subarray(n);
        if (businessReply) {
          businessReply = false;
          if (s.failRead) { s.failRead = false; throw new Error('uncertain read'); }
        }
        return b;
      },
      async close() {
        if (this.closed) return;
        this.closed = 1;
        finishWrite?.();
        wakeRead?.();
        if (s.closeFailure) throw s.closeFailure;
      },
    };
    s.connections.push(c);
    return c;
  };
  return s;
}
const transport = (s, extra = {}) => createIpcEncodedTransport('ipc://persistent', {connect: s.connect, ...extra});
const call = (t, name = 'manager') => t.call(name, CONTRACT, 'ping', new Uint8Array());

test('Node IPC connection deadline accepts only positive safe milliseconds', () => {
  const s = server();
  for (const value of [0, -1, 1.5, Number.NaN, Number.POSITIVE_INFINITY, Number.MAX_SAFE_INTEGER + 1]) {
    assert.throws(() => transport(s, {connectTimeoutMs: value}), /connectTimeoutMs must be a positive safe integer/);
  }
});

// This generated Node test also runs on Windows in the existing codegen CI gate.
test('Node IPC explicit connection budget can exceed the native five-second guard', {timeout: 8000}, async () => {
  const s = server();
  let passedBudget;
  const t = transport(s, {
    connectTimeoutMs: 6500,
    async connect(address, timeoutMs) {
      assert.equal(address, 'ipc://persistent');
      passedBudget = timeoutMs;
      await new Promise(resolve => setTimeout(resolve, 5100));
      return s.connect();
    },
  });
  try {
    await call(t);
    assert.ok(passedBudget > 5000 && passedBudget <= 6500);
    assert.equal(s.calls.length, 1);
  } finally { await t.close(); }
});

test('Node IPC connect expiry closes a late connection without business dispatch', {timeout: 1000}, async () => {
  const s = server();
  const t = transport(s, {
    connectTimeoutMs: 25,
    async connect() {
      await new Promise(resolve => setTimeout(resolve, 75));
      return s.connect();
    },
  });
  try {
    await assert.rejects(() => call(t), /connection deadline expired/);
    await new Promise(resolve => setTimeout(resolve, 100));
    assert.equal(s.connections.length, 1);
    assert.equal(s.connections[0].closed, 1);
    assert.equal(s.calls.length, 0);
  } finally { await t.close(); }
});

test('Node IPC metadata handshake shares the connection deadline', {timeout: 1000}, async () => {
  const s = server();
  const t = transport(s, {
    connectTimeoutMs: 25,
    async connect() {
      const connection = await s.connect();
      return {
        ...connection,
        async readExactly() { return await new Promise(() => {}); },
        close() { return connection.close(); },
      };
    },
  });
  try {
    await assert.rejects(() => call(t), /connection deadline expired/);
    assert.equal(s.connections.length, 1);
    assert.equal(s.connections[0].closed, 1);
    assert.equal(s.calls.length, 0);
  } finally { await t.close(); }
});

test('authoritative late route, shared serial prepare/calls, and independent tokens', async () => {
  const s = server(), t = transport(s);
  await call(t);
  s.routes.set('builder', record('builder'));
  await Promise.all([t.prepare('builder', CONTRACT), call(t, 'builder'), call(t)]);
  assert.equal(s.connections.length, 1);
  assert.equal(s.calls.length, 3);
  assert.deepEqual(s.lookups.map((r) => [r.expected.route_name, r.observed_route_uid]), [
    ['manager', null], ['builder', null], ['builder', 'builder-uid'], ['manager', 'manager-uid'],
  ]);
  await t.close(); await t.close();
  assert.equal(s.connections[0].closed, 1);
});

test('prepare and call reject replacements without poisoning another route', async () => {
  const s = server(), t = transport(s);
  s.routes.set('builder', record('builder'));
  await call(t, 'builder');
  s.routes.set('builder', record('builder', {route_uid: 'replacement'}));
  await assert.rejects(() => t.prepare('builder', CONTRACT), /route token mismatch/);
  await assert.rejects(() => call(t, 'builder'), /route token mismatch/);
  await call(t);
  assert.equal(s.calls.length, 2);
  assert.equal(s.connections.length, 1);
  assert.equal(s.connections[0].closed, 0);
  await t.close();
});

test('uncertain writes are never replayed; same identity reconnect retains the old token', async () => {
  const s = server(), t = transport(s);
  await call(t);
  s.failCall = true;
  await assert.rejects(() => call(t), /uncertain write/);
  assert.equal(s.calls.length, 2);
  await call(t);
  assert.equal(s.connections.length, 2);
  assert.equal(s.connections[0].closed, 1);
  s.failCall = true;
  await assert.rejects(() => call(t), /uncertain write/);
  s.routes.set('manager', record('manager', {route_uid: 'replacement'}));
  await assert.rejects(() => call(t), /route token mismatch/);
  assert.equal(s.calls.length, 4);
  await t.close();
});

test('server incarnation is shared and pinned even for a newly acquired route', async () => {
  const s = server(), t = transport(s);
  await call(t);
  s.failCall = true;
  await assert.rejects(() => call(t), /uncertain write/);
  s.instance = 'new-instance';
  s.routes.set('builder', record('builder', {owner_server_instance_id: s.instance}));
  await assert.rejects(() => t.prepare('builder', CONTRACT), /server identity mismatch/);
  await assert.rejects(() => call(t), /server identity mismatch/);
  assert.equal(s.calls.length, 2, 'the one failed write attempt is never replayed');
  await t.close();
});

test('closed/removed/not-found/contract mismatch do not dispatch or close a healthy stream', async () => {
  const s = server(), t = transport(s);
  await call(t);
  for (const body of [
    {status: 'closed', route_name: 'manager', route_uid: 'manager-uid', reason: 'shutdown'},
    {status: 'removed', route_name: 'manager', route_uid: 'manager-uid'},
    {status: 'not_found', route_name: 'manager'},
    {status: 'contract_mismatch', current: record('manager')},
  ]) {
    s.nextControl = {tag: 0x11, body};
    await assert.rejects(() => call(t));
    await call(t);
  }
  assert.equal(s.calls.length, 5);
  assert.equal(s.connections.length, 1);
  await t.close();
});

test('a future semantic NACK code and name remain a valid error envelope', async () => {
  const s = server(), t = transport(s);
  s.nextControl = {tag: 0x15, body: {error: {
    version: 1, code: 799, name: 'FutureRouteCondition', message: 'future condition', details: {},
  }}};
  try {
    await assert.rejects(() => call(t), (error) => {
      assert.ok(error instanceof C2IpcTransportError);
      assert.match(error.message, /NACK 799: future condition/);
      return true;
    });
    assert.equal(s.calls.length, 0);
  } finally { await t.close(); }
});

test('NACK decoding still rejects malformed error fields', async () => {
  for (const [error, expected] of [
    [{version: 1, code: 799, name: 42, message: 'bad name', details: {}}, /missing error name/],
    [{version: 1, code: 799, name: 'FutureRouteCondition', message: 'bad details', details: {stage: 42}}, /invalid NACK error details/],
  ]) {
    const s = server(), t = transport(s);
    s.nextControl = {tag: 0x15, body: {error}};
    try {
      await assert.rejects(() => call(t), expected);
      assert.equal(s.calls.length, 0);
    } finally { await t.close(); }
  }
});

test('invalid complete control replies fail lookup and preserve the stream', async () => {
  for (const corrupt of [
    (id) => frame(id + 1n, 18, Buffer.from([0x11, 123, 125])),
    (id) => frame(id, 2, Buffer.from([0x11, 123, 125])),
    (id) => frame(id, 18, Buffer.from([0x13, 123, 125])),
    (id) => frame(id, 18, Buffer.from([0x11, 123])),
    (id) => frame(id, 18, Buffer.from([0x11, ...Buffer.from(JSON.stringify({status: 'ready', current: record('manager', {route_revision: 9007199254740992})}))])),
  ]) {
    const s = server(), t = transport(s);
    s.corrupt = corrupt;
    await assert.rejects(() => call(t));
    assert.equal(s.calls.length, 0);
    assert.equal(s.connections[0].closed, 0);
    await call(t);
    assert.equal(s.connections.length, 1);
    await t.close();
  }
});

test('resolved relay identity and token remain mandatory in the local acquire path', async () => {
  const s = server();
  const t = transport(s, {expectedServerIdentity: {serverId: 'server', serverInstanceId: 'instance'}, expectedRouteToken: {routeUid: 'resolved-old', }});
  await assert.rejects(() => call(t), /route token mismatch/);
  assert.equal(s.calls.length, 0);
  await t.close();
  const local = createRelayAwareHttpEncodedTransport('http://127.0.0.1:7357', {
    ipc: {connect: s.connect},
    fetch: async (url) => {
      assert.ok(url.includes('/_resolve/'), 'no HTTP business fallback after local prepare');
      return {status: 200, async text() {return JSON.stringify([{
        name: 'manager', relay_url: 'http://127.0.0.1:7357', ipc_address: 'ipc://persistent',
        route_uid: 'resolved-old', server_id: 'server', server_instance_id: 'instance',
        crm_ns: CONTRACT.namespace, crm_name: CONTRACT.name, crm_ver: CONTRACT.version,
        abi_hash: CONTRACT.abiHash, signature_hash: CONTRACT.signatureHash, max_payload_size: 1048576,
      }]);}};
    },
  });
  await assert.rejects(() => call(local), /same-path fallback denied/);
  assert.equal(s.calls.length, 0);
  await local.close();

  const good = server(), observations = [];
  const matching = createRelayAwareHttpEncodedTransport('http://127.0.0.1:7357', {
    ipc: {connect: good.connect}, observe: (value) => observations.push(value),
    fetch: async (url) => {
      assert.ok(url.includes('/_resolve/'));
      return {status: 200, async text() {return JSON.stringify([{
        name: 'manager', relay_url: 'http://127.0.0.1:7357', ipc_address: 'ipc://persistent',
        route_uid: 'manager-uid', server_id: 'server', server_instance_id: 'instance',
        crm_ns: CONTRACT.namespace, crm_name: CONTRACT.name, crm_ver: CONTRACT.version,
        abi_hash: CONTRACT.abiHash, signature_hash: CONTRACT.signatureHash, max_payload_size: 1048576,
      }]);}};
    },
  });
  await call(matching);
  assert.equal(good.calls.length, 1);
  assert.equal(good.lookups.length, 2, 'prepare and call both query authority with the resolved token');
  assert.ok(good.lookups.every((request) => request.observed_route_uid === 'manager-uid'));
  assert.equal(good.connections[0].closed, 1, 'relay-aware temporary transport closes after its call');
  assert.equal(observations[0].path, 'RelayAwareLocalIpc');
  assert.equal(observations[0].requests, 1);
  await matching.close();
});

test('relay prepare cleanup failure keeps the identity error and its local close cause', async () => {
  const s = server();
  const cleanup = new Error('local close failed after identity rejection');
  s.closeFailure = cleanup;
  const t = createRelayAwareHttpEncodedTransport('http://127.0.0.1:7357', {
    ipc: {connect: s.connect},
    fetch: async (url) => {
      assert.ok(url.includes('/_resolve/'), 'no HTTP fallback on cleanup failure');
      return {status: 200, async text() {return JSON.stringify([{
        name: 'manager', relay_url: 'http://127.0.0.1:7357', ipc_address: 'ipc://persistent',
        route_uid: 'manager-uid', server_id: 'server', server_instance_id: 'different-instance',
        crm_ns: CONTRACT.namespace, crm_name: CONTRACT.name, crm_ver: CONTRACT.version,
        abi_hash: CONTRACT.abiHash, signature_hash: CONTRACT.signatureHash, max_payload_size: 1048576,
      }]);}};
    },
  });
  try {
    await assert.rejects(() => call(t), (error) => {
      assert.ok(error instanceof C2IpcTransportError);
      assert.match(error.message, /server identity mismatch/);
      assert.equal(error.category, null);
      assert.equal(error.localProtocolVersion, null);
      assert.equal(error.peerProtocolVersion, null);
      assert.ok(error.cause instanceof C2IpcTransportError);
      assert.match(error.cause.message, /IPC close failed/);
      assert.equal(error.cause.cause, cleanup);
      return true;
    });
    assert.equal(s.connections[0].closed, 1);
    assert.equal(s.lookups.length, 0);
    assert.equal(s.calls.length, 0);
  } finally { await t.close(); }
});

test('response read loss never replays a business call or changes the reconnect token', async () => {
  const s = server(), t = transport(s);
  await call(t);
  s.failRead = true;
  await assert.rejects(() => call(t), /uncertain read/);
  assert.equal(s.calls.length, 2);
  assert.equal(s.connections[0].closed, 1);
  s.routes.set('manager', record('manager', {route_uid: 'replacement'}));
  await assert.rejects(() => call(t), /route token mismatch/);
  assert.equal(s.calls.length, 2);
  await t.close();
});

test('contract, owner, and non-ready records cannot change a binding', async () => {
  const s = server(), t = transport(s);
  await call(t);
  const lookups = s.lookups.length;
  await assert.rejects(() => t.prepare('manager', {...CONTRACT, signatureHash: 'c'.repeat(64)}), /expected CRM contract/);
  assert.equal(s.lookups.length, lookups);
  for (const overrides of [
    {contract: {...wireContract('manager'), signature_hash: 'c'.repeat(64)}},
    {owner_server_instance_id: 'other-instance'},
    {state: 'closed'},
  ]) {
    s.routes.set('manager', record('manager', overrides));
    await assert.rejects(() => call(t));
    assert.equal(s.calls.length, 1);
  }
  s.routes.set('manager', record('manager'));
  await call(t);
  assert.equal(s.connections.length, 1);
  await t.close();
});

test('failed initial acquisition leaves server binding unset until successful acquire', async () => {
  const s = server(), t = transport(s);
  await assert.rejects(() => t.prepare('builder', CONTRACT));
  s.connections.at(-1).readExactly = async () => { throw new Error('uncertain read before any route binding'); };
  await assert.rejects(() => t.prepare('builder', CONTRACT), /uncertain read/);
  s.instance = 'new-instance';
  s.routes.set('builder', record('builder', {owner_server_instance_id: 'new-instance'}));
  await call(t, 'builder');
  assert.equal(s.calls.length, 1);
  await t.close();
});

const refusalPayload = Buffer.from('C2E1' + JSON.stringify({version: 1, code: 702, name: 'ResourceUnavailable', message: 'reassembly budget exhausted', details: {}}));
const refusal = (id) => frame(id, 2 | 256, Buffer.concat([Buffer.from([1]), u32(refusalPayload.length), refusalPayload]));
const nextTurn = () => new Promise((resolve) => setImmediate(resolve));

test('an early complete success ends chunk upload after a whole frame and keeps the stream', async () => {
  const s = server(), t = transport(s, {requestChunkSize: 2});
  let replied = false;
  s.onCall = ({id, feed}) => {
    if (!replied) {
      replied = true;
      feed(frame(id, 2 | 256, Buffer.from([0])));
    }
  };
  try {
    await t.call('manager', CONTRACT, 'ping', new Uint8Array(8));
    assert.ok(s.calls.length > 0 && s.calls.length < 4);
    assert.equal(s.connections[0].closed, 0);
    s.onCall = undefined;
    await call(t);
    assert.equal(s.connections.length, 1);
  } finally { await t.close(); }
});

for (const failure of ['refusal', 'refusal-with-successful-write', 'wrong-id', 'bad-flags', 'read-loss']) {
  test(`chunked ${failure} settles its write before the next call`, async () => {
    const s = server(), t = transport(s, {requestChunkSize: 2});
    let sentFailure = false;
    s.onCall = ({id, feed, block}) => {
      if (sentFailure) return;
      sentFailure = true;
      if (failure === 'read-loss') {
        s.readFailure = new Error('independent read failure');
        feed(Buffer.alloc(0));
        return block();
      } else {
        feed(failure.startsWith('refusal') ? refusal(id) : failure === 'wrong-id'
          ? refusal(id + 1n) : frame(id, 0, Buffer.from([0])));
      }
      return failure === 'refusal-with-successful-write' ? nextTurn() : undefined;
    };
    const result = t.call('manager', CONTRACT, 'ping', new Uint8Array(8)).catch((error) => error);
    try {
      const error = await result;
      assert.equal(s.connections[0].closed, failure === 'read-loss' ? 1 : 0);
      if (failure.startsWith('refusal')) {
        assert.equal(error.name, 'C2CrmMethodError');
        assert.deepEqual(Buffer.from(error.payload), refusalPayload);
      } else {
        assert.match(error.message, failure === 'wrong-id' ? /did not match/ : failure === 'bad-flags' ? /v2 reply/ : /independent read failure/);
      }
      if (failure.startsWith('refusal')) {
        assert.ok(s.calls.length > 0 && s.calls.length < 4, 'a complete service refusal stops later chunks');
      } else {
        assert.equal(s.calls.length, failure === 'read-loss' ? 1 : 4, 'completed frames are not replayed');
      }
      assert.equal(s.writesAfterClose, 0);
      s.onCall = undefined;
      await call(t); await call(t);
      assert.equal(s.connections.length, failure === 'read-loss' ? 2 : 1);
      assert.equal(s.lookups.at(-1).observed_route_uid, 'manager-uid');
    } finally {
      await t.close();
      await result;
    }
  });
}

test('a refusal after the last chunk skips duplicate older replies on the same stream', async () => {
  const s = server(), t = transport(s, {requestChunkSize: 2});
  s.onCall = ({id, flags, feed}) => {
    if (flags & 1024) feed(Buffer.concat([refusal(id), refusal(id)]));
  };
  try {
    await assert.rejects(t.call('manager', CONTRACT, 'ping', new Uint8Array(8)), (error) => {
      assert.equal(error.name, 'C2CrmMethodError');
      assert.deepEqual(Buffer.from(error.payload), refusalPayload);
      return true;
    });
    assert.equal(s.connections[0].closed, 0);
    assert.equal(s.calls.length, 4);
    s.onCall = undefined;
    await call(t); await call(t);
    assert.equal(s.connections.length, 1);
  } finally { await t.close(); }
});

function memory(s, events = []) {
  let releases = [];
  return {
    prefix: '/requestpool',
    finishHandshake(prefix) { events.push(['handshake', prefix]); },
    prepareRequest(payload) { events.push(['prepare', payload.byteLength]); return { block: {segmentIndex: 0, generation: 1, offset: 0, byteLength: payload.byteLength, loanId: 0xffffffffffffffffn, dedicated: false} }; },
    discardRequest() { events.push(['cancel']); },
    admitResponse(block) { events.push(['admit', block.loanId]); return block; },
    consumeResponse(block, dst) { events.push(['consume', block.loanId]); dst.set(s.responseData); releases.push(block.loanId); },
    discardResponse(block) { events.push(['discard', block.loanId]); releases.push(block.loanId); },
    beginWrite(kind) {
      const prefix = releases.length ? frame(0n, 16, Buffer.concat([Buffer.from([0x17, releases.length]), ...releases.map(u64)])) : Buffer.alloc(0);
      releases = [];
      return kind === 2 && prefix.length === 0 ? undefined : { prefix, start() {}, complete() {} };
    },
    receiveRelease(bytes) { events.push(['release', Buffer.from(bytes).readBigUInt64LE(18)]); },
    gc() { return 1000; },
    deactivate() {}, tryClose() { return true; },
  };
}

test('IPC15 metadata uses directional flags and no segment list', async () => {
  const s = server(), t = transport(s);
  await call(t);
  assert.equal(s.clientHandshakes[0].toString('hex'), '000000');
  await t.close();
  const n = server(); n.memory = memory(n);
  const native = transport(n);
  await call(native);
  assert.equal(n.clientHandshakes[0].subarray(-2).readUInt16LE(), 24);
  assert.equal(n.clientHandshakes[0].length, 1 + '/requestpool'.length + 2);
  await native.close();
});

test('request SHM requires the ACK and preserves a full u64 loan ID', async () => {
  const s = server(), events = []; s.memory = memory(s, events);
  const t = transport(s, {requestShmThreshold: 0});
  await t.call('manager', CONTRACT, 'ping', new Uint8Array(7));
  assert.equal(s.callFlags.at(-1) & 64, 0);
  assert.equal(events.some(([event]) => event === 'prepare'), false);
  s.ackShmRequest = true; await t.close();
  const acknowledged = transport(s, {requestShmThreshold: 0});
  await acknowledged.call('manager', CONTRACT, 'ping', new Uint8Array(7));
  assert.equal(s.callFlags.at(-1), 128 | 64);
  assert.equal(s.calls.at(-1).subarray(0, 23).toString('hex'), '000001000000000000000700000000ffffffffffffffff');
  assert.deepEqual(events.filter(([event]) => event === 'release'), [['release', 0xffffffffffffffffn]]);
  await acknowledged.close();
});

test('response copy sends release before the next call and keeps provider memory local', async () => {
  const s = server(), events = []; s.memory = memory(s, events);
  s.ackShmResponse = true; s.shmReply = true; s.responseAllocationId = 9007199254740993n;
  s.responseData = Buffer.from([42, 43, 44]);
  const t = transport(s, {responsePayloadAllocator(size) { const view = new Uint8Array(size); return {payload: {byteLength: size, view}, view}; }});
  const value = await call(t);
  assert.deepEqual(Array.from(value.view), [42, 43, 44]);
  assert.deepEqual(events.filter(([event]) => ['admit', 'consume'].includes(event)), [['admit', 9007199254740993n], ['consume', 9007199254740993n]]);
  assert.deepEqual(s.responseReleases, [9007199254740993n]);
  await t.close();
});

test('SHM replies without the response ACK settle the loan and keep the stream', async () => {
  const s = server(), events = []; s.memory = memory(s, events); s.shmReply = true; s.responseData = Buffer.from([42]);
  const t = transport(s, {responsePayloadAllocator(size) {
    if (s.shmReply) assert.fail('unacknowledged response allocated');
    const view = new Uint8Array(size); return {payload: view, view};
  }});
  await assert.rejects(() => call(t), /without the acknowledged SHM response capability/);
  assert.deepEqual(events.filter(([event]) => ['admit', 'discard'].includes(event)), [['admit', 1n], ['discard', 1n]]);
  assert.deepEqual(s.responseReleases, [1n]);
  assert.equal(s.connections[0].closed, 0);
  s.shmReply = false;
  assert.deepEqual(Array.from(await call(t)), [42]);
  assert.equal(s.connections.length, 1);
  await t.close();
});
test('provider allocator handles inline and chunked responses on the shared stream', async () => {
  const s = server(); s.responseData = Buffer.from([42, 43, 44]);
  const allocations = [];
  const t = transport(s, {requestChunkSize: 2, responsePayloadAllocator(size) {
    const view = new Uint8Array(size), payload = {byteLength: size, view};
    allocations.push(payload); return {payload, view};
  }});
  assert.deepEqual(Array.from((await call(t)).view), [42, 43, 44]);
  s.chunkedReply = true;
  assert.deepEqual(Array.from((await t.call('manager', CONTRACT, 'ping', new Uint8Array(7))).view), [42, 43, 44]);
  assert.equal(s.callFlags.filter(flags => flags & 512).length, 4);
  assert.deepEqual(allocations.map(p => p.byteLength), [3, 3]);
  assert.equal(s.connections.length, 1); await t.close();
});
for (const dedicated of [false, true]) {
  test(`failed chunked write discards an admitted response once (${dedicated})`, async () => {
    const s = server(), events = []; s.memory = memory(s, events); s.ackShmResponse = true;
    const t = transport(s, {requestChunkSize: 2, responsePayloadAllocator() { assert.fail('failed exchange allocated'); }});
    s.onCall = ({id, flags, feed}) => {
      if (!(flags & 1024)) return;
      const buddy = Buffer.concat([u16(0), u32(1), u32(0), u32(3), Buffer.from([Number(dedicated)]), u64(0xfffffffffffffff1n)]);
      feed(frame(id, 2 | 256 | 64, Buffer.concat([buddy, Buffer.from([0])])));
      return new Promise((_, reject) => setImmediate(() => reject(new Error('write acknowledgement lost'))));
    };
    await assert.rejects(() => t.call('manager', CONTRACT, 'ping', new Uint8Array(8)), /write acknowledgement lost/);
    assert.deepEqual(events.filter(([event]) => ['admit', 'consume', 'discard'].includes(event)), [['admit', 0xfffffffffffffff1n], ['discard', 0xfffffffffffffff1n]]);
    await t.close();
  });
}
test('failed response release write preserves the result and closes the original stream', async () => {
  const s = server(); s.memory = memory(s); s.ackShmResponse = true; s.shmReply = true;
  s.responseData = Buffer.from([42]); s.failRelease = true;
  const t = transport(s);
  assert.deepEqual(Array.from(await call(t)), [42]);
  assert.equal(s.connections[0].closed, 1);
  await t.close();
});
"#,
    );
}
