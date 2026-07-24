import assert from 'node:assert/strict';
import { createHash } from 'node:crypto';
import { readFile } from 'node:fs/promises';
import { pathToFileURL } from 'node:url';

import { createBundledC2MemFfiNodeRuntime } from '@c-two/c2-mem-ffi';
import {
  BuildPolicy,
  Builder,
  CompiledSpec,
  PayloadError,
  initPayload,
} from 'fastdb4ts/payload';

const configPath = process.argv[2];
if (configPath === undefined) {
  throw new Error('usage: node typescript_real_call.mjs CONFIG.json');
}
const config = JSON.parse(await readFile(configPath, 'utf8'));
await initPayload();

const contract = await import(pathToFileURL(config.contractModule).href);
const payloadApi = config.payloadModule === null
  ? undefined
  : await import(pathToFileURL(config.payloadModule).href);
const observations = [];
const nodeRuntime = createBundledC2MemFfiNodeRuntime();
const responseShmReader = contract.createC2MemFfiNativeBuddyResponseShmReader({
  binding: nodeRuntime.responsePoolFactory,
});

function transportFor(responsePayloadAllocator) {
  const observe = (observation) => observations.push(observation);
  const ipc = {
    connect: nodeRuntime.connect,
    responseShmReader,
    requestChunkSize: 64 * 1024,
    responsePayloadAllocator,
  };
  if (config.mode === 'direct-ipc') {
    return contract.createIpcEncodedTransport(config.endpoint, {
      ...ipc,
      observe,
    });
  }
  if (config.mode === 'explicit-relay') {
    return contract.createHttpRelayEncodedTransport(config.endpoint, {
      observe,
      responsePayloadAllocator,
      responsePayloadUnknownLengthStrategy: 'buffer',
    });
  }
  if (config.mode === 'relay-aware-local-ipc') {
    return contract.createRelayAwareHttpEncodedTransport(config.endpoint, {
      observe,
      ipc,
      responsePayloadAllocator,
      responsePayloadUnknownLengthStrategy: 'buffer',
    });
  }
  if (config.mode === 'relay-aware-http') {
    return contract.createRelayAwareHttpEncodedTransport(config.endpoint, {
      observe,
      responsePayloadAllocator,
      responsePayloadUnknownLengthStrategy: 'buffer',
    });
  }
  throw new Error(`unsupported TypeScript real-call mode ${config.mode}`);
}

async function closeTransport(transport) {
  if (typeof transport.close === 'function') {
    await transport.close();
    await transport.close();
  }
}

function usingView(view, operation) {
  try {
    return operation(view);
  } finally {
    view.dispose();
  }
}

function usingAccess(access, operation) {
  try {
    return operation(access);
  } finally {
    access.dispose();
  }
}

function root(payload, entryIndex = 0) {
  const sequence = payload.entryView(entryIndex);
  try {
    return sequence.at(0n);
  } finally {
    sequence.dispose();
  }
}

function buildRecordPayload(api) {
  const spec = api.compileSpec();
  const builder = Builder.create(spec);
  spec.dispose();
  try {
    builder
      .entryBegin(0, 1n)
      .valueComponentBegin()
      .valueBool(true)
      .valueU8(0xab)
      .valueU16(0x1234)
      .valueU32(0x89ab_cdef)
      .valueI32(-42)
      .valueU8n(0)
      .valueU16n(1)
      .valueF32Bits(0x3fc0_0000)
      .valueF64Bits(0x4004_0000_0000_0000n)
      .valueStr('\ufeffA\0B')
      .valueWstrUnits(
        new Uint16Array([0xfeff, 0x0041, 0, 0xd83c, 0xdf0d, 0x03a9]),
      )
      .valueBytes(new Uint8Array([0, 1, 0xff]))
      .valueComponentBegin()
      .valueListBegin(3n)
      .valueListBegin(0n)
      .valueNull()
      .valueListBegin(3n)
      .valueStr('')
      .valueNull()
      .valueStr('tail');
    builder
      .entryBegin(1, 4n)
      .valueNull()
      .valueListBegin(0n)
      .valueListBegin(3n)
      .valueU8(0)
      .valueNull()
      .valueU8(0xff)
      .valueListBegin(1n)
      .valueU8(7);
    const plan = builder.freeze();
    try {
      return plan.execute(BuildPolicy.AllowStaging).payload;
    } finally {
      plan.dispose();
    }
  } finally {
    builder.dispose();
  }
}

