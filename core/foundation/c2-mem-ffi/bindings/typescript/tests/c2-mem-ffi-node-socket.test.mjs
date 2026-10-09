import assert from 'node:assert/strict';
import { EventEmitter } from 'node:events';
import test from 'node:test';

import { createNodeIpcConnect } from '../dist/index.js';

async function connectionWith(write) {
  const socket = new EventEmitter();
  socket.destroyed = false;
  socket.write = (bytes, callback) => write(socket, bytes, callback);
  socket.end = () => socket;
  socket.destroy = () => { socket.destroyed = true; return socket; };
  const connector = createNodeIpcConnect({
    resolveEndpoint: () => process.platform === 'win32' ? '\\\\.\\pipe\\c2-test-socket' : '/tmp/c2-test-socket',
    createConnection: () => {
      queueMicrotask(() => socket.emit('connect'));
      return socket;
    },
  });
  return { connection: await connector('ipc://socket-test'), connector, socket };
}

test('write callback failure is reported without an error event', async () => {
  const { connection, connector, socket } = await connectionWith((_socket, _bytes, callback) => {
    queueMicrotask(() => callback(new Error('callback-only failure')));
    return false;
  });
  try {
    await assert.rejects(connection.write(new Uint8Array([1, 2])), /callback-only failure/);
    assert.equal(socket.listenerCount('close'), 1, 'only the receiver lifetime listener remains');
  } finally {
    await connection.close();
    connector.close();
  }
});

test('successful write callback accepts null error', async () => {
  const { connection, connector } = await connectionWith((_socket, _bytes, callback) => {
    queueMicrotask(() => callback(null));
    return true;
  });
  try {
    await connection.write(new Uint8Array([1, 2]));
  } finally {
    await connection.close();
    connector.close();
  }
});

test('socket close before write callback rejects the pending write', async () => {
  const { connection, connector } = await connectionWith((socket) => {
    queueMicrotask(() => socket.emit('close'));
    return false;
  });
  try {
    await assert.rejects(connection.write(new Uint8Array([1, 2])), /closed/);
  } finally {
    await connection.close();
    connector.close();
  }
});
