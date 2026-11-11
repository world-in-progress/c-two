import assert from 'node:assert/strict';
import {spawn} from 'node:child_process';
import {mkdirSync, mkdtempSync, readFileSync, rmSync, writeFileSync} from 'node:fs';
import {createRequire} from 'node:module';
import {tmpdir} from 'node:os';
import {dirname, resolve} from 'node:path';
import {createInterface} from 'node:readline';
import {fileURLToPath, pathToFileURL} from 'node:url';
import test from 'node:test';
import {createNodeIpcConnector} from '../dist/index.js';
import {buildProtocolPeer} from '../scripts/cargo-tools.mjs';

const packageRoot = resolve(dirname(fileURLToPath(import.meta.url)), '..');
let executable;

async function withPeer(run) {
  executable ??= buildProtocolPeer({timeout: 120_000});
  const directory = mkdtempSync(resolve(tmpdir(), 'c2-boundary-'));
  const root = resolve(directory, 'ipc');
  mkdirSync(root, {mode: 0o700});
  const address = `ipc://node-boundary-${process.pid}-${Math.random().toString(16).slice(2)}`;
  const child = spawn(executable, [process.platform === 'win32' ? '-' : root, address]);
  const exited = new Promise(resolve => child.once('exit', code => resolve(code)));
  const lines = createInterface({input: child.stdout})[Symbol.asyncIterator]();
  let stderr = '';
  child.stderr.on('data', bytes => { stderr += bytes; });
  const connector = createNodeIpcConnector(process.platform === 'win32' ? {} : {ipcRoot: root});
  let killed = false;
  try {
    const ready = JSON.parse((await lines.next()).value);
    assert.equal(ready.kind, 'ready', stderr);
    const ts = createRequire(import.meta.url)('typescript');
    const source = readFileSync(resolve(packageRoot, '../../../c2-codegen/assets/typescript_transport.ts'), 'utf8');
    const modulePath = resolve(directory, 'transport.mjs');
    writeFileSync(modulePath, ts.transpileModule(source, {compilerOptions: {
      target: ts.ScriptTarget.ES2022, module: ts.ModuleKind.ES2022,
    }}).outputText);
    const transportModule = await import(pathToFileURL(modulePath));
    await run({connector, address, contract: ready.contract, lines, child, ...transportModule,
      async killPeer() {
        killed = true;
        // ChildProcess.kill also terminates the real process on Windows; no POSIX-only suspension.
        assert.equal(child.kill(), true);
        await exited;
      },
    });
  } finally {
    connector.close();
    if (!killed) child.stdin.end('quit\n');
    const timer = setTimeout(() => child.kill(), 12_000);
    const code = await exited;
    clearTimeout(timer);
    rmSync(directory, {recursive: true, force: true});
    if (!killed) assert.equal(code, 0, stderr);
  }
}

function input(mode, count, length) {
  const bytes = new Uint8Array(6);
  bytes[0] = mode;
  bytes[1] = count;
  new DataView(bytes.buffer).setUint32(2, length, true);
  return bytes;
}

test('full B reports the stream deadline and CANCEL completes before either hold is released', {timeout: 150_000}, async () => {
  // Requirement 3 / design section 9: holds fill B; deadline is truthful and control does not wait for B.
  const previous = process.env.C2_CALL_PAYLOAD_BUFFER_BUDGET_BYTES;
  process.env.C2_CALL_PAYLOAD_BUFFER_BUDGET_BYTES = '8192';
  try {
    await withPeer(async ({connector, address, contract, lines, createIpcEncodedTransport, C2IpcTransportError}) => {
      const transport = createIpcEncodedTransport(address, {connect: connector, callTimeoutMs: 2000});
      const holds = [];
      let stream;
      try {
        await transport.prepare('echo', contract);
        stream = transport.stream('echo', contract, 'echo', () => input(6, 3, 4096), bytes => bytes, true);
        for (let index = 0; index < 2; index++) {
          const item = await stream.next();
          assert.equal(item.done, false);
          holds.push(item.value);
          assert.ok(item.value.value.every(byte => byte === index));
          assert.deepEqual(JSON.parse((await lines.next()).value), {kind: 'item_step', index});
        }
        assert.equal(connector.stats().payload_buffer.used_bytes, '8192');
        let settled = false;
        const pending = stream.next().finally(() => { settled = true; });
        const expired = assert.rejects(pending, error => {
          assert.ok(error instanceof C2IpcTransportError);
          assert.equal(error.code, 715);
          assert.equal(error.canonicalName, 'CallDeadlineExceeded');
          assert.equal(error.transportPhase, 'dispatch_uncertain');
          assert.equal(error.retrySafe, false);
          return true;
        });
        assert.deepEqual(JSON.parse((await lines.next()).value), {kind: 'item_step', index: 2});
        assert.equal(settled, false, 'the next item waits with B full');
        assert.equal(connector.stats().payload_buffer.used_bytes, '8192');
        await expired;
        const receipt = await stream.close(null);
        assert.equal(receipt.completed, true, 'CANCEL and CALL_END pass while both holds still fill B');
        assert.equal(receipt.error.code, 718);
        assert.equal(receipt.error.canonicalName, 'CallCancelled');
        assert.equal(receipt.completeItems, 2n);
        assert.equal(connector.stats().used_operations, '0');
        assert.equal(connector.stats().payload_buffer.used_bytes, '8192');
        assert.equal(connector.stats().payload_buffer.rejected_allocations, '0');
        for (let index = 0; index < holds.length; index++) {
          assert.ok(holds[index].value.every(byte => byte === index));
          holds[index].release();
        }
        assert.equal(connector.stats().payload_buffer.used_bytes, '0');
      } finally {
        if (stream !== undefined) await stream.close(0);
        for (const held of holds) held.release();
        await transport.close();
      }
    });
  } finally {
    if (previous === undefined) delete process.env.C2_CALL_PAYLOAD_BUFFER_BUDGET_BYTES;
    else process.env.C2_CALL_PAYLOAD_BUFFER_BUDGET_BYTES = previous;
  }
});