function buildGraphPayload(api) {
  const spec = api.compileSpec();
  const nodeIndex = spec.componentIndex('Node');
  const assetIndex = spec.componentIndex('Asset');
  const builder = Builder.create(spec);
  spec.dispose();
  try {
    const node = builder.declareObject(nodeIndex);
    const asset = builder.declareObject(assetIndex);
    builder
      .objectFillBegin(node)
      .valueBool(true)
      .valueU8(0x12)
      .valueU16(0x3456)
      .valueU32(0x789a_bcde)
      .valueI32(-1_234_567)
      .valueU8nBits(0x3fe0_0000_0000_0000n)
      .valueU16nBits(0n)
      .valueF32Bits(0x7fa1_2345)
      .valueF64Bits(0xfff8_0000_0000_1234n)
      .valueStr('same')
      .valueWstrUnits(new Uint16Array([0x0041, 0xd83d, 0xde00]))
      .valueBytes(new Uint8Array([0, 0xff, 0x7e]))
      .valueComponentBegin()
      .valueNull()
      .valueU16(0xbeef)
      .valueListBegin(3n)
      .valueF32Bits(0x8000_0000)
      .valueNull()
      .valueF32Bits(0xff80_0001)
      .valueRef(node)
      .valueRef(asset)
      .objectFillBegin(asset)
      .valueStr('same')
      .valueRef(node)
      .entryBegin(0, 1n)
      .valueObject(node)
      .entryBegin(1, 1n)
      .valueObject(asset)
      .entryBegin(2, 2n)
      .valueRef(node)
      .valueNull()
      .entryBegin(3, 3n)
      .valueU8nBits(0n)
      .valueU8nBits(0x3fe0_0000_0000_0000n)
      .valueU8nBits(0x3ff0_0000_0000_0000n)
      .entryBegin(4, 3n)
      .valueU16nBits(0xbff0_0000_0000_0000n)
      .valueU16nBits(0n)
      .valueU16nBits(0x3ff0_0000_0000_0000n);
    const plan = builder.freeze();
    try {
      return plan.execute(BuildPolicy.AllowStaging).payload;
    } finally {
      plan.dispose();
    }
  } finally {
    builder.dispose();
  }
}

function inspectRecordPayload(payload) {
  const value = root(payload);
  assert.equal(value.fieldCount(), 14);
  usingView(value.field(0), (field) => assert.equal(field.getBool(), true));
  usingView(value.field(1), (field) => assert.equal(field.getU8(), 0xab));
  usingView(value.field(9), (field) =>
    usingAccess(field.acquire(), (access) => assert.equal(access.str(), '\ufeffA\0B')));
  usingView(value.field(10), (field) =>
    usingAccess(field.acquire(), (access) => assert.equal(access.wstr(), '\ufeffA\0🌍Ω')));
  usingView(value.field(11), (field) =>
    usingAccess(
      field.acquire(),
      (access) => assert.deepEqual(Array.from(access.bytes()), [0, 1, 0xff]),
    ));
  return value;
}

function inspectGraphPayload(payload) {
  const value = root(payload);
  const identity = value.graphIdentity();
  usingView(value.field(9), (field) =>
    usingAccess(field.acquire(), (access) => assert.equal(access.str(), 'same')));
  usingView(value.field(10), (field) =>
    usingAccess(field.acquire(), (access) => assert.equal(access.wstr(), 'A😀')));
  usingView(value.field(11), (field) =>
    usingAccess(
      field.acquire(),
      (access) => assert.deepEqual(Array.from(access.bytes()), [0, 0xff, 0x7e]),
    ));
  usingView(value.field(14), (selfRef) => {
    assert.deepEqual(selfRef.graphIdentity(), identity);
    usingView(selfRef.refTarget(), (target) =>
      assert.deepEqual(target.graphIdentity(), identity));
  });
  usingView(value.field(15), (assetRef) =>
    usingView(assetRef.refTarget(), (asset) =>
      usingView(asset.field(1), (ownerRef) =>
        usingView(ownerRef.refTarget(), (owner) =>
          assert.deepEqual(owner.graphIdentity(), identity)))));
  return value;
}

function assertDetached(payloadProfile, detached) {
  if (payloadProfile === 'record-v1') {
    usingView(detached.field(1), (field) => assert.equal(field.getU8(), 0xab));
  } else {
    const identity = detached.graphIdentity();
    usingView(detached.field(14), (selfRef) =>
      usingView(selfRef.refTarget(), (target) =>
        assert.deepEqual(target.graphIdentity(), identity)));
  }
}

function assertInvalidated(view) {
  assert.throws(
    () => view.kind(),
    (error) => error instanceof PayloadError && error.symbol === 'VIEW_INVALIDATED',
  );
}

function canonicalJson(value) {
  if (Array.isArray(value)) {
    return `[${value.map(canonicalJson).join(',')}]`;
  }
  if (value !== null && typeof value === 'object') {
    return `{${Object.keys(value).sort().map(
      (key) => `${JSON.stringify(key)}:${canonicalJson(value[key])}`,
    ).join(',')}}`;
  }
  return JSON.stringify(value);
}

