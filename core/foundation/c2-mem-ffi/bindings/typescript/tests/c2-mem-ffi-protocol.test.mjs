import assert from 'node:assert/strict';
import {spawn, spawnSync} from 'node:child_process';
import {copyFileSync, mkdirSync, mkdtempSync, readFileSync, rmSync, writeFileSync} from 'node:fs';
import {createRequire} from 'node:module';
import {tmpdir} from 'node:os';
import {dirname, resolve} from 'node:path';
import {fileURLToPath, pathToFileURL} from 'node:url';
import {createInterface} from 'node:readline';
import test from 'node:test';
import {Worker} from 'node:worker_threads';
import {createNodeIpcConnector} from '../dist/index.js';
import {resolveCargo} from '../scripts/cargo-tools.mjs';

const packageRoot = resolve(dirname(fileURLToPath(import.meta.url)), '..');
const fixture = resolve(packageRoot, 'tests/protocol-server');
const target = process.env.CARGO_TARGET_DIR ?? resolve(packageRoot, '../../../../target');

function streamInput(mode, count, length = 1) {
  const input = new Uint8Array(6);
  input[0] = mode; input[1] = count;
  new DataView(input.buffer).setUint32(2, length, true);
  return input;
}

let executable;
function buildServer() {
  if (executable) return executable;
  let result;
  try {
    copyFileSync(resolve(packageRoot, '../../../../Cargo.lock'), resolve(fixture, 'Cargo.lock'));
    result = spawnSync(resolveCargo(), ['build', '--offline', '--manifest-path', resolve(fixture, 'Cargo.toml'), '--target-dir', target], {encoding: 'utf8', timeout: 120_000});
  } finally { rmSync(resolve(fixture, 'Cargo.lock'), {force: true}); }
  assert.equal(result.status, 0, `protocol fixture build failed:\n${result.error ?? ''}\n${result.stdout}\n${result.stderr}`);
  executable = resolve(target, 'debug', `c2-node-protocol-server${process.platform === 'win32' ? '.exe' : ''}`);
  return executable;
}

async function withServer(run) {
  const server = buildServer();
  const directory = mkdtempSync(resolve(tmpdir(), 'c2n8-'));
  const root = resolve(directory, 'ipc'); mkdirSync(root, {mode: 0o700});
  const address = `ipc://node-abi8-${process.pid}-${Math.random().toString(16).slice(2)}`;
  const child = spawn(server, [process.platform === 'win32' ? '-' : root, address], {stdio: ['pipe', 'pipe', 'pipe']});
  const lines = createInterface({input: child.stdout})[Symbol.asyncIterator]();
  let stderr = ''; child.stderr.on('data', b => { stderr += b; });
  const closed = new Promise(resolve => child.once('exit', code => resolve(code)));
  const connector = createNodeIpcConnector(process.platform === 'win32' ? {} : {ipcRoot: root});
  try {
    const ready = JSON.parse((await lines.next()).value);
    assert.equal(ready.kind, 'ready', stderr);
    const require = createRequire(import.meta.url);
    const ts = require('typescript');
    const source = readFileSync(resolve(packageRoot, '../../../c2-codegen/assets/typescript_transport.ts'), 'utf8');
    const modulePath = resolve(directory, 'transport.mjs');
    writeFileSync(modulePath, ts.transpileModule(source, {compilerOptions: {
      target: ts.ScriptTarget.ES2022, module: ts.ModuleKind.ES2022,
      typeRoots: [resolve(directory, 'node_modules', '@types')],
    }}).outputText);
    const {createIpcEncodedTransport} = await import(pathToFileURL(modulePath));
    await run({connector, address, lines, child, contract: ready.contract, createIpcEncodedTransport, modulePath, directory, root});
  } finally {
    connector.close();
    child.stdin.end('quit\n');
    const timer = setTimeout(() => child.kill(), 12_000);
    const code = await closed; clearTimeout(timer);
    rmSync(directory, {recursive: true, force: true});
    assert.equal(code, 0, stderr);
  }
}

async function unary(endpoint, contract, bytes) {
  await endpoint.prepare('echo', contract);
  const call = endpoint.beginCall('echo', 'ping', false, 10_000);
  try {
    await call.chargeInput(bytes.byteLength);
    await call.dispatch(bytes);
    const response = await call.next();
    assert.ok(response);
    try { return Uint8Array.from(response); }
    finally { response.release(); }
  } finally { call.close(); }
}

