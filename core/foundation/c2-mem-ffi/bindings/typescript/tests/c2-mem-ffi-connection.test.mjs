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
  const memory = {peerEnd() {events.push('peer_end');}, deactivate() {events.push('deactivate');}, close() {events.push('close');}};
  return {socket, events, connection: new NodeIpcConnection(socket, memory)};
}
test('Node connection close is idempotent and does not manufacture peer end', () => {
  const {socket, events, connection} = fixture();
  connection.close(); connection.close();
  assert.deepEqual(events, ['deactivate', 'close']); assert.equal(socket.ends, 0, 'a retained native descriptor must not be half-closed');
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
