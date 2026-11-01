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
function peer({memory, reply, controlReply, failRelease = false} = {}) {
  let buffered = Buffer.alloc(0), metadata = false, controlReplies = 0;
  const ids = [], calls = [], closed = [], releaseFrames = [];
  const feed = bytes => { buffered = Buffer.concat([buffered, bytes]); };
  const connection = {
    memory,
    write(data) {
      const b = Buffer.from(data), id = b.readBigUInt64LE(4), flags = b.readUInt32LE(12), payload = b.subarray(16);
      if (flags === 4) {
        if (!metadata) { metadata = true; feed(Buffer.from('1500000000000000000000000600000043324950020f000000', 'hex')); }
        else feed(frame(0n, 6, Buffer.concat([text(memory === undefined ? '' : '/peer'), u16(memory === undefined ? 0 : 16), text('server'), text('instance'), u16(0)])));
      } else if (flags === 16) {
        if (payload[0] === 0x17) { if (failRelease) throw new Error('release write failed'); releaseFrames.push(b); return; }
        ids.push(id);
        const request = JSON.parse(payload.subarray(1).toString());
        const current = {route_name: 'echo', route_uid: 'uid', owner_server_id: 'server', owner_server_instance_id: 'instance', contract: request.expected, methods: [{name: 'echo', index: 0}], max_payload_size: 1048576, state: 'ready', state_reason: 'register_committed'};
        const selected = Array.isArray(controlReply) ? controlReply[controlReplies] : controlReply;
        controlReplies++;
        feed(frame(id, selected?.flags ?? 18, selected?.payload ?? Buffer.from([0x11, ...Buffer.from(JSON.stringify({status: 'ready', current}))])));
      } else {
        ids.push(id); calls.push(id);
        const selected = Array.isArray(reply) ? reply[calls.length - 1] : reply;
        for (const part of selected?.frames ?? [selected ?? {}]) {
          feed(frame(id, part.flags ?? 258, part.payload ?? Buffer.from([0, 42])));
        }
      }
    },
    readExactly(length) { assert.ok(buffered.length >= length); const bytes = buffered.subarray(0, length); buffered = buffered.subarray(length); return bytes; },
    close() { closed.push(true); },
  };
  return {connection, ids, calls, closed, releaseFrames, feed};
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
  let deactivated = false, release = false, admissions = 0, consumptions = 0, discards = 0;
  return {
    prefix: '/local', finishHandshake() {},
    get admissions() { return admissions; }, get consumptions() { return consumptions; }, get discards() { return discards; },
    get deactivated() { return deactivated; },
    queueRelease() { release = true; },
    admitResponse(block) {
      admissions++;
      if (block.byteLength === 0 || block.loanId === 0n || block.generation === 0 || (block.dedicated && block.offset !== 0)) {
        if (block.loanId !== 0n) release = true;
        throw new Error('native rejected SHM descriptor');
      }
      return {};
    },
    consumeResponse(_response, dst) { consumptions++; dst.fill(42); release = true; },
    discardResponse() { discards++; release = true; },
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
for (const [label, payload, flags, expectedAdmissions, expectedReleases] of [
  ['truncated descriptor', new Uint8Array(22), 322, 0, 0],
  ['empty span', Buffer.concat([blockBytes({byteLength: 0}), Buffer.from([0])]), 322, 1, 1],
  ['zero loan ID', Buffer.concat([blockBytes({loanId: 0n}), Buffer.from([0])]), 322, 1, 0],
  ['invalid generation', Buffer.concat([blockBytes({generation: 0}), Buffer.from([0])]), 322, 1, 1],
  ['dedicated offset', Buffer.concat([blockBytes({dedicated: true, offset: 1}), Buffer.from([0])]), 322, 1, 1],
  ['chunked SHM', Buffer.concat([blockBytes(), Buffer.from([0])]), 322 | 512, 1, 1],
]) {
  test(`invalid SHM reply fails its call and preserves the connection: ${label}`, async () => {
    const {createIpcEncodedTransport} = await transportModule();
    const memory = memoryPeer(), first = peer({memory, reply: [{payload, flags}, undefined]});
    let connects = 0;
    const transport = createIpcEncodedTransport('ipc://p8a', {connect: () => { connects++; return first.connection; }});
    await assert.rejects(() => transport.call('echo', contract, 'echo', new Uint8Array()));
    assert.equal(first.closed.length, 0); assert.equal(memory.deactivated, false);
    assert.equal(memory.admissions, expectedAdmissions); assert.equal(memory.consumptions, 0);
    assert.equal(first.releaseFrames.length, expectedReleases);
    assert.deepEqual(Array.from(await transport.call('echo', contract, 'echo', new Uint8Array())), [42]);
    assert.equal(connects, 1); await transport.close();
  });
}
test('native SHM admission failure queues release and preserves the original connection', async () => {
  const {createIpcEncodedTransport} = await transportModule();
  const memory = memoryPeer(); memory.admitResponse = () => { memory.queueRelease(); throw new Error('span outside mapping'); };
  const p = peer({memory, reply: [{flags: 322, payload: Buffer.concat([blockBytes(), Buffer.from([0])])}, undefined]});
  const transport = createIpcEncodedTransport('ipc://p8a', {connect: () => p.connection});
  await assert.rejects(() => transport.call('echo', contract, 'echo', new Uint8Array()), /span outside mapping/);
  assert.equal(p.closed.length, 0); assert.equal(memory.deactivated, false);
  assert.equal(p.releaseFrames.length, 1, 'failed native admission must flush its release');
  assert.deepEqual(Array.from(await transport.call('echo', contract, 'echo', new Uint8Array())), [42]);
  await transport.close();
});
for (const [label, flags, control] of [
  ['invalid flags', 66, 0],
  ['invalid reply control', 322, 255],
]) {
  test(`decodable SHM descriptor is released after ${label}`, async () => {
    const {createIpcEncodedTransport} = await transportModule();
    const memory = memoryPeer(), p = peer({memory, reply: [{flags, payload: Buffer.concat([blockBytes(), Buffer.from([control])])}, undefined]});
    const transport = createIpcEncodedTransport('ipc://p8a', {connect: () => p.connection});
    await assert.rejects(() => transport.call('echo', contract, 'echo', new Uint8Array()));
    assert.equal(memory.admissions, 1); assert.equal(memory.discards, 1);
    assert.equal(p.releaseFrames.length, 1); assert.equal(p.closed.length, 0);
    assert.deepEqual(Array.from(await transport.call('echo', contract, 'echo', new Uint8Array())), [42]);
    await transport.close();
  });
}
test('malformed chunked reply fails one call and skips its remaining frames', async () => {
  const {createIpcEncodedTransport} = await transportModule();
  const chunk = (index, last = false) => ({flags: 258 | 512 | (last ? 1024 : 0), payload: Buffer.concat([u64(2n), u32(2), u32(index), Buffer.from([index + 1])])});
  const p = peer({reply: [{frames: [chunk(1), chunk(1, true)]}, undefined]});
  const transport = createIpcEncodedTransport('ipc://p8a', {connect: () => p.connection});
  await assert.rejects(() => transport.call('echo', contract, 'echo', new Uint8Array()), /first chunked response/);
  assert.equal(p.closed.length, 0);
  assert.deepEqual(Array.from(await transport.call('echo', contract, 'echo', new Uint8Array())), [42]);
  assert.equal(p.calls.length, 2); await transport.close();
});
test('late failed-call chunks are skipped after another call has completed', async () => {
  const {createIpcEncodedTransport} = await transportModule();
  const chunk = (index, last = false) => ({flags: 258 | 512 | (last ? 1024 : 0), payload: Buffer.concat([u64(2n), u32(2), u32(index), Buffer.from([index + 1])])});
  const p = peer({reply: [{frames: [chunk(1)]}, undefined, undefined]});
  const transport = createIpcEncodedTransport('ipc://p8a', {connect: () => p.connection});
  await assert.rejects(() => transport.call('echo', contract, 'echo', new Uint8Array()), /first chunked response/);
  assert.deepEqual(Array.from(await transport.call('echo', contract, 'echo', new Uint8Array())), [42]);
  p.feed(frame(p.calls[0], chunk(1, true).flags, chunk(1, true).payload));
  assert.deepEqual(Array.from(await transport.call('echo', contract, 'echo', new Uint8Array())), [42]);
  assert.equal(p.closed.length, 0); assert.equal(p.calls.length, 3);
  await transport.close();
});
test('invalid complete inline reply fails one call and preserves the connection', async () => {
  const {createIpcEncodedTransport} = await transportModule();
  const p = peer({reply: [{flags: 258, payload: Buffer.from([255])}, undefined]});
  const transport = createIpcEncodedTransport('ipc://p8a', {connect: () => p.connection});
  await assert.rejects(() => transport.call('echo', contract, 'echo', new Uint8Array()), /reply status/);
  assert.equal(p.closed.length, 0);
  assert.deepEqual(Array.from(await transport.call('echo', contract, 'echo', new Uint8Array())), [42]);
  assert.equal(p.calls.length, 2); await transport.close();
});
test('a queued call completes after an earlier complete reply fails', async () => {
  const {createIpcEncodedTransport} = await transportModule();
  const p = peer({reply: [{flags: 258, payload: Buffer.from([255])}, undefined]});
  const transport = createIpcEncodedTransport('ipc://p8a', {connect: () => p.connection});
  const failed = transport.call('echo', contract, 'echo', new Uint8Array());
  const queued = transport.call('echo', contract, 'echo', new Uint8Array());
  await assert.rejects(failed, /reply status/);
  assert.deepEqual(Array.from(await queued), [42]);
  assert.equal(p.closed.length, 0); assert.equal(p.calls.length, 2);
  await transport.close();
});
test('invalid complete route reply fails only its lookup', async () => {
  const {createIpcEncodedTransport} = await transportModule();
  const p = peer({controlReply: [{flags: 18, payload: Buffer.from([255])}, undefined]});
  const transport = createIpcEncodedTransport('ipc://p8a', {connect: () => p.connection});
  await assert.rejects(() => transport.prepare('echo', contract), /catalog.*response tag/);
  assert.equal(p.closed.length, 0);
  assert.deepEqual(Array.from(await transport.call('echo', contract, 'echo', new Uint8Array())), [42]);
  await transport.close();
});
test('SHM descriptor in invalid complete route reply is released', async () => {
  const {createIpcEncodedTransport} = await transportModule();
  const memory = memoryPeer();
  const p = peer({memory, controlReply: [{flags: 82, payload: Buffer.concat([blockBytes(), Buffer.from([0x11])])}, undefined]});
  const transport = createIpcEncodedTransport('ipc://p8a', {connect: () => p.connection});
  await assert.rejects(() => transport.prepare('echo', contract), /invalid request id or flags/);
  assert.equal(memory.admissions, 1); assert.equal(memory.discards, 1);
  assert.equal(p.releaseFrames.length, 1); assert.equal(p.closed.length, 0);
  assert.deepEqual(Array.from(await transport.call('echo', contract, 'echo', new Uint8Array())), [42]);
  await transport.close();
});
test('local control write preparation failure preserves the connection', async () => {
  const {createIpcEncodedTransport} = await transportModule();
  const memory = memoryPeer(), beginWrite = memory.beginWrite.bind(memory);
  let failPreparation = true;
  memory.beginWrite = (kind, request) => {
    if (kind === 0 && failPreparation) { failPreparation = false; throw new Error('local memory failure'); }
    return beginWrite(kind, request);
  };
  const p = peer({memory}), transport = createIpcEncodedTransport('ipc://p8a', {connect: () => p.connection});
  await assert.rejects(() => transport.prepare('echo', contract), /local memory failure/);
  assert.equal(p.closed.length, 0);
  assert.deepEqual(Array.from(await transport.call('echo', contract, 'echo', new Uint8Array())), [42]);
  await transport.close();
});
test('local release preparation failure retains the connection and pending release', async () => {
  const {createIpcEncodedTransport} = await transportModule();
  const memory = memoryPeer(), beginWrite = memory.beginWrite.bind(memory);
  let failPreparation = true, connects = 0;
  memory.beginWrite = (kind, request) => {
    if (kind === 2 && failPreparation) {
      failPreparation = false;
      throw new Error('local release allocation failed');
    }
    return beginWrite(kind, request);
  };
  const p = peer({memory, reply: [{flags: 322, payload: Buffer.concat([blockBytes(), Buffer.from([0])])}, undefined]});
  const write = p.connection.write.bind(p.connection);
  p.connection.write = bytes => {
    const data = Buffer.from(bytes);
    for (let offset = 0; offset < data.length;) {
      const end = offset + data.readUInt32LE(offset) + 4;
      write(data.subarray(offset, end));
      offset = end;
    }
  };
  const transport = createIpcEncodedTransport('ipc://p8a', {connect: () => {connects++; return p.connection;}});
  assert.deepEqual(Array.from(await transport.call('echo', contract, 'echo', new Uint8Array())), Array(8).fill(42));
  assert.equal(p.closed.length, 0); assert.equal(memory.deactivated, false);
  assert.equal(p.releaseFrames.length, 0, 'an unprepared release must stay pending');
  assert.deepEqual(Array.from(await transport.call('echo', contract, 'echo', new Uint8Array())), [42]);
  assert.equal(p.releaseFrames.length, 1, 'the next write settles the pending release');
  assert.equal(connects, 1);
  await transport.close();
});
test('SHM copy failure releases its loan and preserves the connection', async () => {
  const {createIpcEncodedTransport} = await transportModule();
  const memory = memoryPeer(), consume = memory.consumeResponse.bind(memory);
  let failCopy = true;
  memory.consumeResponse = (response, dst) => {
    if (failCopy) { failCopy = false; memory.queueRelease(); throw new Error('copy failed'); }
    consume(response, dst);
  };
  const p = peer({memory, reply: [{flags: 322, payload: Buffer.concat([blockBytes(), Buffer.from([0])])}, undefined]});
  const transport = createIpcEncodedTransport('ipc://p8a', {connect: () => p.connection});
  await assert.rejects(() => transport.call('echo', contract, 'echo', new Uint8Array()), /copy failed/);
  assert.equal(p.releaseFrames.length, 1); assert.equal(p.closed.length, 0);
  assert.deepEqual(Array.from(await transport.call('echo', contract, 'echo', new Uint8Array())), [42]);
  await transport.close();
});
test('local response allocation failure preserves a complete inline connection', async () => {
  const {createIpcEncodedTransport} = await transportModule();
  const p = peer(); let failAllocation = true;
  const transport = createIpcEncodedTransport('ipc://p8a', {connect: () => p.connection, responsePayloadAllocator(length) {
    if (failAllocation) { failAllocation = false; throw new Error('allocation failed'); }
    const view = new Uint8Array(length); return {view, payload: view};
  }});
  await assert.rejects(() => transport.call('echo', contract, 'echo', new Uint8Array()), /allocation failed/);
  assert.equal(p.closed.length, 0);
  assert.deepEqual(Array.from(await transport.call('echo', contract, 'echo', new Uint8Array())), [42]);
  await transport.close();
});
test('incomplete frame discards its connection before a new call', async () => {
  const {createIpcEncodedTransport} = await transportModule();
  const broken = peer({reply: [{frames: []}]}), next = peer(); let connects = 0;
  const transport = createIpcEncodedTransport('ipc://p8a', {connect: () => connects++ === 0 ? broken.connection : next.connection});
  await assert.rejects(() => transport.call('echo', contract, 'echo', new Uint8Array()), /frame read failed/);
  assert.equal(broken.closed.length, 1);
  assert.deepEqual(Array.from(await transport.call('echo', contract, 'echo', new Uint8Array())), [42]);
  assert.equal(connects, 2); await transport.close();
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
