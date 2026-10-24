import assert from 'node:assert/strict';
import { spawn, spawnSync } from 'node:child_process';
import { randomBytes } from 'node:crypto';
import { closeSync, copyFileSync, existsSync, mkdirSync, mkdtempSync, readdirSync, rmSync, statSync } from 'node:fs';
import { dirname, isAbsolute, resolve } from 'node:path';
import test from 'node:test';
import { fileURLToPath } from 'node:url';

import {
  captureLocalIpcEndpointContext,
  createBundledC2MemFfiNodeRuntime,
  createNodeIpcConnect,
  loadBundledC2MemFfiNodeNativeSymbols,
} from '../dist/index.js';
import { resolveCargo } from '../scripts/cargo-tools.mjs';

const packageRoot = resolve(dirname(fileURLToPath(import.meta.url)), '..');
const fixtureRoot = resolve(packageRoot, 'tests', 'endpoint-context-server');
const hostCommand = 'mkdir -p /tmp/c2n && C2_ENDPOINT_CONTEXT_TEST_ROOT=/tmp/c2n node --test tests/c2-mem-ffi-endpoint-context-transport.test.mjs';

// This gate exchanges actual bytes over c2-local streams. It does not exercise
// portable RPC, FastDB payloads, SHM or a release matrix.
let serverExecutable;
function buildServer() {
  if (serverExecutable !== undefined) return serverExecutable;
  const target = process.env.CARGO_TARGET_DIR ?? resolve(packageRoot, 'target', 'endpoint-context-server');
  let result;
  try {
    copyFileSync(resolve(packageRoot, '../../../../Cargo.lock'), resolve(fixtureRoot, 'Cargo.lock'));
    result = spawnSync(resolveCargo(), [
      'build', '--offline', '--manifest-path', resolve(fixtureRoot, 'Cargo.toml'),
      '--target-dir', target,
    ], { cwd: packageRoot, encoding: 'utf8', timeout: 100_000 });
  } finally {
    // Cargo's generated lock is not a package input or historical receipt.
    rmSync(resolve(fixtureRoot, 'Cargo.lock'), { force: true });
  }
  assert.equal(result.status, 0, `fixture build failed: ${result.error ?? ''}\n${result.stdout}\n${result.stderr}`);
  serverExecutable = resolve(target, 'debug', `c2-node-endpoint-context-server${process.platform === 'win32' ? '.exe' : ''}`);
  return serverExecutable;
}

class FixtureFailure extends Error {
  constructor(result) {
    super(`Rust endpoint fixture ${result.phase}: ${result.message}`);
    this.result = result;
  }
}

function probe(server, root, address) {
  const result = spawnSync(server, ['probe', root, address], {
    cwd: packageRoot, encoding: 'utf8', timeout: 10_000,
  });
  assert.equal(result.error, undefined, `probe did not run: ${result.error}`);
  const message = JSON.parse(result.stdout.trim());
  if (message.kind === 'error') throw new FixtureFailure(message);
  assert.equal(result.status, 0, result.stderr);
  assert.equal(message.kind, 'probe');
  return message;
}

function startServer(server, root, address, marker, connections, mode = 'serve') {
  const child = spawn(server, [mode, root, address, String(marker), String(connections)], {
    cwd: packageRoot, stdio: ['pipe', 'pipe', 'pipe'],
  });
  const messages = [];
  let stderr = '';
  let pending = '';
  let readySettled = false;
  let resolveReady;
  let rejectReady;
  const ready = new Promise((resolve, reject) => {
    resolveReady = resolve;
    rejectReady = reject;
  });
  const readyDeadline = setTimeout(() => {
    if (!readySettled) {
      readySettled = true;
      rejectReady(new Error('fixture did not become ready within 10 seconds'));
    }
  }, 10_000);
  readyDeadline.unref();
  child.stderr.on('data', (chunk) => { stderr += chunk; });
  child.stdout.on('data', (chunk) => {
    pending += chunk;
    for (;;) {
      const newline = pending.indexOf('\n');
      if (newline < 0) break;
      const line = pending.slice(0, newline);
      pending = pending.slice(newline + 1);
      try {
        const message = JSON.parse(line);
        messages.push(message);
        if (!readySettled && message.kind === 'ready') {
          readySettled = true;
          clearTimeout(readyDeadline);
          resolveReady(message);
        } else if (!readySettled && message.kind === 'error') {
          readySettled = true;
          clearTimeout(readyDeadline);
          rejectReady(new FixtureFailure(message));
        }
      } catch (error) {
        if (!readySettled) {
          readySettled = true;
          clearTimeout(readyDeadline);
          rejectReady(error);
        }
      }
    }
  });
  child.once('error', (error) => {
    if (!readySettled) {
      readySettled = true;
      clearTimeout(readyDeadline);
      rejectReady(error);
    }
  });
  const closed = new Promise((resolve) => child.once('close', (code, signal) => {
    if (!readySettled) {
      readySettled = true;
      clearTimeout(readyDeadline);
      rejectReady(new Error(`fixture exited before readiness (${code}, ${signal}): ${stderr}`));
    }
    resolve({ code, signal, messages, stderr });
  }));
  return { child, ready, closed };
}