async function beginStream(endpoint, contract, mode, count, length = 1) {
  await endpoint.prepare('echo', contract);
  const input = streamInput(mode, count, length);
  const call = endpoint.beginCall('echo', 'echo', true, 10_000);
  await call.chargeInput(input.byteLength);
  await call.dispatch(input);
  return call;
}

async function takeItem(call, expectedLength, value) {
  const item = await call.next();
  assert.ok(item);
  try {
    assert.equal(item.byteLength, expectedLength);
    assert.ok(item.every(byte => byte === value));
  } finally { item.release(); }
}

test('ABI 8 native endpoint completes unary calls and reuses the connector domain', {timeout: 150_000}, async () => {
  await withServer(async ({connector, address, lines, child, contract}) => {
    const first = await connector(address);
    try {
      assert.deepEqual(await unary(first, contract, new Uint8Array([1, 2])), new Uint8Array([1, 2]));
      child.stdin.write('stats\n');
      assert.equal(JSON.parse((await lines.next()).value).allocations, 0, 'released response owner settled before next call');
    } finally { await first.close(); }
    const second = await connector(address);
    try {
      assert.deepEqual(await unary(second, contract, new Uint8Array([3, 4])), new Uint8Array([3, 4]));
      assert.equal(typeof connector.stats(), 'object');
    } finally { await first.close(); await second.close(); }
  });
});

test('ABI 8 real IPC backing survives view GC and release survives backing GC', {timeout: 150_000}, async () => {
  await withServer(async ({address, contract, root}) => {
    const script = `
      import assert from 'node:assert/strict';
      const {createNodeIpcConnector} = await import(process.argv[1]);
      const connector = createNodeIpcConnector(process.platform === 'win32' ? {} : {ipcRoot: process.argv[4]});
      const endpoint = await connector(process.argv[3]);
      await endpoint.prepare('echo', JSON.parse(process.argv[2]));
      async function next(bytes) {
        const call = endpoint.beginCall('echo', 'ping', false, 10_000);
        try {
          await call.chargeInput(bytes.byteLength);
          await call.dispatch(bytes);
          return await call.next();
        } finally { call.close(); }
      }
      try {
        let view = await next(Uint8Array.of(11, 22, 33));
        let backing = view.buffer;
        let alias = new Uint8Array(backing);
        let viewFinalized = false;
        const registry = new FinalizationRegistry(() => { viewFinalized = true; });
        registry.register(view, 1);
        view = null;
        for (let attempt = 0; attempt < 300 && !viewFinalized; attempt++) {
          global.gc(); await new Promise(resolve => setImmediate(resolve));
        }
        assert.equal(viewFinalized, true, 'the view finalized while its ArrayBuffer alias stayed live');
        assert.deepEqual([...alias], [11, 22, 33]);
        backing = null;
        alias = null;
        let released = await next(Uint8Array.of(44, 55));
        let releasedBacking = released.buffer;
        let releasedAlias = new Uint8Array(releasedBacking);
        released.release();
        assert.equal(releasedBacking.byteLength, 0);
        assert.equal(releasedAlias.byteLength, 0);
        releasedBacking = null;
        releasedAlias = null;
        for (let attempt = 0; attempt < 4; attempt++) {
          global.gc(); await new Promise(resolve => setImmediate(resolve));
        }
        released.release();
        released = null;
      } finally { await endpoint.close(); connector.close(); }
    `;
    const gc = spawnSync(process.execPath, ['--expose-gc', '--input-type=module', '-e', script,
      pathToFileURL(resolve(packageRoot, 'dist/index.js')).href, JSON.stringify(contract), address, root],
    {encoding: 'utf8'});
    assert.equal(gc.status, 0, `${gc.signal ?? ''} ${gc.stdout}${gc.stderr}`);
  });
});

test('closing one same-address endpoint leaves the other endpoint callable', {timeout: 150_000}, async () => {
  await withServer(async ({connector, address, contract}) => {
    const first = await connector(address);
    const second = await connector(address);
    try {
      await first.prepare('echo', contract);
      assert.deepEqual(await unary(second, contract, new Uint8Array([1])), new Uint8Array([1]));
      await first.close();
      assert.deepEqual(await unary(second, contract, new Uint8Array([2])), new Uint8Array([2]));
    } finally { await second.close(); }
  });
});

