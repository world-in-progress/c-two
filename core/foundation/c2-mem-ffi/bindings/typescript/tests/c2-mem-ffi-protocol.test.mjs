import assert from 'node:assert/strict';
import { spawn, spawnSync } from 'node:child_process';
import { copyFileSync, mkdirSync, mkdtempSync, readFileSync, rmSync, writeFileSync } from 'node:fs';
import { createRequire } from 'node:module';
import { tmpdir } from 'node:os';
import { dirname, resolve } from 'node:path';
import { fileURLToPath, pathToFileURL } from 'node:url';
import { createInterface } from 'node:readline';
import test from 'node:test';
import { createNodeIpcConnect } from '../dist/index.js';
import { resolveCargo } from '../scripts/cargo-tools.mjs';

const packageRoot = resolve(dirname(fileURLToPath(import.meta.url)), '..');
const fixture = resolve(packageRoot, 'tests/protocol-server');
const target = process.env.CARGO_TARGET_DIR ?? resolve(packageRoot, '../../../../target');
let executable, buildFailure;
function buildServer() {
  if (executable) return executable;
  if (buildFailure) throw buildFailure;
  let result;
  try {
    copyFileSync(resolve(packageRoot, '../../../../Cargo.lock'), resolve(fixture, 'Cargo.lock'));
    result = spawnSync(resolveCargo(), ['build', '--offline', '--manifest-path', resolve(fixture, 'Cargo.toml'), '--target-dir', target], {encoding: 'utf8', timeout: 120_000});
  } finally { rmSync(resolve(fixture, 'Cargo.lock'), {force: true}); }
  if (result.status !== 0) { buildFailure = new Error(`protocol fixture build failed:\n${result.error ?? ''}\n${result.stdout}\n${result.stderr}`); throw buildFailure; }
  executable = resolve(target, 'debug', `c2-node-protocol-server${process.platform === 'win32' ? '.exe' : ''}`);
  return executable;
}
async function withServer(run, options = {}) {
  const server = buildServer();
  const directory = mkdtempSync(resolve(tmpdir(), 'c2n7-'));
  const root = resolve(directory, 'ipc'); mkdirSync(root, {mode: 0o700});
  const address = `ipc://node-p8a-${process.pid}-${Math.random().toString(16).slice(2)}`;
  const child = spawn(server, [process.platform === 'win32' ? '-' : root, address], {stdio: ['pipe', 'pipe', 'pipe']});
  const lines = createInterface({input: child.stdout})[Symbol.asyncIterator]();
  let stderr = ''; child.stderr.on('data', b => { stderr += b; });
  const closed = new Promise(resolve => child.once('exit', code => resolve(code)));
  const connect = createNodeIpcConnect(process.platform === 'win32' ? {} : {ipcRoot: root});
  const owners = [];
  const connector = async address => { const c = await connect(address); owners.push(c); return c; };
  try {
    const ready = await lines.next(); assert.equal(JSON.parse(ready.value).kind, 'ready', stderr);
    const require = createRequire(import.meta.url), ts = require('typescript');
    const source = readFileSync(new URL('../../../../c2-codegen/assets/typescript_transport.ts', import.meta.url), 'utf8');
    const module = resolve(directory, 'transport.mjs');
    writeFileSync(module, ts.transpileModule(source, {compilerOptions: {target: ts.ScriptTarget.ES2022, module: ts.ModuleKind.ES2022}}).outputText);
    const {createIpcEncodedTransport} = await import(pathToFileURL(module));
    const transport = createIpcEncodedTransport(address, {connect: connector, ...options});
    const stats = async () => { child.stdin.write('stats\n'); return JSON.parse((await lines.next()).value).allocations; };
    const settled = async () => {
      const deadline = Date.now() + 2000;
      while (await stats() !== 0) { assert.ok(Date.now() < deadline, 'release did not settle before the next call'); await new Promise(r => setTimeout(r, 10)); }
    };
    const closeTransport = await run({transport, connection: () => owners.at(-1), settled, stats});
    if (closeTransport !== false) await transport.close();
  } finally {
    for (const owner of owners) owner.close();
    connect.close(); child.stdin.end('quit\n');
    const timer = setTimeout(() => child.kill(), 12_000);
    const code = await closed; clearTimeout(timer);
    rmSync(directory, {recursive: true, force: true});
    assert.equal(code, 0, stderr);
  }
}
const contract = {schema: 'c-two.contract.v2', namespace: 'test.p8a', name: 'Echo', version: '0.1.0', descriptorSha256: 'd'.repeat(64), abiHash: 'a'.repeat(64), signatureHash: 'b'.repeat(64)};
const call = (transport, bytes) => transport.call('echo', contract, 'echo', bytes);