async function stopServer(server) {
  if (server.child.exitCode === null && server.child.signalCode === null) {
    server.child.kill('SIGKILL');
  }
  await server.closed; // Observe actual child exit before removing its own root.
}

async function exchange(connection, marker) {
  const payload = new Uint8Array([0, 1, 127, 128, 255, 43]);
  const header = new Uint8Array(4);
  new DataView(header.buffer).setUint32(0, payload.length, true);
  await connection.write(header);
  await connection.write(payload);
  const responseHeader = await connection.readExactly(4);
  assert.equal(new DataView(responseHeader.buffer, responseHeader.byteOffset, 4).getUint32(0, true), payload.length + 1);
  assert.deepEqual(await connection.readExactly(payload.length + 1), new Uint8Array([marker, ...payload]));
}

async function runWindowsScenario(server) {
  const address = `ipc://node-user-context-${process.pid}-${randomBytes(6).toString('hex')}`;
  const fixture = startServer(server, '-', address, 0x7c, 5);
  const connect = createNodeIpcConnect({});
  let raw;
  try {
    await fixture.ready;
    const connection = await connect(address);
    await exchange(connection, 0x7c);
    await connection.close(); await connection.close();

    const { symbols } = loadBundledC2MemFfiNodeNativeSymbols();
    raw = symbols.c2_mem_ffi_local_endpoint_context_capture(null).value;
    const owned = await raw.connectWindows(address);
    assert.equal(owned.status, 0);
    owned.value.close(); owned.value.close();
    assert.throws(() => owned.value.takeFd(), /closed or transferred/);
    const taken = await raw.connectWindows(address);
    assert.equal(taken.status, 0);
    const fd = taken.value.takeFd();
    assert.ok(fd >= 3);
    closeSync(fd);
    taken.value.close(); taken.value.close();
    assert.throws(() => taken.value.takeFd(), /closed or transferred/);
    assert.throws(() => symbols.c2_mem_ffi_endpoint_context_connect_windows({}, address), /context receiver/);

    const closing = raw.connectWindows(address);
    raw.close(); raw.close();
    assert.equal((await closing).status, 5);
    const disposed = createNodeIpcConnect({});
    const pending = disposed(address);
    disposed.close();
    await assert.rejects(pending, /connector is closed/);
    const result = await fixture.closed;
    assert.equal(result.code, 0, `${JSON.stringify(result.messages)} ${result.stderr}`);
    assert.equal(result.messages.at(-1).cleanup, 'NotApplicable');
  } finally {
    raw?.close(); connect.close(); await stopServer(fixture);
  }
}