test('ABI 8 server streams deliver zero, one and many inline or large items', {timeout: 150_000}, async () => {
  await withServer(async ({connector, address, contract}) => {
    const endpoint = await connector(address);
    try {
      for (const [count, length] of [[0, 1], [1, 1], [5, 8192]]) {
        const call = await beginStream(endpoint, contract, 0, count, length);
        try {
          for (let i = 0; i < count; i++) await takeItem(call, length, i);
          assert.equal(await call.next(), null);
          assert.deepEqual(await call.observe(null), {completed: true, error: null, completeItems: BigInt(count)});
        } finally { call.close(); }
      }
    } finally { await endpoint.close(); }
  });
});

test('ABI 8 stream retains completed item count after a resource error', {timeout: 150_000}, async () => {
  await withServer(async ({connector, address, contract}) => {
    const endpoint = await connector(address);
    try {
      const call = await beginStream(endpoint, contract, 1, 2);
      try {
        await takeItem(call, 1, 0);
        await takeItem(call, 1, 1);
        await assert.rejects(() => call.next(), /fixture stream failure/);
        const receipt = await call.observe(null);
        assert.equal(receipt.completed, true);
        assert.equal(receipt.completeItems, 2n);
        assert.match(receipt.error.message, /fixture stream failure/);
        assert.equal(typeof receipt.error.nativeDetails, 'object');
      } finally { call.close(); }
    } finally { await endpoint.close(); }
  });
});

test('ABI 8 handles two streams and concurrent unary calls on one endpoint', {timeout: 150_000}, async () => {
  await withServer(async ({connector, address, contract}) => {
    const endpoint = await connector(address);
    try {
      const [a, b] = await Promise.all([
        beginStream(endpoint, contract, 0, 3, 4), beginStream(endpoint, contract, 0, 4, 8192),
      ]);
      try {
        await Promise.all([
          (async () => { for (let i = 0; i < 3; i++) await takeItem(a, 4, i); assert.equal(await a.next(), null); })(),
          (async () => { for (let i = 0; i < 4; i++) await takeItem(b, 8192, i); assert.equal(await b.next(), null); })(),
          ...Array.from({length: 4}, (_, i) => unary(endpoint, contract, new Uint8Array([i, i + 1])).then(reply => {
            assert.deepEqual(reply, new Uint8Array([i, i + 1]));
          })),
        ]);
        assert.equal((await a.observe(null)).completeItems, 3n);
        assert.equal((await b.observe(null)).completeItems, 4n);
      } finally { a.close(); b.close(); }
    } finally { await endpoint.close(); }
  });
});

test('ABI 8 completes a second unary while the first is blocked on the same connection', {timeout: 150_000}, async () => {
  await withServer(async ({connector, address, contract, lines, child}) => {
    const endpoint = await connector(address);
    try {
      const firstBinding = await endpoint.prepare('echo', contract);
      let firstSettled = false;
      const first = unary(endpoint, contract, new Uint8Array([255])).then(reply => {
        firstSettled = true;
        return reply;
      });
      assert.equal(JSON.parse((await lines.next()).value).kind, 'unary_blocked');
      const secondBinding = await endpoint.prepare('echo', contract);
      assert.equal(secondBinding.connectionId, firstBinding.connectionId);
      assert.deepEqual(await unary(endpoint, contract, new Uint8Array([42])), new Uint8Array([42]));
      assert.equal(firstSettled, false);
      child.stdin.write('release_unary\n');
      assert.equal(JSON.parse((await lines.next()).value).kind, 'unary_released');
      assert.deepEqual(await first, new Uint8Array([255]));
      assert.equal((await endpoint.prepare('echo', contract)).connectionId, firstBinding.connectionId);
    } finally {
      child.stdin.write('release_unary\n');
      await endpoint.close();
    }
  });
});

