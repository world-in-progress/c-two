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
  const memory = {gc() {events.push('gc'); return 5;}, peerEnd() {events.push('peer_end');}, deactivate() {events.push('deactivate');}, close() {events.push('close');}};
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
    view.setUint32(0, 22, true); view.setUint32(12, 16, true);
    release[16] = 0x17; release[17] = 1; view.setBigUint64(18, 17n, true);
    const response = new Uint8Array(18), responseView = new DataView(response.buffer);
    responseView.setUint32(0, 14, true); responseView.setBigUint64(4, 1n, true);
    responseView.setUint32(12, 2 | 256, true); response.set([0, 42], 16);
    socket.emit('data', new Uint8Array([...release, ...response]));
    if (ending === 'end') socket.emit('end');
    else socket.emit('error', Object.assign(new Error('reset'), {code: 'ECONNRESET', syscall: 'read'}));
    const header = await connection.readExactly(16), payload = await connection.readExactly(10);
    connection.memory.receiveRelease(new Uint8Array([...header, ...payload]));
    await connection.readExactly(16);
    assert.deepEqual(Array.from(await connection.readExactly(2)), [0, 42]);
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
