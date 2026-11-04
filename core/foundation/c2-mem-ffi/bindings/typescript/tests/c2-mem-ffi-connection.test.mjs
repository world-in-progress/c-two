import assert from 'node:assert/strict';
import {EventEmitter} from 'node:events';
import {mkdtempSync, readFileSync, rmSync, writeFileSync} from 'node:fs';
import {tmpdir} from 'node:os';
import {join} from 'node:path';
import {pathToFileURL} from 'node:url';
import test from 'node:test';
import ts from 'typescript';

// Exercise the shipped class without adding an application export or bypassing
// native verification. Real verified sockets/owners are covered in protocol.test.
const directory = mkdtempSync(join(tmpdir(), 'c2-node-connection-'));
let NodeIpcConnection;
try {
  const source = readFileSync(new URL('../src/index.ts', import.meta.url), 'utf8');
  writeFileSync(join(directory, 'connection.mjs'), ts.transpileModule(`${source}\nexport {NodeIpcConnection};`, {
    compilerOptions: {target: ts.ScriptTarget.ES2022, module: ts.ModuleKind.ES2022},
  }).outputText);
  ({NodeIpcConnection} = await import(pathToFileURL(join(directory, 'connection.mjs'))));
} finally { rmSync(directory, {recursive: true, force: true}); }
class Socket extends EventEmitter {
  destroyed = false;
  ends = 0;
  pause() {} resume() {}
  write(_bytes, callback) { callback?.(this.writeError); }
  end() { this.ends++; this.emit('end'); }
  destroy() { this.destroyed = true; this.emit('close'); }
}
function fixture() {
  const socket = new Socket(), events = [];
  const memory = {writeTimeoutMillis: 30_000, gc() {events.push('gc'); return 5;}, peerEnd() {events.push('peer_end');}, deactivate() {events.push('deactivate');}, close() {events.push('close');}};
  return {socket, events, connection: new NodeIpcConnection(socket, memory)};
}
test('Node performs idle memory GC and stops its timer on close', async () => {
  const {events, connection} = fixture();
  const deadline = Date.now() + 500;
  while (events.filter(event => event === 'gc').length < 2 && Date.now() < deadline) {
    await new Promise(resolve => setTimeout(resolve, 5));
  }
  assert.ok(events.filter(event => event === 'gc').length >= 2, 'idle connection must maintain memory');
  connection.close();
  const count = events.filter(event => event === 'gc').length;
  await new Promise(resolve => setTimeout(resolve, 23));
  assert.equal(events.filter(event => event === 'gc').length, count);
});
for (const ending of ['end', 'read reset']) {
  test(`Node ${ending} processes buffered release before peer end`, {skip: process.platform === 'win32' && ending === 'read reset'}, async () => {
    const {socket, connection} = fixture();
    const events = [], loans = new Set([17n]);
    connection.memory.receiveRelease = bytes => {
      const loan = new DataView(bytes.buffer, bytes.byteOffset + 18, 8).getBigUint64(0, true);
      assert.ok(loans.delete(loan), 'release must find its outstanding loan');
      events.push('release');
    };
    connection.memory.peerEnd = () => { loans.clear(); events.push('peer_end'); };
    const release = new Uint8Array(26), view = new DataView(release.buffer);
    view.setUint32(0, 22, true); view.setUint32(12, 0x1000, true);
    release[16] = 7; release[17] = 1; view.setBigUint64(18, 17n, true);
    const response = new Uint8Array(19), responseView = new DataView(response.buffer);
    responseView.setUint32(0, 15, true); responseView.setBigUint64(4, 1n, true);
    responseView.setUint32(12, 0x1002, true); response.set([6, 0, 42], 16);
    socket.emit('data', new Uint8Array([...release, ...response]));
    if (ending === 'end') socket.emit('end');
    else socket.emit('error', Object.assign(new Error('reset'), {code: 'ECONNRESET', syscall: 'read'}));
    const header = await connection.readExactly(16), payload = await connection.readExactly(10);
    connection.memory.receiveRelease(new Uint8Array([...header, ...payload]));
    await connection.readExactly(16);
    assert.deepEqual(Array.from(await connection.readExactly(3)), [6, 0, 42]);
    events.push('response');
    await new Promise(resolve => setImmediate(resolve));
    assert.deepEqual(events, ['release', 'response', 'peer_end']);
    connection.close();
  });
}
test('Node connection close is idempotent and does not manufacture peer end', () => {
  const {socket, events, connection} = fixture();
  connection.close(); connection.close();
  assert.deepEqual(events, ['gc', 'deactivate', 'close']); assert.equal(socket.ends, 0, 'a retained native descriptor must not be half-closed');
});
test('Node connection rejects reads and writes after close', async () => {
  const {connection} = fixture(); connection.close();
  await assert.rejects(() => connection.readExactly(1), /closed/);
  await assert.rejects(() => connection.write(new Uint8Array([1])), /closed/);
});
test('Node write timeout deactivates admission while the current write and reads continue', async () => {
  const {socket, connection, events} = fixture();
  connection.memory.writeTimeoutMillis = 5;
  let complete;
  socket.write = (_bytes, callback) => { complete = callback; return true; };
  const writing = connection.write(new Uint8Array([1]));
  await new Promise(resolve => setTimeout(resolve, 15));
  assert.equal(connection.deactivated, true);
  assert.equal(events.filter(event => event === 'deactivate').length, 1, 'native loan admission is sealed while the write is still blocked');
  socket.emit('data', new Uint8Array([9]));
  assert.deepEqual(Array.from(await connection.readExactly(1)), [9]);
  let finished = false;
  writing.then(() => { finished = true; });
  await Promise.resolve();
  assert.equal(finished, false);
  complete();
  await writing;
  socket.write = (_bytes, callback) => { callback?.(); return true; };
  await connection.write(new Uint8Array([2]));
  connection.close();
});
for (const requested of [3, 514]) {
test(`Node local destination allocation failure consumes exactly ${requested} bytes and retains metadata`, async () => {
  const {socket, connection} = fixture();
  const input = Uint8Array.from({length: requested + 2}, (_, index) => index % 251);
  const split = Math.floor(requested / 2);
  socket.emit('data', input.subarray(0, split));
  socket.emit('data', input.subarray(split));
  const Original = globalThis.Uint8Array;
  globalThis.Uint8Array = class extends Original {
    constructor(...args) { if (args[0] === requested) throw new Error('allocation denied'); super(...args); }
  };
  try {
    await assert.rejects(() => connection.readExactly(requested), error => {
      assert.equal(error.c2LocalAllocationFailure, true);
      assert.match(error.message, /allocation denied/);
      assert.deepEqual(Array.from(error.c2ReadPrefix), Array.from(input.subarray(0, requested)));
      return true;
    });
  } finally { globalThis.Uint8Array = Original; }
  assert.deepEqual(Array.from(await connection.readExactly(2)), Array.from(input.subarray(requested)));
  connection.close();
});
}
for (const syscall of ['write', undefined, 'read']) {
  test(`ECONNRESET is peer end only with a read syscall: ${syscall ?? 'unknown'}`, {skip: process.platform === 'win32'}, async () => {
    const {socket, events, connection} = fixture();
    const error = Object.assign(new Error('reset'), {code: 'ECONNRESET', syscall});
    socket.writeError = error;
    if (syscall === 'write') await assert.rejects(() => connection.write(new Uint8Array([1])), /reset/);
    socket.emit('error', error);
    await assert.rejects(() => connection.readExactly(1), /reset/);
    assert.equal(events.filter(event => event === 'peer_end').length, syscall === 'read' ? 1 : 0);
    connection.close();
  });
}
