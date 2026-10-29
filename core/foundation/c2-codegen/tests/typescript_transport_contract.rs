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
const mismatch = new C2IpcTransportError("version mismatch", { localProtocolVersion: 14, peerProtocolVersion: 15 });
const category: "protocol_version_mismatch" | null = mismatch.category;
const localVersion: number | null = mismatch.localProtocolVersion;
const peerVersion: number | null = mismatch.peerProtocolVersion;
// @ts-expect-error Version facts are supplied as one complete pair.
new C2IpcTransportError("incomplete version facts", { localProtocolVersion: 14 });
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
    );
}

#[test]
fn generated_c2_mem_ffi_reader_bootstraps_unadvertised_lazy_segments() {
    let project = CompiledTransportProject::compile();
    project.run_node_fixture(
        "lazy_reader.mjs",
        r#"import assert from "node:assert/strict";
import { createC2MemFfiNativeResponseShmReader } from "./dist/typescript/c_two_contract.js";

const bootstrapFloor = 2 * 4096;
const created = [];
const reads = [];
const releases = [];
const binding = {
  createResponsePool(options) {
    created.push(options);
    return {
      read(block, destination) {
        reads.push([options.prefix, block.segmentIndex, block.generation, block.offset, block.byteLength, destination.byteLength]);
        destination.fill(0x2a);
      },
      release(block) {
        releases.push([options.prefix, block.segmentIndex, block.generation]);
      },
    };
  },
};
const reader = createC2MemFfiNativeResponseShmReader({ binding });

// A lazy server pool advertises no segments at handshake; the reader must
// bootstrap a legal peer pool instead of demanding the server's real geometry.
const lazyBlock = {
  prefix: "/lazy_peer",
  segments: [],
  segmentIndex: 0,
  generation: 1,
  offset: 0,
  byteLength: 4,
  allocationId: 1n, dedicated: false,
};
const lazyBytes = await reader.read(lazyBlock);
assert.deepEqual(Array.from(lazyBytes), [0x2a, 0x2a, 0x2a, 0x2a]);
assert.equal(created.length, 1);
assert.deepEqual(created[0], {
  prefix: "/lazy_peer",
  segmentSize: bootstrapFloor,
  maxSegments: 16,
  minBlockSize: 4096,
});
assert.deepEqual(reads, [["/lazy_peer", 0, 1, 0, 4, 4]]);

await reader.release(lazyBlock);
assert.equal(created.length, 1, "one pool per owner prefix");
assert.deepEqual(releases, [["/lazy_peer", 0, 1]]);

const secondOwner = { ...lazyBlock, prefix: "/second_peer", generation: 3 };
await reader.read(secondOwner, new Uint8Array(4));
assert.equal(created.length, 2, "each owner prefix gets its own peer pool");
assert.equal(created[1].segmentSize, bootstrapFloor);

// The handshake snapshot is descriptive: when present it seeds the bootstrap
// capacity, and native still validates the real backing.
const advertisedBlock = {
  prefix: "/snapshot_peer",
  segments: [{ name: "/snapshot_segment", size: 1 << 20 }],
  segmentIndex: 0,
  generation: 2,
  offset: 8,
  byteLength: 4,
  allocationId: 1n, dedicated: false,
};
await reader.read(advertisedBlock, new Uint8Array(4));
assert.equal(created[2].segmentSize, 1 << 20);

// Dedicated responses have no buddy snapshot and only need a legal bootstrap.
const dedicatedBlock = {
  prefix: "/dedicated_peer",
  segments: [],
  segmentIndex: 7,
  generation: 0,
  offset: 0,
  byteLength: 4,
  allocationId: 0n, dedicated: true,
};
await reader.read(dedicatedBlock, new Uint8Array(4));
assert.equal(created[3].segmentSize, bootstrapFloor);

const configured = createC2MemFfiNativeResponseShmReader({
  binding,
  segmentSize: 65536,
  maxSegments: 2,
  minBlockSize: 4096,
});
await configured.read(advertisedBlock, new Uint8Array(4));
assert.equal(created[4].segmentSize, 65536, "explicit configuration wins");
assert.equal(created[4].maxSegments, 2);

await reader.close();
await configured.close();
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

// Handwritten IPC14 records, independent of every production frame/handshake encoder.
// Each is LE bodylen=21, RID=0, exact direction flags, C2IP, kind, LE u32 version.
const HELLO14 = Buffer.from('1500000000000000000000000400000043324950010e000000', 'hex');
const ACCEPTED14 = Buffer.from('1500000000000000000000000600000043324950020e000000', 'hex');
const MISMATCH15 = Buffer.from('1500000000000000000000000600000043324950030f000000', 'hex');
const CLIENT_METADATA = Buffer.from('110000000000000000000000040000000000000700', 'hex');
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
  assert.equal(error.localProtocolVersion, 14);
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
  [15, '1500000000000000000000000600000043324950030f000000'],
  [256, '15000000000000000000000006000000433249500300010000'],
  [4294967295, '150000000000000000000000060000004332495003ffffffff'],
]) {
  test(`fixed rejection reports local 14 and peer ${version} without remote EOF`, {timeout: 1000}, async () => {
    const peer = fixedPeer(Buffer.from(hex, 'hex'));
    const transport = createIpcEncodedTransport(ADDRESS, {connect: peer.connect});
    try {
      const error = await captureError(prepare(transport));
      assertMismatch(error, version);
      assert.deepEqual(peer.writes, [HELLO14]);
      assert.deepEqual(peer.reads, [16, 9], 'read the fixed envelope before the fixed body');
      assert.equal(peer.closes, 1, 'reject and close locally after the complete record');
      assert.equal(peer.connects, 1, 'a rejection must not reconnect');
    } finally { await transport.close(); }
  });
}