test('real peer death keeps delivered holds and a known terminal while the open stream becomes uncertain once', {timeout: 150_000}, async () => {
  // Requirement 3 / handshake and design section 8: EOF preserves delivered items and known terminals.
  await withPeer(async ({connector, address, contract, lines, child, killPeer, createIpcEncodedTransport, C2IpcTransportError}) => {
    const endpoint = await connector(address);
    await endpoint.prepare('echo', contract);
    const transport = createIpcEncodedTransport(address, {connect: async () => endpoint});
    const holds = [];
    let completed, completedItem, interrupted;
    try {
      // Keep this native facade alive: re-observe Rust's original receipt after EOF,
      // rather than checking only the public stream's cached close result.
      completed = endpoint.beginCall('echo', 'echo', true, null);
      const request = input(0, 1, 4096);
      await completed.chargeInput(request.byteLength);
      await completed.dispatch(request);
      completedItem = await completed.next();
      assert.ok(completedItem);
      assert.equal(await completed.next(), null);
      const known = await completed.observe(null);
      assert.deepEqual(known, {completed: true, error: null, completeItems: 1n});
      child.stdin.write('stats\n');
      const before = JSON.parse((await lines.next()).value);
      assert.equal(before.stream_dispatches, 1);
      interrupted = transport.stream('echo', contract, 'echo', () => input(2, 3, 4096), bytes => bytes, true);
      holds.push((await interrupted.next()).value);
      assert.equal(JSON.parse((await lines.next()).value).kind, 'step_blocked');
      child.stdin.write('stats\n');
      const running = JSON.parse((await lines.next()).value);
      assert.equal(running.stream_dispatches - before.stream_dispatches, 1, 'one resource entry for the interrupted stream');
      const lost = assert.rejects(interrupted.next(), error => {
        assert.ok(error instanceof C2IpcTransportError);
        assert.equal(error.transportPhase, 'dispatch_uncertain');
        assert.equal(error.retrySafe, false);
        return true;
      });
      await killPeer();
      await lost;
      const receipt = await interrupted.close(null);
      assert.equal(receipt.completed, true);
      assert.equal(receipt.completeItems, null, 'no CALL_END item count was received');
      assert.ok(receipt.error instanceof C2IpcTransportError);
      assert.equal(receipt.error.transportPhase, 'dispatch_uncertain');
      assert.equal(receipt.error.retrySafe, false);
      assert.deepEqual(await completed.observe(null), known, 'EOF cannot replace a known native terminal');
      assert.equal(completedItem.byteLength, 4096);
      assert.ok(completedItem.every(byte => byte === 0));
      completedItem.release();
      for (const held of holds) {
        assert.equal(held.value.byteLength, 4096);
        assert.ok(held.value.every(byte => byte === 0));
        held.release();
      }
      assert.equal(connector.stats().payload_buffer.used_bytes, '0');
      assert.equal(connector.stats().used_operations, '0');
    } finally {
      if (interrupted !== undefined) await interrupted.close(0);
      if (completed !== undefined) completed.close();
      if (completedItem !== undefined) completedItem.release();
      for (const held of holds) held.release();
      await transport.close();
    }
  });
});