async function runScenario(server, container) {
  const ownedRoot = mkdtempSync(resolve(container, 't'));
  const segments = Array(4).fill('目录 with spaces ' + 'x'.repeat(100));
  const rootA = resolve(ownedRoot, 'a', ...segments);
  const rootB = resolve(ownedRoot, 'b', ...segments);
  assert.ok(Buffer.byteLength(rootA, 'utf8') >= 512);
  const address = `ipc://node-context-${process.pid}-${randomBytes(6).toString('hex')}`;
  const otherAddress = `${address}-other`;
  const fixtures = [], owners = [], connections = [];
  const connect = async (factory, name = address) => {
    const connection = await factory(name);
    connections.push(connection);
    return connection;
  };
  try {
    const metadataA = probe(server, rootA, address);
    const metadataB = probe(server, rootB, address);
    const metadataOtherA = probe(server, rootA, otherAddress);
    assert.notEqual(metadataA.endpointName, metadataB.endpointName);
    assert.notEqual(metadataA.namespaceId, metadataB.namespaceId);
    mkdirSync(rootA, { recursive: true, mode: 0o700 }); mkdirSync(rootB, { recursive: true, mode: 0o700 });
    process.env.C2_ENV_FILE = '';
    process.env.C2_IPC_ROOT = rootA;
    const captured = captureLocalIpcEndpointContext();
    owners.push(captured);
    assert.equal(captured.endpointName(address), metadataA.endpointName);
    assert.equal(captured.namespaceId(), metadataA.namespaceId);

    const runtimeA = createBundledC2MemFfiNodeRuntime();
    const failedA = createBundledC2MemFfiNodeRuntime();
    owners.push(runtimeA, failedA);
    assert.equal(runtimeA.resolveEndpoint(address), metadataA.endpointName);
    process.env.C2_IPC_ROOT = rootB;
    assert.equal(runtimeA.resolveEndpoint(address), metadataB.endpointName, 'pure queries do not freeze');
    process.env.C2_IPC_ROOT = rootA;
    // No listener exists yet: this failed actual connect still fixes root A.
    await assert.rejects(() => failedA.connect(address), /connect failed/);
    process.env.C2_IPC_ROOT = rootB;
    assert.equal(failedA.resolveEndpoint(address), metadataA.endpointName);
    assert.equal(captured.endpointName(address), metadataA.endpointName);
    assert.equal(captured.namespaceId(), metadataA.namespaceId);

    // Binds briefly share each root's namespace gate. Wait for readiness at
    // startup, then exercise the independently live listeners below.
    const fixtureA = startServer(server, rootA, address, 0xa1, 7);
    fixtures.push(fixtureA);
    const readyA = await fixtureA.ready;
    const fixtureOtherA = startServer(server, rootA, otherAddress, 0xa2, 3);
    fixtures.push(fixtureOtherA);
    const readyOtherA = await fixtureOtherA.ready;
    const fixtureB = startServer(server, rootB, address, 0xb2, 2);
    fixtures.push(fixtureB);
    const readyB = await fixtureB.ready;
    assert.deepEqual([readyA, readyOtherA, readyB], [
      { ...metadataA, kind: 'ready' },
      { ...metadataOtherA, kind: 'ready' },
      { ...metadataB, kind: 'ready' },
    ]);

    const typed = createNodeIpcConnect({ endpointContext: captured });
    const explicit = createNodeIpcConnect({ ipcRoot: rootA });
    owners.push(typed, explicit);
    const fromSnapshot = await connect(typed);
    const typedOther = await connect(typed, otherAddress);
    await exchange(fromSnapshot, 0xa1);
    await fromSnapshot.close(); await fromSnapshot.close();
    assert.equal(captured.endpointName(address), metadataA.endpointName, 'socket close does not consume context');
    captured.close(); captured.close();
    await exchange(typedOther, 0xa2);
    const typedRetry = await connect(typed);
    await exchange(typedRetry, 0xa1);
    typed.close(); typed.close();
    // Factory disposal only closes the name snapshot, never these streams.
    await exchange(typedOther, 0xa2);
    await typedOther.close(); await typedRetry.close();

    const fromRoot = await connect(explicit);
    await exchange(fromRoot, 0xa1); await fromRoot.close();

    process.env.C2_IPC_ROOT = rootA;
    const firstA = await connect(runtimeA.connect);
    process.env.C2_IPC_ROOT = rootB;
    assert.equal(runtimeA.resolveEndpoint(address), metadataA.endpointName);
    assert.equal(runtimeA.resolveEndpoint(otherAddress), metadataOtherA.endpointName);
    const secondA = await connect(runtimeA.connect, otherAddress);
    await exchange(firstA, 0xa1); await firstA.close();
    await exchange(secondA, 0xa2);
    const reconnectedA = await connect(runtimeA.connect);
    await exchange(reconnectedA, 0xa1); await reconnectedA.close();
    runtimeA.close(); runtimeA.close();
    await exchange(secondA, 0xa2); await secondA.close();

    const afterFailure = await connect(failedA.connect);
    await exchange(afterFailure, 0xa1); await afterFailure.close();
    assert.equal(failedA.resolveEndpoint(address), metadataA.endpointName);
    const retryFailure = await connect(failedA.connect);
    const failureOther = await connect(failedA.connect, otherAddress);
    await exchange(retryFailure, 0xa1); await exchange(failureOther, 0xa2);
    await retryFailure.close(); await failureOther.close();

    const runtimeB = createBundledC2MemFfiNodeRuntime();
    const explicitB = createBundledC2MemFfiNodeRuntime({ ipcRoot: rootB });
    owners.push(runtimeB, explicitB);
    assert.equal(runtimeB.resolveEndpoint(address), metadataB.endpointName);
    const fromB = await connect(runtimeB.connect);
    process.env.C2_IPC_ROOT = rootA;
    assert.equal(runtimeB.resolveEndpoint(address), metadataB.endpointName);
    const fromExplicitB = await connect(explicitB.connect);
    await exchange(fromB, 0xb2); await exchange(fromExplicitB, 0xb2);
    await fromB.close(); await fromExplicitB.close();

    // Physical connections have their own ownership boundary, independent of
    // the context used to start them. Closing during async work must stay safe.
    const fdAddress = `${address}-fd-owner`;
    const fdFixture = startServer(server, rootA, fdAddress, 0xff, 3);
    fixtures.push(fdFixture);
    await fdFixture.ready;
    const { symbols } = loadBundledC2MemFfiNodeNativeSymbols();
    const raw = symbols.c2_mem_ffi_local_endpoint_context_capture(rootA).value;
    const owned = await raw.connectUnix(fdAddress);
    assert.equal(owned.status, 0);
    owned.value.close(); owned.value.close();
    assert.throws(() => owned.value.takeFd(), /closed or transferred/);
    const closing = raw.connectUnix(fdAddress);
    raw.close(); raw.close();
    assert.equal((await closing).status, 5, 'closed raw context must discard the socket and retain native context until work ends');
    const disposed = createNodeIpcConnect({ ipcRoot: rootA });
    const pending = disposed(fdAddress);
    disposed.close();
    await assert.rejects(pending, /connector is closed/);

    for (const fixture of fixtures) {
      const result = await fixture.closed;
      assert.equal(result.code, 0, `${JSON.stringify(result.messages)} ${result.stderr}`);
      assert.equal(result.signal, null);
      assert.equal(result.messages.at(-1).cleanup, 'Reaped');
    }
    return { kind: 'passed', connections: 15, listeners: 4,
      evidence: 'Real Node→c2-local streams: success-first and failure-first Runtime A survive env flip to B; new Runtime B, explicit roots, shared typed context, multiple addresses/connections, reconnect and factory disposal preserve stream bytes. No RPC/SHM/matrix assertion.' };
  } finally {
    for (const connection of connections) await connection.close();
    for (const owner of owners) owner.close();
    for (const fixture of fixtures) await stopServer(fixture);
    rmSync(ownedRoot, { recursive: true, force: true });
  }
}