test('ABI 8 settles a gated native call after its Worker environment terminates', {timeout: 150_000}, async () => {
  await withServer(async ({address, contract, root, lines, child}) => {
    const workerSource = `
      const {parentPort, workerData} = require('node:worker_threads');
      (async () => {
        const {createNodeIpcConnector} = await import(workerData.module);
        const connector = createNodeIpcConnector(workerData.options);
        const endpoint = await connector(workerData.address);
        await endpoint.prepare('echo', workerData.contract);
        const call = endpoint.beginCall('echo', 'ping', false, null);
        await call.chargeInput(1);
        const pending = call.dispatch(Uint8Array.of(254));
        parentPort.postMessage({kind: 'dispatch_pending'});
        await pending;
      })().catch(error => parentPort.postMessage({kind: 'error', message: String(error)}));
    `;
    const worker = new Worker(workerSource, {eval: true, workerData: {
      module: pathToFileURL(resolve(packageRoot, 'dist/index.js')).href,
      options: process.platform === 'win32' ? {} : {ipcRoot: root}, address, contract,
    }});
    try {
      const message = new Promise((resolve, reject) => {
        worker.once('message', resolve);
        worker.once('error', reject);
        worker.once('exit', code => reject(new Error(`Worker exited before native next: ${code}`)));
      });
      assert.deepEqual(await message, {kind: 'dispatch_pending'});
      assert.equal(JSON.parse((await lines.next()).value).kind, 'unary_blocked');
      assert.equal(await worker.terminate(), 1);
      child.stdin.write('release_unary\n');
      const finished = new Set([
        JSON.parse((await lines.next()).value).kind,
        JSON.parse((await lines.next()).value).kind,
      ]);
      assert.deepEqual(finished, new Set(['unary_released', 'unary_completed']));
    } finally {
      child.stdin.write('release_unary\n');
      await worker.terminate();
    }
  });
});

for (const cause of ['shutdown', 'unregister']) {
  test(`ABI 8 ${cause} delivers the running step before cancelling an unlimited stream`, {timeout: 150_000}, async () => {
    await withServer(async ({connector, address, contract, lines, child}) => {
      const endpoint = await connector(address);
      try {
        const call = await beginStream(endpoint, contract, 7, 0);
        try {
          await takeItem(call, 1, 0);
          assert.equal(JSON.parse((await lines.next()).value).kind, 'step_blocked');
          child.stdin.write(`${cause}\n`);
          assert.deepEqual(JSON.parse((await lines.next()).value), {kind: 'route_closed', cause});
          child.stdin.write('release\n');
          assert.equal(JSON.parse((await lines.next()).value).kind, 'released');
          await takeItem(call, 1, 1);
          await assert.rejects(() => call.next(), error =>
            error.code === 718 && error.canonicalName === 'CallCancelled' &&
            error.nativeDetails?.message.includes(cause));
          const receipt = await call.observe(null);
          assert.equal(receipt.completed, true);
          assert.equal(receipt.completeItems, 2n);
          assert.equal(receipt.error?.code, 718);
          assert.equal(receipt.error?.canonicalName, 'CallCancelled');
          assert.match(receipt.error.nativeDetails.message, new RegExp(cause));
        } finally { call.close(); }
      } finally {
        child.stdin.write('release\n');
        await endpoint.close();
      }
    });
  });
}

test('ABI 8 cancellation observes pending cleanup, then completes after a host barrier', {timeout: 150_000}, async () => {
  await withServer(async ({connector, address, contract, lines, child}) => {
    const endpoint = await connector(address);
    try {
      const call = await beginStream(endpoint, contract, 2, 3);
      try {
        await takeItem(call, 1, 0);
        assert.equal(JSON.parse((await lines.next()).value).kind, 'step_blocked');
        call.cancel();
        const pending = await call.observe(0);
        assert.equal(pending.completed, false);
        child.stdin.write('release\n');
        assert.equal(JSON.parse((await lines.next()).value).kind, 'released');
        const receipt = await call.observe(null);
        assert.equal(receipt.completed, true);
        assert.ok(receipt.completeItems === 1n || receipt.completeItems === 2n);
        assert.ok(receipt.error);
      } finally { call.close(); }
      assert.deepEqual(await unary(endpoint, contract, new Uint8Array([8])), new Uint8Array([8]));
    } finally { await endpoint.close(); }
  });
});