test('Hello is the only write while acceptance is pending, including with a private SHM pool', {timeout: 1000}, async () => {
  const peer = fixedPeer(MISMATCH15);
  const connect = peer.connect;
  let signalRead, releaseReply;
  const reading = new Promise((resolve) => { signalRead = resolve; });
  const replyReady = new Promise((resolve) => { releaseReply = resolve; });
  peer.connect = async () => {
    const connection = await connect();
    return {
      ...connection,
      async readExactly(n) {
        signalRead();
        await replyReady;
        return connection.readExactly(n);
      },
    };
  };
  const transport = createIpcEncodedTransport(ADDRESS, {
    connect: peer.connect,
    requestShmWriter: {
      prefix: '/private_before_acceptance',
      segments: [{name: '/private_before_acceptance_b0000', size: 4096}],
      async write() { assert.fail('no SHM payload before acceptance'); },
      async release() { assert.fail('no SHM allocation before acceptance'); },
    },
  });
  const result = captureError(prepare(transport));
  try {
    await reading;
    assert.deepEqual(peer.writes, [HELLO14], 'SHM names and variable metadata wait for Accepted14');
  } finally {
    releaseReply();
    await result;
    await transport.close();
  }
});

test('local metadata encoding errors precede Hello and cannot be hidden by a peer rejection', async () => {
  const peer = fixedPeer(MISMATCH15);
  let nameReads = 0;
  const transport = createIpcEncodedTransport(ADDRESS, {
    connect: peer.connect,
    requestShmWriter: {
      prefix: '/localpool',
      segments: [{
        // Normalization validates the initial value, then snapshots it. The
        // encoder must still reject an invalid snapshot before sending Hello.
        get name() { return nameReads++ === 0 ? '/valid_segment' : '/invalid\0segment'; },
        size: 4096,
      }],
      async write() { assert.fail('must not allocate'); },
      async release() { assert.fail('must not release an unallocated block'); },
    },
  });
  try {
    const error = await captureError(prepare(transport));
    assert.ok(error instanceof C2IpcTransportError);
    assert.match(error.message, /client SHM segment 0 name cannot contain control characters/);
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
  test(`Accepted14 releases version-free metadata, then rejects metadata ACK ${label}`, async () => {
    const peer = fixedPeer(Buffer.concat([ACCEPTED14, Buffer.from(hex, 'hex')]));
    const transport = createIpcEncodedTransport(ADDRESS, {connect: peer.connect});
    try {
      const error = await captureError(prepare(transport));
      assert.ok(error instanceof C2IpcTransportError);
      assert.match(error.message, /metadata response has invalid request id or flags/);
      assert.equal(error.category, null);
      assert.equal(error.localProtocolVersion, null);
      assert.equal(error.peerProtocolVersion, null);
      assert.deepEqual(peer.writes, [HELLO14, CLIENT_METADATA]);
      assert.deepEqual(peer.reads, [16, 9, 16]);
      assert.equal(peer.closes, 1);
    } finally { await transport.close(); }
  });
}

for (const [label, hex, expectedReads] of [
  ['short envelope', '150000000000000000000000060000', [16]],
  ['wrong body length', '1600000000000000000000000600000043324950030f000000', [16]],
  ['untrusted huge body length', 'ffffffff00000000000000000600000043324950030f000000', [16]],
  ['nonzero RID', '1500000001000000000000000600000043324950030f000000', [16]],
  ['request flags', '1500000000000000000000000400000043324950030f000000', [16]],
  ['missing handshake flag', '1500000000000000000000000200000043324950030f000000', [16]],
  ['extra flags', '1500000000000000000000000e00000043324950030f000000', [16]],
  ['short body', '1500000000000000000000000600000043324950030f0000', [16, 9]],
  ['bad magic', '1500000000000000000000000600000043324951030f000000', [16, 9]],
  ['Hello response', '1500000000000000000000000600000043324950010f000000', [16, 9]],
  ['unknown kind', '1500000000000000000000000600000043324950040f000000', [16, 9]],
  ['zero version', '15000000000000000000000006000000433249500300000000', [16, 9]],
  ['Accepted with different version', '1500000000000000000000000600000043324950020f000000', [16, 9]],
  ['VersionMismatch with equal version', '1500000000000000000000000600000043324950030e000000', [16, 9]],
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
      assert.deepEqual(peer.writes, [HELLO14], 'malformed responses cannot admit metadata');
      assert.equal(peer.closes, 1);
    } finally { await transport.close(); }
  });
}

