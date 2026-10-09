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
  dedicated: false,
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
  dedicated: false,
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
  dedicated: true,
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
fn persistent_ipc_acquire_uses_authority_and_keeps_bound_identities() {
    let project = CompiledTransportProject::compile();
    project.run_node_fixture(
        "persistent_ipc_contract.mjs",
        r#"import assert from 'node:assert/strict';
import test from 'node:test';
import { createIpcEncodedTransport, createRelayAwareHttpEncodedTransport } from './dist/typescript/c_two_contract.js';

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
const handshake = (routes, instance) => Buffer.concat([
  Buffer.from([11]), text('/serverpool'), u16(0), u16(7), text('server'), text(instance), u16(routes.length),
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
    nextControl: undefined, corrupt: undefined,
  };
  s.connect = async () => {
    let buffered = Buffer.alloc(0);
    let businessReply = false;
    const c = {
      closed: 0,
      async write(data) {
        const b = Buffer.from(data);
        assert.equal(b.readUInt32LE(0), b.length - 4);
        const id = b.readBigUInt64LE(4), flags = b.readUInt32LE(12);
        const payload = b.subarray(16);
        const reply = (tag, value) => {
          const bytes = Buffer.concat([Buffer.from([tag]), Buffer.from(JSON.stringify(value))]);
          buffered = Buffer.concat([buffered, frame(id, 2 | 16, bytes)]);
        };
        if (flags === 4) {
          buffered = Buffer.concat([buffered, frame(0n, 2 | 4, handshake([...s.routes.values()], s.instance))]);
        } else if (flags === 16) {
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
            buffered = Buffer.concat([buffered, s.corrupt(id)]);
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
          if ((flags & 512) !== 0 && (flags & 1024) === 0) return;
          businessReply = true;
          const body = Buffer.concat([Buffer.from([0]), s.responseData]);
          if (s.chunkedReply) {
            for (let index = 0; index < 2; index += 1) {
              const data = s.responseData.subarray(index * 2, (index + 1) * 2);
              buffered = Buffer.concat([buffered, frame(id, 2 | 256 | 512 | (index === 1 ? 1024 : 0), Buffer.concat([u64(s.responseData.length), u32(2), u32(index), data]))]);
            }
          } else if (s.shmReply) {
            const buddy = Buffer.concat([u16(0), u32(1), u32(0), u32(s.responseData.length), Buffer.from([0])]);
            buffered = Buffer.concat([buffered, frame(id, 2 | 256 | 64, Buffer.concat([buddy, Buffer.from([0])]))]);
          } else {
            buffered = Buffer.concat([buffered, frame(id, 2 | 256, body)]);
          }
        }
      },
      async readExactly(n) {
        assert.ok(buffered.length >= n, 'control/call frame stream must stay serialized');
        const b = buffered.subarray(0, n); buffered = buffered.subarray(n);
        if (businessReply) {
          businessReply = false;
          if (s.failRead) { s.failRead = false; throw new Error('uncertain read'); }
        }
        return b;
      },
      async close() { this.closed += 1; },
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

test('provider allocator, SHM release and chunk framing remain on the same shared stream', async () => {
  const s = server();
  s.responseData = Buffer.from([42, 43, 44]);
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
        shmEvents.push('read');
        destination.set(s.responseData);
      },
      async release() {shmEvents.push('response-release');},
    },
  });
  assert.deepEqual(Array.from((await call(t)).view), [42, 43, 44]);
  s.chunkedReply = true;
  assert.deepEqual(Array.from((await t.call('manager', CONTRACT, 'ping', new Uint8Array(7))).view), [42, 43, 44]);
  assert.equal(s.callFlags.filter((flags) => flags & 512).length, 4);
  assert.equal(s.callFlags.at(-1), 128 | 512 | 1024);
  s.chunkedReply = false; s.shmReply = true;
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
        return {segmentIndex: 0, generation: 1, offset: 0, byteLength: payload.byteLength, dedicated: false};
      },
      async markConsumed() {requestEvents.push('consumed');},
      async release() {requestEvents.push('request-release');},
    },
  });
  s.shmReply = false;
  await request.call('manager', CONTRACT, 'ping', new Uint8Array(7));
  assert.equal(s.callFlags.at(-1), 128 | 64);
  assert.deepEqual(requestEvents, ['write', 'consumed']);
  await request.close();
});
"#,
    );
}