test('holding one native item keeps the shared payload budget until release', {timeout: 150_000}, async () => {
  const previous = process.env.C2_CALL_PAYLOAD_BUFFER_BUDGET_BYTES;
  process.env.C2_CALL_PAYLOAD_BUFFER_BUDGET_BYTES = '4102';
  try {
    await withServer(async ({connector, address, contract, lines}) => {
      const endpoint = await connector(address);
      try {
        const first = await beginStream(endpoint, contract, 0, 1, 4096);
        const held = await first.next();
        assert.ok(held);
        assert.equal(held.byteLength, 4096);
        assert.equal(connector.stats().payload_buffer.limit_bytes, '4102');
        assert.equal(connector.stats().payload_buffer.used_bytes, '4096');
        const second = await beginStream(endpoint, contract, 3, 1, 4096);
        try {
          const pending = second.next();
          assert.equal(JSON.parse((await lines.next()).value).kind, 'second_stream_step');
          assert.equal(connector.stats().payload_buffer.used_bytes, '4096');
          held.release();
          const item = await pending;
          assert.ok(item);
          assert.equal(item.byteLength, 4096);
          item.release();
          assert.equal(await second.next(), null);
          assert.equal((await second.observe(null)).completeItems, 1n);
        } finally { second.close(); first.close(); }
      } finally { await endpoint.close(); }
    });
  } finally {
    if (previous === undefined) delete process.env.C2_CALL_PAYLOAD_BUFFER_BUDGET_BYTES;
    else process.env.C2_CALL_PAYLOAD_BUFFER_BUDGET_BYTES = previous;
  }
});

test('public stream close(0) reports pending cleanup and close(null) observes completion', {timeout: 150_000}, async () => {
  await withServer(async ({connector, address, contract, lines, child, createIpcEncodedTransport}) => {
    const transport = createIpcEncodedTransport(address, {connect: connector});
    try {
      const stream = transport.stream('echo', contract, 'echo', () => streamInput(2, 3), bytes => Uint8Array.from(bytes));
      assert.deepEqual((await stream.next()).value, new Uint8Array([0]));
      assert.equal(JSON.parse((await lines.next()).value).kind, 'step_blocked');
      const pending = await stream.close(0);
      assert.equal(pending.completed, false);
      child.stdin.write('release\n');
      assert.equal(JSON.parse((await lines.next()).value).kind, 'released');
      const receipt = await stream.close(null);
      assert.equal(receipt.completed, true);
      assert.ok(receipt.completeItems === 1n || receipt.completeItems === 2n);
      assert.ok(receipt.error);
    } finally { await transport.close(); }
  });
});

test('public iterator return submits cancellation without waiting for blocked cleanup', {timeout: 150_000}, async () => {
  await withServer(async ({connector, address, contract, lines, child, createIpcEncodedTransport}) => {
    const transport = createIpcEncodedTransport(address, {connect: connector});
    try {
      const stream = transport.stream('echo', contract, 'echo', () => streamInput(2, 3), bytes => Uint8Array.from(bytes));
      assert.deepEqual((await stream.next()).value, new Uint8Array([0]));
      assert.equal(JSON.parse((await lines.next()).value).kind, 'step_blocked');
      assert.deepEqual(await stream.return(), {done: true, value: undefined});
      child.stdin.write('release\n');
      assert.equal(JSON.parse((await lines.next()).value).kind, 'released');
      const receipt = await stream.close(null);
      assert.equal(receipt.completed, true);
      assert.ok(receipt.error);
    } finally { await transport.close(); }
  });
});

test('public stream deadline leaves the original record running and later reports natural completion', {timeout: 150_000}, async () => {
  await withServer(async ({connector, address, contract, lines, child, createIpcEncodedTransport}) => {
    const transport = createIpcEncodedTransport(address, {connect: connector, callTimeoutMs: 1000});
    try {
      const stream = transport.stream('echo', contract, 'echo', () => streamInput(4, 2), bytes => Uint8Array.from(bytes));
      assert.deepEqual((await stream.next()).value, new Uint8Array([0]));
      assert.equal(JSON.parse((await lines.next()).value).kind, 'step_blocked');
      await assert.rejects(() => stream.next(), error => error.code === 715 && error.transportPhase === 'dispatch_uncertain');
      child.stdin.write('release\n');
      const events = new Set([JSON.parse((await lines.next()).value).kind, JSON.parse((await lines.next()).value).kind]);
      assert.deepEqual(events, new Set(['released', 'continued_after_deadline']));
      assert.deepEqual(await stream.close(null), {completed: true, error: null, completeItems: 2n});
    } finally { await transport.close(); }
  });
});