if (process.argv[2] === '--endpoint-context-child') {
  try {
    console.log(JSON.stringify(await runScenario(process.argv[3], process.argv[4])));
  } catch (error) {
    console.log(JSON.stringify({ kind: 'error', message: String(error),
      phase: error.result?.phase, rawOsError: error.result?.rawOsError,
      fixtureMessage: error.result?.message }));
    process.exitCode = 1;
  }
} else {
  test('Native endpoint contexts retain verified streams and close pending connections', {
    timeout: 120_000,
  }, async (t) => {
    if (process.platform === 'win32') {
      await runWindowsScenario(buildServer());
      return;
    }
    const explicitContainer = process.env.C2_ENDPOINT_CONTEXT_TEST_ROOT;
    const server = buildServer();
    const container = explicitContainer ?? resolve(packageRoot, 'target', 'node-transport-tests');
    if (explicitContainer !== undefined) {
      assert.equal(isAbsolute(container), true, 'C2_ENDPOINT_CONTEXT_TEST_ROOT must be an absolute pre-created Unix container');
      assert.equal(statSync(container).isDirectory(), true, 'Host must pre-create C2_ENDPOINT_CONTEXT_TEST_ROOT');
    } else mkdirSync(container, { recursive: true });
    // All process-env changes and sockets live in one child, isolated from the suite.
    const output = spawnSync(process.execPath, [fileURLToPath(import.meta.url), '--endpoint-context-child', server, container], {
      cwd: packageRoot, encoding: 'utf8', timeout: 110_000,
    });
    assert.equal(output.error, undefined, `transport child did not finish: ${output.error}`);
    const result = JSON.parse(output.stdout.trim());
    const deniedBind = result.phase === 'bind' && [1, 13].includes(result.rawOsError);
    if (explicitContainer === undefined && deniedBind) {
      t.skip(`Real transport not run: ${result.message}. Host gate: ${hostCommand}`);
      return;
    }
    assert.equal(output.status, 0, `${output.stdout} ${output.stderr}`);
    assert.equal(result.kind, 'passed');
    t.diagnostic(result.evidence);
  });
  for (const ending of ['EOF', 'explicit close']) {
    test(`native Node IPC pauses idle input and preserves read progress through ${ending}`, { timeout: 10000 }, async () => {
      const root = process.platform === 'win32' ? undefined : mkdtempSync('/tmp/c2-node-');
      const connect = createNodeIpcConnect({ ipcRoot: root });
      const address = `ipc://node-buffer-${process.pid}-${randomBytes(6).toString('hex')}`;
      const socketPath = connect.resolveEndpoint(address);
      const fixture = startServer(buildServer(), root ?? '-', address, ending === 'EOF' ? 0 : 1, 1, 'buffering');
      let connection;
      const sendNext = () => fixture.child.stdin.write('x');
      try {
        await fixture.ready;
        connection = await connect(address);
        await connection.write(new Uint8Array([9, 8]));
        const payload = new Uint8Array(8 * 1024 * 1024).fill(0x5a);
        sendNext();
        assert.equal(connection.socket.isPaused(), true, 'idle input must apply native backpressure');
        assert.equal(connection.bufferedBytes, 0, 'idle input must remain in Node, not an unbounded private queue');
        const [large] = await Promise.all([connection.readExactly(payload.length), connection.write(payload)]);
        assert.equal(large.length, payload.length);
        assert.ok(large.every((value) => value === 0x5a), 'reads larger than the native high-water mark must progress');
        assert.equal(connection.bufferedBytes, 0);
        sendNext();
        assert.equal(connection.socket.isPaused(), true, 'finishing a read must pause native input again');
        assert.equal(connection.bufferedBytes, 0, 'finishing a read must restore idle backpressure');
        assert.deepEqual(Array.from(await connection.readExactly(2)), [8, 9]);
        sendNext();
        const first = connection.readExactly(3);
        sendNext();
        assert.deepEqual(Array.from(await first), [1, 2, 3]);
        assert.deepEqual(Array.from(await connection.readExactly(1)), [4]);
        const concurrent = [connection.readExactly(1), connection.readExactly(2)];
        sendNext();
        assert.deepEqual((await Promise.all(concurrent)).map((bytes) => Array.from(bytes)), [[5], [6, 7]]);
        sendNext();
        assert.deepEqual(Array.from(await connection.readExactly(1)), [1]);
        const incomplete = connection.readExactly(3);
        if (ending === 'EOF') sendNext();
        else connection.close();
        await assert.rejects(incomplete, /ended|closed/);
        if (ending === 'EOF') assert.deepEqual(Array.from(await connection.readExactly(2)), [2, 3]);
        await assert.rejects(connection.readExactly(1), /ended|closed/);
        const result = await fixture.closed;
        assert.equal(result.code, 0, result.stderr);
        assert.equal(result.messages.at(-1).cleanup, process.platform === 'win32' ? 'NotApplicable' : 'Reaped');
      } finally {
        connection?.close(); connect.close(); await stopServer(fixture);
        if (root !== undefined) {
          assert.equal(existsSync(socketPath), false, 'closed server must unlink its socket');
          assert.equal(readdirSync(root).some((name) => name.endsWith('.lease')), false);
          rmSync(root, { recursive: true });
        }
      }
    });
  }

}