test('Node calls the real server after an idle interval longer than the former heartbeat timeout', {timeout: 150_000}, async () => {
  await withServer(async ({transport}) => {
    assert.deepEqual(Array.from(await call(transport, new Uint8Array([1, 2]))), [1, 2]);
    await new Promise(resolve => setTimeout(resolve, 32_000));
    assert.deepEqual(Array.from(await call(transport, new Uint8Array([3, 4]))), [3, 4]);
  });
});
test('manual PING remains independent from idle detection', {timeout: 150_000}, async () => {
  await withServer(async ({transport, connection}) => {
    await transport.prepare('echo', contract);
    const c = connection(), id = 0x20000000000001n;
    const frame = new Uint8Array(17), view = new DataView(frame.buffer);
    view.setUint32(0, 13, true); view.setBigUint64(4, id, true); view.setUint32(12, 1 << 11, true); frame[16] = 1;
    const batch = c.memory.beginWrite(1); batch.start(); await c.write(frame); batch.complete(true);
    const header = await c.readExactly(16), h = new DataView(header.buffer, header.byteOffset, header.byteLength);
    assert.equal(h.getBigUint64(4, true), id); assert.equal(h.getUint32(12, true), (1 << 11) | 2);
    assert.deepEqual(Array.from(await c.readExactly(h.getUint32(0, true) - 12)), [2]);
  });
});
test('native response copy and failed destination settle before another call', {timeout: 150_000}, async () => {
  await withServer(async ({transport, connection, settled}) => {
    const payload = new Uint8Array(8192).fill(42);
    assert.deepEqual(await call(transport, payload), payload); await settled();
    const memory = connection().memory;
    const consume = memory.consumeResponse.bind(memory);
    memory.consumeResponse = response => consume(response, new Uint8Array(0));
    try { await assert.rejects(() => call(transport, payload), /INSUFFICIENT_BUFFER/); await settled(); }
    finally { memory.consumeResponse = consume; }
    assert.deepEqual(await call(transport, payload), payload); await settled();
  });
});
test('native loader rejects lossy u64 values and old descriptor fields before mapping', {timeout: 150_000}, async () => {
  await withServer(async ({transport, connection}) => {
    await transport.prepare('echo', contract);
    const memory = connection().memory;
    const block = {segmentIndex: 0, generation: 1, offset: 0, byteLength: 1, loanId: 1n, dedicated: false};
    for (const loanId of [1, -1n, 0x10000000000000000n]) assert.throws(() => memory.admitResponse({...block, loanId}), /lossless u64 bigint/);
    for (const field of ['allocationId', 'prefix', 'segments', 'extra', 'offset\0extra', Symbol('extra')]) assert.throws(() => memory.admitResponse({...block, [field]: 1}), /Unknown block fields/);
  });
});
test('native allocator failure discards the unread response before returning', {timeout: 150_000}, async () => {
  await withServer(async ({transport, settled}) => {
    await assert.rejects(() => call(transport, new Uint8Array(8192).fill(7)), /allocator failed/);
    await settled();
  }, {responsePayloadAllocator() { throw new Error('allocator failed'); }});
});
test('native memory byte arguments reject a malformed release without allocating SHM', {timeout: 150_000}, async () => {
  await withServer(async ({transport, connection}) => {
    await transport.prepare('echo', contract);
    assert.throws(() => connection().memory.receiveRelease(new Uint8Array(16)), /INVALID_ARGUMENT/);
  });
});