function logicalResultSha256(profile) {
  const logical = {
    'no-payload': {
      profile: 'no-payload',
      result: { ping: 'ok' },
      schema: 'c-two.portable-logical-result.v1',
    },
    'record-v1': {
      profile: 'record-v1',
      result: {
        bytes_hex: '0001ff',
        nested: [[], null, ['', null, 'tail']],
        record_bool: true,
        record_u8: 0xab,
        series: [null, [], [0, null, 0xff], [7]],
        str: '\ufeffA\0B',
        wstr: '\ufeffA\0🌍Ω',
      },
      schema: 'c-two.portable-logical-result.v1',
    },
    'object-graph-v1': {
      profile: 'object-graph-v1',
      result: {
        bytes_hex: '00ff7e',
        mutual_cycle: true,
        nested_null: true,
        self_cycle: true,
        shared_reference: true,
        str: 'same',
        wstr: 'A😀',
      },
      schema: 'c-two.portable-logical-result.v1',
    },
  }[profile];
  return createHash('sha256').update(canonicalJson(logical)).digest('hex');
}

function bytesFromHex(value) {
  return new Uint8Array(Buffer.from(value, 'hex'));
}

function proveFrozenDigestCause() {
  const otherSpec = CompiledSpec.compile(new TextEncoder().encode(JSON.stringify({
    schema: 'fastdb.payload.v1',
    profile: 'record.v1',
    entries: [{
      id: 'value',
      cardinality: 'one',
      type: { kind: 'u16', nullable: false },
    }],
    components: [],
  })));
  const builder = Builder.create(otherSpec);
  otherSpec.dispose();
  let payload;
  try {
    builder.entryBegin(0, 1n).valueU16(7);
    const plan = builder.freeze();
    try {
      payload = plan.execute(BuildPolicy.AllowStaging).payload;
    } finally {
      plan.dispose();
    }
  } finally {
    builder.dispose();
  }
  try {
    assert.throws(
      () => payload.requireSpecSha256(
        bytesFromHex(
          config.fastdbDigestMismatchCause.fastdb_details_json
            .match(/"expected":"([0-9a-f]{64})"/)[1],
        ),
      ),
      (error) => {
        assert.ok(error instanceof PayloadError);
        assert.deepEqual(
          contract.projectFastDbCause(error),
          config.fastdbDigestMismatchCause,
        );
        return true;
      },
    );
  } finally {
    payload.dispose();
  }
}

async function invokePrimary() {
  const transport = transportFor(undefined);
  const client = new contract.ContractClient(transport, config.routeName);
  let source;
  let checkedView;
  let detached;
  try {
    if (config.payload === 'no-payload') {
      assert.equal(await client.method_0_ping(), undefined);
      return {
        closeIdempotent: true,
        checkedViewInvalidated: null,
        materializedSurvived: null,
      };
    }
    source = config.payload === 'record-v1'
      ? buildRecordPayload(payloadApi)
      : buildGraphPayload(payloadApi);
    const held = await client.hold_method_0_roundtrip(source);
    checkedView = config.payload === 'record-v1'
      ? inspectRecordPayload(held.value)
      : inspectGraphPayload(held.value);
    detached = checkedView.materialize();
    held.release();
    held.release();
    assertInvalidated(checkedView);
    assertDetached(config.payload, detached);
    return {
      closeIdempotent: true,
      checkedViewInvalidated: true,
      materializedSurvived: true,
    };
  } finally {
    checkedView?.dispose();
    detached?.dispose();
    source?.dispose();
    await closeTransport(transport);
  }
}

async function proveOpaqueAllocatorRelease() {
  let released = 0;
  const responsePayloadAllocator = (byteLength) => {
    const payload = {
      byteLength,
      release() {
        released += 1;
      },
    };
    return {
      payload,
      view: new Uint8Array(byteLength),
    };
  };
  const transport = transportFor(responsePayloadAllocator);
  const client = new contract.ContractClient(transport, config.routeName);
  const source = config.payload === 'record-v1'
    ? buildRecordPayload(payloadApi)
    : buildGraphPayload(payloadApi);
  try {
    await assert.rejects(
      () => client.method_0_roundtrip(source),
      /opaque response allocators are not supported/,
    );
    assert.equal(released, 1);
    return { rejected: true, released: 1 };
  } finally {
    source.dispose();
    await closeTransport(transport);
  }
}

let lifecycle;
let opaqueAllocator = null;
try {
  lifecycle = await invokePrimary();
  if (config.proveDigestCause) {
    proveFrozenDigestCause();
  }
  if (config.proveOpaqueAllocator) {
    opaqueAllocator = await proveOpaqueAllocatorRelease();
  }
} finally {
  await responseShmReader.close();
}

const expectedPath = {
  'direct-ipc': 'DirectIpc',
  'explicit-relay': 'ExplicitRelay',
  'relay-aware-local-ipc': 'RelayAwareLocalIpc',
  'relay-aware-http': 'RelayAwareHttp',
}[config.mode];
assert.ok(observations.length >= 1);
assert.ok(observations.every((observation) => observation.path === expectedPath));
assert.ok(observations.every((observation) => observation.routeUid));
assert.ok(observations.every((observation) => observation.routeRevision > 0));

process.stdout.write(`NODE_RECEIPT ${JSON.stringify({
  cleanup: {
    responseShmReaderClosed: true,
    transportClosedIdempotently: true,
  },
  lifecycle,
  logicalResultSha256: logicalResultSha256(config.payload),
  observations,
  opaqueAllocator,
})}\n`);
