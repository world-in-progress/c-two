import assert from 'node:assert/strict';
import { mkdtempSync, readFileSync, rmSync, writeFileSync } from 'node:fs';
import { tmpdir } from 'node:os';
import { join } from 'node:path';
import { pathToFileURL } from 'node:url';
import { createRequire } from 'node:module';
import test from 'node:test';

const require = createRequire(new URL('../../c2-mem-ffi/bindings/typescript/package.json', import.meta.url));
const ts = require('typescript');
async function transportModule(start = 1n) {
  const directory = mkdtempSync(join(tmpdir(), 'c2-consume-'));
  try {
    const source = readFileSync(new URL('../assets/typescript_transport.ts', import.meta.url), 'utf8')
      .replace('let nextRequestId = 1n;', `let nextRequestId = ${start}n;`);
    const code = ts.transpileModule(`${source}\nexport { readIpcShmSuccessPayload, writeIpcFrame };\n`, {
      compilerOptions: { target: ts.ScriptTarget.ES2022, module: ts.ModuleKind.ES2022 },
    }).outputText;
    writeFileSync(join(directory, 'transport.mjs'), code);
    return await import(pathToFileURL(join(directory, 'transport.mjs')));
  } finally { rmSync(directory, { recursive: true, force: true }); }
}
const { readIpcShmSuccessPayload, writeIpcFrame } = await transportModule();
for (const allocated of [false, true]) {
  test(`SHM copy failure settles the remote read with allocator=${allocated}`, async () => {
    const events = [];
    const memory = {
      consumeResponse() { try { events.push('copy'); throw new Error('copy failed'); } finally { events.push('settled'); } },
      discardResponse() { assert.fail('a consumed response must not be discarded again'); },
    };
    const allocator = allocated ? length => { const view = new Uint8Array(length); return { view, payload: view }; } : undefined;
    await assert.rejects(() => readIpcShmSuccessPayload({}, 8, allocator, memory, async () => { events.push('flushed'); }), /copy failed/);
    assert.deepEqual(events, ['copy', 'settled', 'flushed']);
  });
}
test('allocator failure discards the unread owner and flushes before returning the failure', async () => {
  const events = [];
  await assert.rejects(() => readIpcShmSuccessPayload({}, 8, () => { throw new Error('allocator failed'); }, {
    consumeResponse() { assert.fail('allocation failed before copying'); },
    discardResponse() { events.push('discard'); },
  }, async () => { events.push('flushed'); }), /allocator failed/);
  assert.deepEqual(events, ['discard', 'flushed']);
});
test('copy failure remains primary when release writing also fails', async () => {
  await assert.rejects(() => readIpcShmSuccessPayload({}, 8, undefined, {
    consumeResponse() { throw new Error('copy failed'); }, discardResponse() { assert.fail(); },
  }, async () => { throw new Error('write failed'); }), /copy failed/);
});
test('successful consumption flushes before delivering the local buffer', async () => {
  const events = [];
  const bytes = await readIpcShmSuccessPayload({}, 8, undefined, {
    consumeResponse(_response, dst) { dst.fill(42); events.push('consumed'); }, discardResponse() { assert.fail(); },
  }, async () => { events.push('flushed'); });
  assert.deepEqual(Array.from(bytes), Array(8).fill(42));
  assert.deepEqual(events, ['consumed', 'flushed']);
});
const u16 = n => { const b = Buffer.alloc(2); b.writeUInt16LE(n); return b; };
const u32 = n => { const b = Buffer.alloc(4); b.writeUInt32LE(n); return b; };
const u64 = n => { const b = Buffer.alloc(8); b.writeBigUInt64LE(n); return b; };
const text = s => Buffer.concat([Buffer.from([Buffer.byteLength(s)]), Buffer.from(s)]);
const frame = (id, flags, data) => Buffer.concat([u32(12 + data.length), u64(id), u32(flags), data]);
const contract = {schema: 'c-two.contract.v2', namespace: 'test.p8a', name: 'Echo', version: '0.1.0', descriptorSha256: 'd'.repeat(64), abiHash: 'a'.repeat(64), signatureHash: 'b'.repeat(64)};
function peer({memory, reply, failRelease = false} = {}) {
  let buffered = Buffer.alloc(0), metadata = false;
  const ids = [], calls = [], closed = [];
  const feed = bytes => { buffered = Buffer.concat([buffered, bytes]); };
  const connection = {
    memory,
    write(data) {
      const b = Buffer.from(data), id = b.readBigUInt64LE(4), flags = b.readUInt32LE(12), payload = b.subarray(16);
      if (flags === 4) {
        if (!metadata) { metadata = true; feed(Buffer.from('1500000000000000000000000600000043324950020f000000', 'hex')); }
        else feed(frame(0n, 6, Buffer.concat([text(memory === undefined ? '' : '/peer'), u16(memory === undefined ? 0 : 16), text('server'), text('instance'), u16(0)])));
      } else if (flags === 16) {
        if (payload[0] === 0x17) { if (failRelease) throw new Error('release write failed'); return; }
        ids.push(id);
        const request = JSON.parse(payload.subarray(1).toString());
        const current = {route_name: 'echo', route_uid: 'uid', owner_server_id: 'server', owner_server_instance_id: 'instance', contract: request.expected, methods: [{name: 'echo', index: 0}], max_payload_size: 1048576, state: 'ready', state_reason: 'register_committed'};
        feed(frame(id, 18, Buffer.from([0x11, ...Buffer.from(JSON.stringify({status: 'ready', current}))])));
      } else { ids.push(id); calls.push(id); feed(frame(id, reply?.flags ?? 258, reply?.payload ?? Buffer.from([0, 42]))); }
    },
    readExactly(length) { assert.ok(buffered.length >= length); const bytes = buffered.subarray(0, length); buffered = buffered.subarray(length); return bytes; },
    close() { closed.push(true); },
  };
  return {connection, ids, calls, closed};
}
test('request IDs beyond Number.MAX_SAFE_INTEGER stay distinct on the wire', async () => {
  const { createIpcEncodedTransport } = await transportModule(9007199254740993n);
  const p = peer(), transport = createIpcEncodedTransport('ipc://p8a', {connect: () => p.connection});
  assert.deepEqual(Array.from(await transport.call('echo', contract, 'echo', new Uint8Array())), [42]);
  assert.deepEqual(p.ids, [9007199254740993n, 9007199254740994n]);
  await transport.close();
});
test('u64 request exhaustion deactivates instead of wrapping or dispatching', async () => {
  const { createIpcEncodedTransport } = await transportModule(0xffffffffffffffffn);
  const p = peer(), transport = createIpcEncodedTransport('ipc://p8a', {connect: () => p.connection});
  await assert.rejects(() => transport.call('echo', contract, 'echo', new Uint8Array()), /ID space exhausted/);
  await new Promise(resolve => setImmediate(resolve));
  assert.deepEqual(p.ids, [0xffffffffffffffffn]); assert.deepEqual(p.calls, []); assert.equal(p.closed.length, 1);
  await transport.close();
});
test('write composition failure discards the unstarted native batch', async () => {
  const events = [];
  const open = {writeChain: Promise.resolve(), connection: {
    memory: {beginWrite() { return {
      get prefix() { throw new Error('composition failed'); },
      start() { assert.fail('composition failed before publication'); },
      complete() { assert.fail('no I/O was attempted'); },
      discard() { events.push('discarded'); },
    }; }},
    write() { assert.fail('unpublished frame reached I/O'); },
  }};
  await assert.rejects(() => writeIpcFrame(open, new Uint8Array(17)), /composition failed/);
  assert.deepEqual(events, ['discarded']);
});