test('native block integer fields reject invalid values without deactivating', {timeout: 150_000}, async () => {
  await withServer(async ({transport, connection}) => {
    await transport.prepare('echo', contract);
    const memory = connection().memory;
    const block = {segmentIndex: 0, generation: 1, offset: 0, byteLength: 8, loanId: 1n, dedicated: false};
    for (const field of ['segmentIndex', 'generation', 'offset', 'byteLength']) {
      for (const value of [-1, 0.5, field === 'segmentIndex' ? 0x10000 : 0x100000000]) {
        assert.throws(() => memory.admitResponse({...block, [field]: value}), /u32 integer|fit in u16/);
      }
    }
    memory.beginWrite(0).discard();
  });
});
test('native dedicated offset fails admission without deactivating the connection', {timeout: 150_000}, async () => {
  await withServer(async ({transport, connection}) => {
    await transport.prepare('echo', contract);
    const memory = connection().memory;
    assert.throws(() => memory.admitResponse({segmentIndex: 0, generation: 1, offset: 1, byteLength: 8, loanId: 1n, dedicated: true}), /INVALID_ARGUMENT/);
    memory.beginWrite(0).discard();
    connection().close();
    return false; // The synthetic loan ID was never issued by this server.
  });
});
test('Node close rejects native operations but still discards live request response and write owners', {timeout: 150_000}, async () => {
  await withServer(async ({transport, connection, settled, stats}) => {
    await transport.prepare('echo', contract);
    const c = connection(), memory = c.memory;
    const request = memory.prepareRequest(new Uint8Array([7, 8]));
    let unread;
    // Deliberately retain the raw admitted carrier to check the Node ownership
    // boundary, without passing it to native consume or exposing a pointer.
    memory.consumeResponse = response => { unread = response; };
    await call(transport, new Uint8Array(8192).fill(9));
    assert.ok(unread);
    const write = memory.beginWrite(1);
    assert.equal(await stats(), 1);
    c.close(); c.close();
    await new Promise(resolve => setTimeout(resolve, 20));
    assert.equal(await stats(), 1, 'an unread carrier must retain its original connection until discard');
    await assert.rejects(() => c.write(new Uint8Array([1])), /closed/);
    await assert.rejects(() => c.readExactly(1), /closed/);
    assert.throws(() => memory.prepareRequest(new Uint8Array([1])), /consumed/);
    assert.throws(() => memory.beginWrite(0), /consumed/);
    assert.throws(() => memory.finishHandshake(null), /consumed/);
    memory.discardRequest(request); memory.discardRequest(request);
    memory.discardResponse(unread); memory.discardResponse(unread);
    write.discard(); write.discard(); memory.close(); memory.close();
    await settled();
    return false; // This raw-owner probe already closed its connector directly.
  });
});
test('Node dedicated requests reuse released slots beyond the four-slot capacity', {timeout: 150_000}, async () => {
  await withServer(async ({transport, connection, settled, stats}) => {
    await transport.prepare('echo', contract);
    const memory = connection().memory, prepare = memory.prepareRequest.bind(memory), blocks = [];
    memory.prepareRequest = payload => { const request = prepare(payload); blocks.push(request.block); return request; };
    // ABI 7 uses the production 256 MiB buddy geometry. One byte beyond it
    // forces dedicated storage without a test-only allocator or config API.
    const payload = new Uint8Array(256 * 1024 * 1024 + 1).fill(7);
    for (let i = 0; i < 6; i++) {
      const reply = await transport.call('echo', contract, 'size', payload);
      assert.equal(new DataView(reply.buffer, reply.byteOffset, reply.byteLength).getUint32(0, true), payload.length);
      assert.deepEqual(Array.from(reply.subarray(4)), [7, 7]);
    }
    assert.equal(blocks.length, 6);
    assert.ok(blocks.every(block => block.dedicated));
    assert.equal(new Set(blocks.map(block => block.segmentIndex)).size, 1);
    assert.equal(new Set(blocks.map(block => block.loanId)).size, 6);
    for (let i = 1; i < blocks.length; i++) assert.ok(blocks[i].generation > blocks[i - 1].generation);
    await settled();
  });
});