test('fixed rejection survives a failed handshake close with the cleanup error as cause', async () => {
  const cleanup = new Error('local connection close failed');
  const peer = fixedPeer(MISMATCH15, cleanup);
  const transport = createIpcEncodedTransport(ADDRESS, {connect: peer.connect});
  try {
    const error = await captureError(prepare(transport));
    assertMismatch(error, 15);
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
      const peer = fixedPeer(MISMATCH15, cleanup);
      const anchor = 'http://127.0.0.1:7357';
      const local = {
        name: 'manager', relay_url: anchor, ipc_address: ADDRESS,
        route_uid: 'local-route', route_revision: 1, server_id: 'server', server_instance_id: 'instance',
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
        assertMismatch(error, 15);
        assert.equal(error.cause, cleanup, 'relay prepare must preserve handshake cleanup cause');
        assert.equal(peer.connects, 1);
        assert.equal(peer.closes, 1);
        assert.deepEqual(peer.writes, [HELLO14]);
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
  route_name: name, route_uid: `${name}-uid`, route_revision: 1, catalog_revision: 1,
  owner_server_id: 'server', owner_server_instance_id: 'instance', owner_epoch: 1,
  contract: wireContract(name), methods: [{name: 'ping', index: 0}],
  max_payload_size: 1048576, state: 'ready', state_reason: 'register_committed',
  lease_deadline_ms: null, ...overrides,
});
const u16 = (n) => { const b = Buffer.alloc(2); b.writeUInt16LE(n); return b; };
const u32 = (n) => { const b = Buffer.alloc(4); b.writeUInt32LE(n); return b; };
const u64 = (n) => { const b = Buffer.alloc(8); b.writeBigUInt64LE(BigInt(n)); return b; };
const text = (s) => Buffer.concat([Buffer.from([Buffer.byteLength(s)]), Buffer.from(s)]);
const frame = (id, flags, payload) => Buffer.concat([u32(12 + payload.length), u64(id), u32(flags), payload]);
const HELLO14 = Buffer.from('1500000000000000000000000400000043324950010e000000', 'hex');
const ACCEPTED14 = Buffer.from('1500000000000000000000000600000043324950020e000000', 'hex');
const handshake = (routes, instance, caps = 7) => Buffer.concat([
  text('/serverpool'), u16(0), u16(caps), text('server'), text(instance), u16(routes.length),
  ...routes.map((r) => Buffer.concat([
    text(r.route_name), text(r.route_uid), u64(r.route_revision),
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
  s.serverCaps = () => 7
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
            assert.deepEqual(b, HELLO14);
            s.clientHellos.push(Buffer.from(b));
            handshakeStage = 1;
            feed(ACCEPTED14);
            return;
          }
          assert.equal(handshakeStage, 1, 'metadata follows exactly one Hello');
          assert.notDeepEqual(b, HELLO14, 'a repeated Hello is not metadata');
          s.clientHandshakes.push(Buffer.from(payload));
          s.clientCaps = payload.subarray(payload.length - 2).readUInt16LE(0);
          handshakeStage = 2;
          feed(frame(0n, 2 | 4, handshake([...s.routes.values()], s.instance, s.serverCaps())));
          return;
        }
        assert.equal(handshakeStage, 2, 'business and route control require both handshake stages');
        if (flags === 16) {
          const request = JSON.parse(payload.subarray(1).toString());
          if (payload[0] === 0x0e) {
            if (request.selector.expected.route_name === 'manager') assert.equal(payload.toString('hex'), '0e7b2273656c6563746f72223a7b2274797065223a22636f6e7472616374222c226578706563746564223a7b22726f7574655f6e616d65223a226d616e61676572222c2263726d5f6e73223a22746573742e70657273697374656e74222c2263726d5f6e616d65223a2250657273697374656e74222c2263726d5f766572223a22302e312e30222c226162695f68617368223a2261616161616161616161616161616161616161616161616161616161616161616161616161616161616161616161616161616161616161616161616161616161222c227369676e61747572655f68617368223a2262626262626262626262626262626262626262626262626262626262626262626262626262626262626262626262626262626262626262626262626262626262227d7d2c226d696e5f7265766973696f6e223a6e756c6c7d');
            assert.deepEqual(request, { selector: {type: 'contract', expected: wireContract(request.selector.expected.route_name)}, min_revision: null });
            s.lists.push(request);
            reply(0x0f, {catalog_revision: 1, min_watch_revision: 1, routes: [...s.routes.values()].filter((r) => r.route_name === request.selector.expected.route_name)});
            return;
          }
          assert.equal(payload[0], 0x10);
          if (request.expected.route_name === 'manager' && request.observed_route_uid === null) assert.equal(payload.toString('hex'), '107b226578706563746564223a7b22726f7574655f6e616d65223a226d616e61676572222c2263726d5f6e73223a22746573742e70657273697374656e74222c2263726d5f6e616d65223a2250657273697374656e74222c2263726d5f766572223a22302e312e30222c226162695f68617368223a2261616161616161616161616161616161616161616161616161616161616161616161616161616161616161616161616161616161616161616161616161616161222c227369676e61747572655f68617368223a2262626262626262626262626262626262626262626262626262626262626262626262626262626262626262626262626262626262626262626262626262626262227d2c226f627365727665645f726f7574655f756964223a6e756c6c2c226f627365727665645f726f7574655f7265766973696f6e223a6e756c6c7d');
          assert.deepEqual(request.expected, wireContract(request.expected.route_name));
          assert.deepEqual(Object.keys(request).sort(), ['expected', 'observed_route_revision', 'observed_route_uid']);
          assert.equal(request.observed_route_uid === null, request.observed_route_revision === null);
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
            status: request.observed_route_uid !== null && (request.observed_route_uid !== current.route_uid || request.observed_route_revision !== current.route_revision) ? 'stale' : 'ready', current,
          } : {status: 'not_found', route_name: request.expected.route_name});
        } else {
          assert.ok((flags & 128) !== 0);
          s.callFlags.push(flags);
          s.calls.push(payload);
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
  s.routes.set('builder', record('builder', {route_uid: 'replacement', route_revision: 2}));
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
  s.routes.set('manager', record('manager', {route_uid: 'replacement', route_revision: 2}));
  await assert.rejects(() => call(t), /route token mismatch/);
  assert.equal(s.calls.length, 4);
  await t.close();
});

test('server incarnation is shared and pinned even for a newly acquired route', async () => {
  const s = server(), t = transport(s);
  await call(t);
  await t.close();
  s.instance = 'new-instance';
  s.routes.set('builder', record('builder', {owner_server_instance_id: s.instance}));
  await assert.rejects(() => t.prepare('builder', CONTRACT), /server identity mismatch/);
  await assert.rejects(() => call(t), /server identity mismatch/);
  assert.equal(s.calls.length, 1);
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

test('compacted catalog uses contract-scoped list then lookup with the pinned token', async () => {
  const s = server(), t = transport(s);
  await call(t);
  s.nextControl = {tag: 0x15, body: {nonce: 0, rejected_revision: 1, error: {
    version: 1, code: 711, name: 'RouteCatalogCompacted', message: 'compacted', details: {},
  }}};
  await call(t);
  assert.equal(s.lists.length, 1);
  assert.equal(s.lookups.length, 3);
  assert.equal(s.lookups[2].observed_route_uid, 'manager-uid');
  await t.close();
});

test('corrupt control replies discard the stream without business dispatch', async () => {
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
    assert.equal(s.connections[0].closed, 1);
    await call(t);
    assert.equal(s.connections.length, 2);
    await t.close();
  }
});

test('resolved relay identity and token remain mandatory in the local acquire path', async () => {
  const s = server();
  const t = transport(s, {expectedServerIdentity: {serverId: 'server', serverInstanceId: 'instance'}, expectedRouteToken: {routeUid: 'resolved-old', routeRevision: 1}});
  await assert.rejects(() => call(t), /route token mismatch/);
  assert.equal(s.calls.length, 0);
  await t.close();
  const local = createRelayAwareHttpEncodedTransport('http://127.0.0.1:7357', {
    ipc: {connect: s.connect},
    fetch: async (url) => {
      assert.ok(url.includes('/_resolve/'), 'no HTTP business fallback after local prepare');
      return {status: 200, async text() {return JSON.stringify([{
        name: 'manager', relay_url: 'http://127.0.0.1:7357', ipc_address: 'ipc://persistent',
        route_uid: 'resolved-old', route_revision: 1, server_id: 'server', server_instance_id: 'instance',
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
        route_uid: 'manager-uid', route_revision: 1, server_id: 'server', server_instance_id: 'instance',
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
        route_uid: 'manager-uid', route_revision: 1, server_id: 'server', server_instance_id: 'different-instance',
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
  s.routes.set('manager', record('manager', {route_revision: 2}));
  await assert.rejects(() => call(t), /route token mismatch/);
  assert.equal(s.calls.length, 2);
  assert.equal(s.lookups.at(-1).observed_route_revision, 1);
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
    ...['pending', 'draining', 'closed', 'removed'].map((state) => ({state})),
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
  await t.close();
  s.instance = 'new-instance';
  s.routes.set('builder', record('builder', {owner_server_instance_id: 'new-instance'}));
  await call(t, 'builder');
  assert.equal(s.calls.length, 1);
  await t.close();
});

const refusalPayload = Buffer.from('C2E1' + JSON.stringify({version: 1, code: 702, name: 'ResourceUnavailable', message: 'reassembly budget exhausted', details: {}}));
const refusal = (id) => frame(id, 2 | 256, Buffer.concat([Buffer.from([1]), u32(refusalPayload.length), refusalPayload]));
const nextTurn = () => new Promise((resolve) => setImmediate(resolve));

for (const failure of ['refusal', 'refusal-with-successful-write', 'wrong-id', 'bad-flags', 'read-loss']) {
  test(`chunked ${failure} ends the currently blocked write and joins before the next call`, async () => {
    const s = server(), t = transport(s, {requestChunkSize: 2});
    s.completeWriteOnClose = failure === 'refusal-with-successful-write';
    s.onCall = ({id, feed, block}) => {
      const pending = block();
      if (failure === 'read-loss') {
        s.readFailure = new Error('independent read failure');
        feed(Buffer.alloc(0));
      } else {
        feed(failure.startsWith('refusal') ? refusal(id) : failure === 'wrong-id'
          ? refusal(id + 1n) : frame(id, 0, Buffer.from([0])));
      }
      return pending;
    };
    const result = t.call('manager', CONTRACT, 'ping', new Uint8Array(8)).catch((error) => error);
    try {
      await nextTurn();
      assert.equal(s.connections[0].closed, 1, 'terminal receive must interrupt this write');
      const error = await result;
      if (failure.startsWith('refusal')) {
        assert.equal(error.name, 'C2CrmMethodError');
        assert.deepEqual(Buffer.from(error.payload), refusalPayload);
      } else {
        assert.match(error.message, failure === 'wrong-id' ? /did not match/ : failure === 'bad-flags' ? /v2 reply/ : /independent read failure/);
      }
      assert.equal(s.calls.length, 1, 'unsent chunks must not be sent or replayed');
      assert.equal(s.writesAfterClose, 0, 'a successful write callback must not resume a closed exchange');
      s.onCall = undefined;
      await call(t); await call(t);
      assert.equal(s.connections.length, 2, 'only the failed connection is replaced');
      assert.equal(s.lookups.at(-1).observed_route_uid, 'manager-uid');
      assert.equal(s.lookups.at(-1).observed_route_revision, 1);
    } finally {
      await t.close();
      await result;
    }
  });
}

test('a refusal after the last chunk discards duplicate replies before the next call', async () => {
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
    assert.equal(s.connections[0].closed, 1);
    assert.equal(s.calls.length, 4);
    s.onCall = undefined;
    await call(t); await call(t);
    assert.equal(s.connections.length, 2);
  } finally { await t.close(); }
});

for (const [generation, allocationId, dedicated] of [[1, 7n, false], [0, 0n, true], [1, 0n, false]]) {
  test(`a received SHM reply is released exactly when authorized after a write failure (${generation}/${allocationId})`, async () => {
    const s = server(), releases = [];
    s.ackShmResponse = true;
    let allocated = 0;
    const t = transport(s, {
      requestChunkSize: 2,
      responsePayloadAllocator() { allocated += 1; throw new Error('must not allocate after failed send'); },
      responseShmReader: {
        read() { throw new Error('must not copy an undelivered reply'); },
        release(block) { releases.push(block); },
      },
    });
    s.onCall = ({id, flags, feed}) => {
      if (!(flags & 1024)) return;
      const buddy = Buffer.concat([u16(0), u32(generation), u32(0), u32(3), Buffer.from([Number(dedicated)]), u64(allocationId)]);
      feed(frame(id, 2 | 256 | 64, Buffer.concat([buddy, Buffer.from([0])])));
      return new Promise((_, reject) => setImmediate(() => reject(new Error('write acknowledgement lost'))));
    };
    try {
      await assert.rejects(t.call('manager', CONTRACT, 'ping', new Uint8Array(8)),
        !dedicated && allocationId === 0n ? /allocationId/ : /write acknowledgement lost/);
      assert.equal(allocated, 0);
      assert.deepEqual(releases.map(({prefix, generation, allocationId, dedicated}) => ({prefix, generation, allocationId, dedicated})),
        !dedicated && allocationId === 0n ? [] : [{prefix: '/serverpool', generation, allocationId, dedicated}]);
      assert.equal(s.connections[0].closed, 1);
    } finally { await t.close(); }
  });
}

test('IPC14 client metadata offers each shared-memory direction separately after Hello', async () => {
  const writerShape = {
    prefix: '/requestpool', segments: [{name: '/requestpool_b0000', size: 1048576}],
    async write() { throw new Error('must not write without the request ACK'); },
    async release() {},
  };
  const readerShape = { read() {}, release() {} };

  const s = server(), t = transport(s);
  await call(t);
  assert.equal(s.clientHandshakes.length, 1);
  assert.deepEqual(s.clientHellos, [HELLO14]);
  assert.equal(s.clientHandshakes[0].subarray(-2).readUInt16LE(0), 7);
  assert.equal(s.clientHandshakes[0][0], 0, 'no pool advertisement without a writer');
  await t.close();

  const readerOnly = server();
  const rt = transport(readerOnly, { responseShmReader: readerShape });
  await call(rt);
  assert.deepEqual(readerOnly.clientHellos, [HELLO14]);
  assert.equal(readerOnly.clientHandshakes[0].subarray(-2).readUInt16LE(0), 7 | 16);
  assert.equal(readerOnly.clientHandshakes[0][0], 0);
  await rt.close();

  const writerOnly = server();
  const wt = transport(writerOnly, { requestShmThreshold: 0, requestShmWriter: writerShape });
  await call(wt);
  const offer = writerOnly.clientHandshakes[0];
  assert.deepEqual(writerOnly.clientHellos, [HELLO14]);
  assert.equal(offer.subarray(-2).readUInt16LE(0), 7 | 8);
  assert.equal(offer.subarray(1, 1 + '/requestpool'.length).toString(), '/requestpool');
  await wt.close();

  const both = server();
  const bt = transport(both, { requestShmThreshold: 0, requestShmWriter: writerShape, responseShmReader: readerShape });
  await call(bt);
  assert.deepEqual(both.clientHellos, [HELLO14]);
  assert.equal(both.clientHandshakes[0].subarray(-2).readUInt16LE(0), 7 | 8 | 16);
  await bt.close();
});

test('unacknowledged request shared memory stays on the owned inline path', async () => {
  const s = server();
  const events = [];
  const writer = {
    prefix: '/requestpool', segments: [{name: '/requestpool_b0000', size: 1048576}],
    async write(payload) {
      events.push(['write', payload.byteLength]);
      return {segmentIndex: 0, generation: 1, offset: 0, byteLength: payload.byteLength, allocationId: 1n, dedicated: false};
    },
    async markConsumed() { events.push(['consumed']); },
    async release() { events.push(['request-release']); },
  };
  const refused = transport(s, { requestShmThreshold: 0, requestShmWriter: writer });
  await refused.call('manager', CONTRACT, 'ping', new Uint8Array(5));
  assert.deepEqual(events, [], 'an unacknowledged direction never touches the writer');
  assert.equal((s.callFlags.at(-1) & 64) === 0, true, 'requests stay on owned inline frames');
  await refused.close();

  s.ackShmRequest = true;
  const accepted = transport(s, { requestShmThreshold: 0, requestShmWriter: writer });
  await accepted.call('manager', CONTRACT, 'ping', new Uint8Array(5));
  assert.deepEqual(events, [['write', 5], ['consumed']]);
  assert.equal((s.callFlags.at(-1) & 64) !== 0, true, 'the ACK enables request shared memory');
  await accepted.close();
});

test('SHM replies without the response ACK are rejected before any reader work', async () => {
  const s = server();
  const readerEvents = [];
  let allocated = 0;
  const allocator = (size) => {
    allocated += 1;
    const view = new Uint8Array(size);
    return {payload: {byteLength: size, view}, view};
  };
  const t = transport(s, {
    responsePayloadAllocator: allocator,
    responseShmReader: {
      read() { readerEvents.push('read'); },
      release() { readerEvents.push('release'); },
    },
  });
  s.shmReply = true;
  s.responseData = Buffer.from([42]);
  await assert.rejects(() => call(t), /without the acknowledged SHM response capability/);
  assert.deepEqual(readerEvents, [], 'no mapper, read, or release may run');
  assert.equal(allocated, 0, 'no response allocation may run');
  assert.equal(s.connections[0].closed, 1, 'the protocol violation discards the connection');

  s.ackShmResponse = true;
  const control = transport(s, {
    responsePayloadAllocator: allocator,
    responseShmReader: {
      read(block, destination) { readerEvents.push('read'); destination.set(Buffer.from([42])); },
      release() { readerEvents.push('release'); },
    },
  });
  assert.deepEqual(Array.from((await call(control)).view), [42]);
  assert.deepEqual(readerEvents, ['read', 'release']);
  await control.close();
});

test('provider allocator, SHM release and chunk framing remain on the same shared stream', async () => {
  const s = server();
  s.responseData = Buffer.from([42, 43, 44]);
  s.ackShmRequest = true;
  s.ackShmResponse = true;
  const allocations = [], shmEvents = [];
  const t = transport(s, {
    requestChunkSize: 2,
    responsePayloadAllocator(size) {
      const view = new Uint8Array(size);
      const payload = {byteLength: size, view};
      allocations.push(payload);
      return {payload, view};
    },
    responseShmReader: {
      async read(block, destination) {
        assert.equal(block.prefix, '/serverpool');
        assert.equal(block.generation, 1);
        assert.equal(block.allocationId, 9007199254740993n);
        shmEvents.push('read');
        destination.set(s.responseData);
      },
      async release(block) {
        assert.equal(block.allocationId, 9007199254740993n);
        shmEvents.push('response-release');
      },
    },
  });
  assert.deepEqual(Array.from((await call(t)).view), [42, 43, 44]);
  s.chunkedReply = true;
  assert.deepEqual(Array.from((await t.call('manager', CONTRACT, 'ping', new Uint8Array(7))).view), [42, 43, 44]);
  assert.equal(s.callFlags.filter((flags) => flags & 512).length, 4);
  assert.equal(s.callFlags.at(-1), 128 | 512 | 1024);
  s.chunkedReply = false; s.shmReply = true; s.responseAllocationId = 9007199254740993n;
  assert.deepEqual(Array.from((await call(t)).view), [42, 43, 44]);
  assert.deepEqual(shmEvents, ['read', 'response-release']);
  assert.deepEqual(allocations.map((p) => p.byteLength), [3, 3, 3]);
  assert.equal(s.connections.length, 1);
  await t.close();

  const requestEvents = [];
  const request = transport(s, {
    requestShmThreshold: 0,
    requestShmWriter: {
      prefix: '/requestpool', segments: [{name: '/requestpool_b0000', size: 1048576}],
      async write(payload) {
        requestEvents.push('write');
        return {segmentIndex: 0, generation: 1, offset: 0, byteLength: payload.byteLength, allocationId: 0xffffffffffffffffn, dedicated: false};
      },
      async markConsumed() {requestEvents.push('consumed');},
      async release() {requestEvents.push('request-release');},
    },
  });
  s.shmReply = false;
  await request.call('manager', CONTRACT, 'ping', new Uint8Array(7));
  assert.equal(s.callFlags.at(-1), 128 | 64);
  assert.equal(s.calls.at(-1).subarray(0, 23).toString('hex'), '000001000000000000000700000000ffffffffffffffff');
  assert.deepEqual(requestEvents, ['write', 'consumed']);
  await request.close();

  for (const allocationId of [undefined, 0n, -1n, 1, 0x1_0000_0000_0000_0000n]) {
    const invalidWriter = transport(s, {
      requestShmThreshold: 0,
      requestShmWriter: {
        prefix: '/requestpool', segments: [{name: '/requestpool_b0000', size: 1048576}],
        write(payload) {
          return {segmentIndex: 0, generation: 1, offset: 0, byteLength: payload.byteLength, allocationId, dedicated: false};
        },
        release() {},
      },
    });
    await assert.rejects(() => invalidWriter.call('manager', CONTRACT, 'ping', new Uint8Array(7)), /allocationId/);
    await invalidWriter.close();
  }

  s.shmReply = true; s.responseAllocationId = 0n;
  const invalidResponse = transport(s, {
    responseShmReader: {
      read() { throw new Error('invalid ID reached reader'); },
      release() { throw new Error('invalid ID reached release'); },
    },
  });
  await assert.rejects(() => call(invalidResponse), /allocationId/);
  await invalidResponse.close();
});
"#,
    );
}