test('a copied provider result survives release write failure without releasing its local owner', async () => {
  let releases = 0;
  const payload = {byteLength: 8, release() { releases++; }};
  const result = await readIpcShmSuccessPayload({}, 8, length => ({view: new Uint8Array(length), payload}), {
    consumeResponse(_response, dst) { dst.fill(42); }, discardResponse() { assert.fail(); },
  }, async () => { throw new Error('release write failed'); });
  assert.equal(result, payload);
  assert.equal(releases, 0);
});

const blockBytes = ({generation = 1, offset = 0, byteLength = 8, dedicated = false, loanId = 1n} = {}) =>
  Buffer.concat([u16(0), u32(generation), u32(offset), u32(byteLength), Buffer.from([Number(dedicated)]), u64(loanId)]);
function memoryPeer() {
  let deactivated = false, release = false, admissions = 0, consumptions = 0;
  return {
    prefix: '/local', finishHandshake() {},
    get admissions() { return admissions; }, get consumptions() { return consumptions; },
    get deactivated() { return deactivated; },
    admitResponse() { admissions++; return {}; },
    consumeResponse(_response, dst) { consumptions++; dst.fill(42); release = true; },
    discardResponse() { release = true; },
    deactivate() { deactivated = true; }, tryClose() { return true; },
    beginWrite(kind) {
      if (kind === 0 && deactivated) throw new Error('new business after deactivation');
      if (kind === 2 && !release) return undefined;
      return {prefix: release ? frame(0n, 16, Buffer.concat([Buffer.from([0x17, 1]), u64(1n)])) : new Uint8Array(),
        start() {}, discard() {}, complete(success) { if (success) release = false; else deactivated = true; }};
    },
  };
}
test('release write failure delivers the successful call and discards only its connection', async () => {
  const {createIpcEncodedTransport} = await transportModule();
  const memory = memoryPeer(), first = peer({memory, failRelease: true, reply: {flags: 322, payload: Buffer.concat([blockBytes(), Buffer.from([0])])}}), next = peer();
  let connects = 0;
  const transport = createIpcEncodedTransport('ipc://p8a', {connect: () => connects++ === 0 ? first.connection : next.connection});
  assert.deepEqual(Array.from(await transport.call('echo', contract, 'echo', new Uint8Array())), Array(8).fill(42));
  assert.equal(first.calls.length, 1, 'successful server work must not be replayed');
  assert.equal(first.closed.length, 1); assert.equal(memory.deactivated, true);
  assert.deepEqual(Array.from(await transport.call('echo', contract, 'echo', new Uint8Array())), [42]);
  assert.equal(connects, 2); await transport.close();
});
for (const [label, payload, flags] of [
  ['truncated descriptor', new Uint8Array(22), 322],
  ['empty span', Buffer.concat([blockBytes({byteLength: 0}), Buffer.from([0])]), 322],
  ['zero loan ID', Buffer.concat([blockBytes({loanId: 0n}), Buffer.from([0])]), 322],
  ['invalid generation', Buffer.concat([blockBytes({generation: 0}), Buffer.from([0])]), 322],
  ['dedicated offset', Buffer.concat([blockBytes({dedicated: true, offset: 1}), Buffer.from([0])]), 322],
  ['chunked SHM', Buffer.concat([blockBytes(), Buffer.from([0])]), 322 | 512],
]) {
  test(`invalid SHM admission discards an inline request connection: ${label}`, async () => {
    const {createIpcEncodedTransport} = await transportModule();
    const memory = memoryPeer(), first = peer({memory, reply: {payload, flags}}), next = peer();
    let connects = 0;
    const transport = createIpcEncodedTransport('ipc://p8a', {connect: () => connects++ === 0 ? first.connection : next.connection});
    await assert.rejects(() => transport.call('echo', contract, 'echo', new Uint8Array()));
    assert.equal(first.closed.length, 1); assert.equal(memory.deactivated, true);
    assert.equal(memory.admissions, 0); assert.equal(memory.consumptions, 0);
    assert.deepEqual(Array.from(await transport.call('echo', contract, 'echo', new Uint8Array())), [42]);
    assert.equal(connects, 2); await transport.close();
  });
}
test('native SHM admission failure deactivates and discards the original connection', async () => {
  const {createIpcEncodedTransport} = await transportModule();
  const memory = memoryPeer(); memory.admitResponse = () => { throw new Error('span outside mapping'); };
  const p = peer({memory, reply: {flags: 322, payload: Buffer.concat([blockBytes(), Buffer.from([0])])}});
  const transport = createIpcEncodedTransport('ipc://p8a', {connect: () => p.connection});
  await assert.rejects(() => transport.call('echo', contract, 'echo', new Uint8Array()), /span outside mapping/);
  assert.equal(p.closed.length, 1); assert.equal(memory.deactivated, true); await transport.close();
});
test('deactivation rejects a new route query while existing control writes remain allowed', async () => {
  const {createIpcEncodedTransport} = await transportModule();
  const memory = memoryPeer(), p = peer({memory});
  const transport = createIpcEncodedTransport('ipc://p8a', {connect: () => p.connection});
  await transport.prepare('echo', contract); memory.deactivate();
  const before = p.ids.length;
  await assert.rejects(() => transport.prepare('echo', contract), /deactivation/);
  assert.equal(p.ids.length, before, 'no query may reach the stream after deactivation');
  const written = [];
  await writeIpcFrame({writeChain: Promise.resolve(), connection: {memory, write(bytes) { written.push(bytes); }}}, frame(0n, 2048, Buffer.from([8])), undefined, true);
  assert.equal(written.length, 1); await transport.close();
});