test('ABI 8 unary encoding consumes the native pre-dispatch deadline', {timeout: 150_000}, async () => {
  await withServer(async ({connector, address, contract, lines, child, createIpcEncodedTransport}) => {
    const transport = createIpcEncodedTransport(address, {connect: connector, callTimeoutMs: 1});
    let encoded = false;
    try {
      await transport.prepare('echo', contract);
      await assert.rejects(() => transport.call('echo', contract, 'ping', () => {
        encoded = true;
        const start = performance.now();
        let iterations = 0;
        while (performance.now() - start < 20) iterations++;
        return Uint8Array.of(iterations & 255);
      }), error => error.code === 715 && error.transportPhase === 'pre_dispatch');
    } finally { await transport.close(); }
    assert.equal(encoded, true);
    child.stdin.write('stats\n');
    const stats = JSON.parse((await lines.next()).value);
    assert.equal(stats.kind, 'stats');
    assert.equal(stats.unary_dispatches, 0, 'expired input never reached the resource');
  });
});


// Force a separate process so this gate always exercises real GC, including when
// the outer Node test runner was not launched with --expose-gc.
test('collecting a public stream with next pending submits native CANCEL and runs resource cleanup', {timeout: 150_000}, async () => {
  await withServer(async ({address, contract, lines, child: server, modulePath, directory, root}) => {
    const script = resolve(directory, 'collect-stream.mjs');
    writeFileSync(script, `
import assert from 'node:assert/strict';
import {setImmediate} from 'node:timers/promises';
import {createNodeIpcConnector} from ${JSON.stringify(pathToFileURL(resolve(packageRoot, 'dist/index.js')).href)};
import {createIpcEncodedTransport} from ${JSON.stringify(pathToFileURL(modulePath).href)};
const connector = createNodeIpcConnector(${JSON.stringify(process.platform === 'win32' ? {} : {ipcRoot: root})});
const emit = kind => process.stdout.write(JSON.stringify({kind}) + '\\n');
let cancelled = false;
let receipt;
let nextStarted;
const pendingStarted = new Promise(resolve => { nextStarted = resolve; });
const connect = async (...args) => {
  const endpoint = await connector(...args);
  return new Proxy(endpoint, {get(target, key) {
    if (key === 'beginCall') return (...callArgs) => {
      const call = target.beginCall(...callArgs);
      let reads = 0;
      return new Proxy(call, {get(native, member) {
        if (member === 'cancel') return () => {
          native.cancel();
          if (!cancelled) { cancelled = true; emit('native_cancel_submitted'); }
        };
        if (member === 'next') return () => {
          const pending = native.next();
          if (++reads === 2) {
            receipt = native.observe(null);
            nextStarted();
            emit('next_pending');
          }
          return pending;
        };
        const value = Reflect.get(native, member);
        return typeof value === 'function' ? value.bind(native) : value;
      }});
    };
    const value = Reflect.get(target, key);
    return typeof value === 'function' ? value.bind(target) : value;
  }});
};
const transport = createIpcEncodedTransport(${JSON.stringify(address)}, {connect});
try {
  let stream = transport.stream('echo', ${JSON.stringify(contract)}, 'echo',
    () => new Uint8Array([5, 3, 1, 0, 0, 0]), bytes => Uint8Array.from(bytes));
  assert.deepEqual((await stream.next()).value, new Uint8Array([0]));
  const pending = stream.next().then(value => ({value}), error => ({error}));
  await pendingStarted;
  await new Promise(resolve => process.stdin.once('data', resolve));
  process.stdin.pause();
  stream = null;
  assert.equal(typeof globalThis.gc, 'function');
  for (let turn = 0; !cancelled && turn < 10000; turn++) {
    globalThis.gc();
    await setImmediate();
  }
  assert.equal(cancelled, true, 'public stream remained rooted while native next was pending');
  const result = await pending;
  assert.equal(result.error?.code, 718);
  const terminal = await receipt;
  assert.equal(terminal.completed, true);
  assert.equal(terminal.error?.code, 718);
  assert.ok(terminal.completeItems === 1n || terminal.completeItems === 2n);
  emit('gc_complete');
} finally {
  await transport.close();
  connector.close();
}
`);
    const gc = spawn(process.execPath, ['--expose-gc', script], {stdio: ['pipe', 'pipe', 'pipe']});
    const events = createInterface({input: gc.stdout})[Symbol.asyncIterator]();
    let stderr = ''; gc.stderr.on('data', bytes => { stderr += bytes; });
    const exited = new Promise(resolve => gc.once('exit', code => resolve(code)));
    try {
      assert.equal(JSON.parse((await events.next()).value).kind, 'next_pending');
      assert.equal(JSON.parse((await lines.next()).value).kind, 'step_blocked');
      gc.stdin.write('collect\n');
      assert.equal(JSON.parse((await events.next()).value).kind, 'native_cancel_submitted');
      server.stdin.write('release\n');
      const finished = new Set([
        JSON.parse((await lines.next()).value).kind,
        JSON.parse((await lines.next()).value).kind,
      ]);
      assert.deepEqual(finished, new Set(['released', 'stream_cleanup']));
      const completedEvent = await events.next();
      assert.equal(completedEvent.done, false, stderr);
      assert.equal(JSON.parse(completedEvent.value).kind, 'gc_complete', stderr);
      assert.equal(await exited, 0, stderr);
    } finally {
      gc.kill();
      server.stdin.write('release\n');
      await exited;
    }
  });
});

test('multiple public held stream items fill B and releasing one resumes next', {timeout: 150_000}, async () => {
  const previous = process.env.C2_CALL_PAYLOAD_BUFFER_BUDGET_BYTES;
  process.env.C2_CALL_PAYLOAD_BUFFER_BUDGET_BYTES = '8192';
  try {
    await withServer(async ({connector, address, contract, lines, createIpcEncodedTransport}) => {
      const transport = createIpcEncodedTransport(address, {connect: connector});
      const holds = [];
      try {
        const stream = transport.stream('echo', contract, 'echo', () => streamInput(6, 3, 4096), bytes => bytes, true);
        for (let index = 0; index < 2; index++) {
          const item = await stream.next();
          assert.equal(item.done, false);
          holds.push(item.value);
          assert.equal(item.value.value.byteLength, 4096);
          assert.ok(item.value.value.every(byte => byte === index));
          assert.deepEqual(JSON.parse((await lines.next()).value), {kind: 'item_step', index});
        }
        assert.equal(connector.stats().payload_buffer.used_bytes, '8192');
        let settled = false;
        const pending = stream.next().then(item => { settled = true; return item; });
        assert.deepEqual(JSON.parse((await lines.next()).value), {kind: 'item_step', index: 2});
        assert.equal(settled, false);
        assert.equal(connector.stats().payload_buffer.used_bytes, '8192');
        holds[0].release();
        assert.throws(() => holds[0].value, /released/);
        const resumed = await pending;
        assert.equal(resumed.done, false);
        holds.push(resumed.value);
        assert.ok(resumed.value.value.every(byte => byte === 2));
        assert.ok(holds[1].value.every(byte => byte === 1));
        assert.equal(connector.stats().payload_buffer.used_bytes, '8192');
        holds[1].release();
        assert.equal(connector.stats().payload_buffer.used_bytes, '4096');
        resumed.value.release();
        assert.equal(connector.stats().payload_buffer.used_bytes, '0');
        assert.deepEqual(await stream.next(), {done: true, value: undefined});
        assert.deepEqual(await stream.close(null), {completed: true, error: null, completeItems: 3n});
      } finally {
        for (const held of holds) held.release();
        await transport.close();
      }
    });
  } finally {
    if (previous === undefined) delete process.env.C2_CALL_PAYLOAD_BUFFER_BUDGET_BYTES;
    else process.env.C2_CALL_PAYLOAD_BUFFER_BUDGET_BYTES = previous;
  }
});

test('public holds keep the same native domain charge across distinct connections and reconnect', {timeout: 150_000}, async () => {
  const previous = process.env.C2_CALL_PAYLOAD_BUFFER_BUDGET_BYTES;
  process.env.C2_CALL_PAYLOAD_BUFFER_BUDGET_BYTES = '4102';
  try {
    await withServer(async ({connector, address, contract, lines, createIpcEncodedTransport}) => {
      const transports = [];
      const holds = [];
      const waitForInputRefund = async () => {
        const deadline = performance.now() + 10_000;
        while (performance.now() < deadline) {
          if (connector.stats().payload_buffer.used_bytes === '4096') return;
          await new Promise(resolve => setImmediate(resolve));
        }
        assert.fail('stream input reservation was not refunded');
      };
      const open = async mode => {
        const endpoint = await connector(address);
        const binding = await endpoint.prepare('echo', contract);
        const transport = createIpcEncodedTransport(address, {connect: async () => endpoint});
        transports.push(transport);
        const stream = transport.stream('echo', contract, 'echo', () => streamInput(mode, 1, 4096), bytes => bytes, true);
        return {endpoint, binding, transport, stream};
      };
      try {
        const first = await open(0);
        const original = (await first.stream.next()).value;
        holds.push(original);
        assert.deepEqual(await first.stream.next(), {done: true, value: undefined});
        assert.equal(connector.stats().payload_buffer.used_bytes, '4096');

        const second = await open(3);
        assert.notEqual(second.binding.connectionId, first.binding.connectionId);
        let settled = false;
        const pending = second.stream.next().then(item => { settled = true; return item; });
        assert.equal(JSON.parse((await lines.next()).value).kind, 'second_stream_step');
        // Resource entry can precede the client's input reservation refund.
        await waitForInputRefund();
        assert.equal(settled, false);
        assert.equal(connector.stats().payload_buffer.used_bytes, '4096');
        await first.transport.close();
        assert.equal(original.value.byteLength, 4096);
        assert.ok(original.value.every(byte => byte === 0));
        assert.equal(connector.stats().payload_buffer.used_bytes, '4096');
        original.release();
        const retained = (await pending).value;
        holds.push(retained);
        assert.deepEqual(await second.stream.next(), {done: true, value: undefined});
        await second.transport.close();
        assert.equal(connector.stats().payload_buffer.used_bytes, '4096');

        const reconnected = await open(3);
        assert.notEqual(reconnected.binding.connectionId, second.binding.connectionId);
        let resumed = false;
        const next = reconnected.stream.next().then(item => { resumed = true; return item; });
        assert.equal(JSON.parse((await lines.next()).value).kind, 'second_stream_step');
        await waitForInputRefund();
        assert.equal(resumed, false);
        assert.equal(connector.stats().payload_buffer.used_bytes, '4096');
        assert.equal(retained.value.byteLength, 4096);
        assert.ok(retained.value.every(byte => byte === 0));
        retained.release();
        const last = (await next).value;
        holds.push(last);
        assert.equal(last.value.byteLength, 4096);
        assert.equal(connector.stats().payload_buffer.used_bytes, '4096');
        last.release();
        assert.equal(connector.stats().payload_buffer.used_bytes, '0');
        assert.deepEqual(await reconnected.stream.next(), {done: true, value: undefined});
        assert.deepEqual(await reconnected.stream.close(null), {completed: true, error: null, completeItems: 1n});
      } finally {
        for (const held of holds) held.release();
        for (const transport of transports) await transport.close();
      }
    });
  } finally {
    if (previous === undefined) delete process.env.C2_CALL_PAYLOAD_BUFFER_BUDGET_BYTES;
    else process.env.C2_CALL_PAYLOAD_BUFFER_BUDGET_BYTES = previous;
  }
});

test('a failed first native prepare freezes the factory endpoint root for retries', {
  timeout: 150_000,
  skip: process.platform === 'win32' && 'Unix process-root selection; Windows uses the SID-derived endpoint',
}, async () => {
  await withServer(async ({address, contract, root, directory}) => {
    const previousRoot=process.env.C2_IPC_ROOT;
    const previousFile=process.env.C2_ENV_FILE;
    let connector, failed, endpoint;
    try {
      process.env.C2_IPC_ROOT=root;
      process.env.C2_ENV_FILE='';
      connector=createNodeIpcConnector();
      failed=await connector(`ipc://node-failed-prepare-${process.pid}`);
      await assert.rejects(failed.prepare('echo',contract));
      await failed.close();
      process.env.C2_IPC_ROOT=resolve(directory,'other-root');
      endpoint=await connector(address);
      assert.ok(connector.resolveEndpoint(address).startsWith(`${root}/`));
      assert.deepEqual(await unary(endpoint,contract,Uint8Array.of(7)),Uint8Array.of(7));
      const fresh=createNodeIpcConnector();
      try { assert.notEqual(fresh.resolveEndpoint(address),connector.resolveEndpoint(address)); }
      finally { fresh.close(); }
    } finally {
      await endpoint?.close(); await failed?.close(); connector?.close();
      if(previousRoot===undefined)delete process.env.C2_IPC_ROOT;else process.env.C2_IPC_ROOT=previousRoot;
      if(previousFile===undefined)delete process.env.C2_ENV_FILE;else process.env.C2_ENV_FILE=previousFile;
    }
  });
});
