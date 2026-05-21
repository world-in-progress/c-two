import assert from 'node:assert/strict';
import { tmpdir } from 'node:os';
import { join } from 'node:path';
import { createServer } from 'node:net';
import { unlinkSync } from 'node:fs';
import test from 'node:test';

import {
  C2_MEM_FFI_ABI_VERSION,
  C2_MEM_FFI_MAX_IPC_SHM_SEGMENTS,
  C2_MEM_FFI_MAX_SHM_PREFIX_BYTES,
  C2MemFfiBindingError,
  createC2MemFfiRequestPoolFromSymbols,
  createC2MemFfiNativeBuddyRequestBackend,
  createC2MemFfiNativeBuddyResponseBackend,
  createC2MemFfiResponsePoolFromSymbols,
  createNodeIpcConnect,
} from '../dist/index.js';

function abiVersion() {
  return C2_MEM_FFI_ABI_VERSION;
}

async function closeServer(server) {
  await new Promise((resolve, reject) => {
    server.close((error) => {
      if (error === undefined) {
        resolve();
      } else {
        reject(error);
      }
    });
  });
}

function waitFor(condition, label) {
  const deadline = Date.now() + 1000;
  return new Promise((resolve, reject) => {
    const tick = () => {
      if (condition()) {
        resolve();
        return;
      }
      if (Date.now() > deadline) {
        reject(new Error(`timed out waiting for ${label}`));
        return;
      }
      setTimeout(tick, 5);
    };
    tick();
  });
}

test('createNodeIpcConnect adapts a Node Unix socket to C2IpcConnection', async (t) => {
  if (process.platform === 'win32') {
    t.skip('Node IPC smoke uses Unix-domain sockets');
    return;
  }
  const socketPath = join(tmpdir(), `c2-node-ipc-${process.pid}-${Date.now()}.sock`);
  try {
    unlinkSync(socketPath);
  } catch {
  }
  const received = [];
  const server = createServer((socket) => {
    socket.on('data', (chunk) => {
      received.push(...chunk);
    });
    socket.write(new Uint8Array([1, 2]));
    setTimeout(() => socket.write(new Uint8Array([3, 4])), 5);
  });
  t.after(async () => {
    await closeServer(server);
    try {
      unlinkSync(socketPath);
    } catch {
    }
  });
  await new Promise((resolve, reject) => {
    server.once('error', reject);
    server.listen(socketPath, resolve);
  });

  const connect = createNodeIpcConnect();
  const connection = await connect(socketPath);
  try {
    const first = await connection.readExactly(3);
    assert.deepEqual(Array.from(first), [1, 2, 3]);
    await connection.write(new Uint8Array([9, 8]));
    await waitFor(() => received.length >= 2, 'server data');
    assert.deepEqual(received.slice(0, 2), [9, 8]);
    const second = await connection.readExactly(1);
    assert.deepEqual(Array.from(second), [4]);
  } finally {
    await connection.close?.();
  }
});

test('createNodeIpcConnect rejects reads after explicit close even with buffered bytes', async (t) => {
  if (process.platform === 'win32') {
    t.skip('Node IPC smoke uses Unix-domain sockets');
    return;
  }
  const socketPath = join(tmpdir(), `c2-node-ipc-close-${process.pid}-${Date.now()}.sock`);
  try {
    unlinkSync(socketPath);
  } catch {
  }
  const server = createServer((socket) => {
    socket.write(new Uint8Array([1, 2, 3]));
  });
  t.after(async () => {
    await closeServer(server);
    try {
      unlinkSync(socketPath);
    } catch {
    }
  });
  await new Promise((resolve, reject) => {
    server.once('error', reject);
    server.listen(socketPath, resolve);
  });

  const connection = await createNodeIpcConnect()(socketPath);
  assert.deepEqual(Array.from(await connection.readExactly(1)), [1]);
  await connection.close?.();
  await assert.rejects(
    () => connection.readExactly(1),
    /closed/,
  );
});

test('createNodeIpcConnect rejects sockets that close before connect', async () => {
  const fakeSocket = {
    destroyed: false,
    write() {
      return true;
    },
    end() {
      return this;
    },
    destroy() {
      this.destroyed = true;
      return this;
    },
    on() {
      return this;
    },
    once(event, listener) {
      if (event === 'close') {
        setTimeout(listener, 0);
      }
      return this;
    },
    off() {
      return this;
    },
  };

  await assert.rejects(
    () => createNodeIpcConnect({ createConnection: () => fakeSocket })('/tmp/c2-closed-before-connect.sock'),
    /closed before connect completed/,
  );
});

test('c2-mem-ffi symbol adapters validate ABI version before creating pools', async () => {
  assert.equal(C2_MEM_FFI_ABI_VERSION, 1);
  let createCount = 0;
  const symbols = {
    c2_mem_ffi_abi_version() {
      return 2;
    },
    c2_mem_ffi_request_pool_new() {
      createCount += 1;
      return { status: 0, value: { kind: 'request' } };
    },
    c2_mem_ffi_request_pool_destroy() {},
    c2_mem_ffi_request_pool_prefix() {
      return { status: 0, value: '/cc2n0000' };
    },
    c2_mem_ffi_request_pool_segment_count() {
      return { status: 0, value: 1 };
    },
    c2_mem_ffi_request_pool_segment_name() {
      return { status: 0, value: '/cc2n0000_b0000' };
    },
    c2_mem_ffi_request_pool_segment_data_size() {
      return { status: 0, value: 4096 };
    },
    c2_mem_ffi_request_pool_write() {
      return { status: 0, value: { segmentIndex: 0, offset: 0, byteLength: 1, dedicated: false } };
    },
    c2_mem_ffi_request_pool_release() {
      return { status: 0 };
    },
    c2_mem_ffi_request_pool_forget_consumed() {
      return { status: 0 };
    },
  };

  await assert.rejects(
    () => createC2MemFfiRequestPoolFromSymbols(symbols, {
      prefix: '/cc2n0000',
      segmentSize: 4096,
      maxSegments: 1,
      minBlockSize: 512,
    }),
    /ABI version 2/,
  );
  assert.equal(createCount, 0);
});

test('c2-mem-ffi native buddy response backend delegates read and release', async () => {
  const calls = [];
  const backend = createC2MemFfiNativeBuddyResponseBackend({
    read(block, destination) {
      calls.push(['read', block, destination.byteLength]);
      destination.set([7, 8, 9]);
    },
    release(block) {
      calls.push(['release', block]);
    },
  });
  const block = {
    prefix: '/cc2s0001',
    segments: [{ name: '/cc2s0001_b0000', size: 4096 }],
    segmentIndex: 0,
    segmentName: '/cc2s0001_b0000',
    offset: 128,
    byteLength: 3,
    dedicated: false,
  };
  const destination = new Uint8Array(3);

  await backend.readResponse(block, destination);
  await backend.releaseResponse(block);

  assert.deepEqual(Array.from(destination), [7, 8, 9]);
  assert.equal(calls.length, 2);
  assert.equal(calls[0][0], 'read');
  assert.equal(calls[1][0], 'release');
});

test('c2-mem-ffi native buddy response backend rejects dedicated blocks before pool calls', async () => {
  let readCount = 0;
  const backend = createC2MemFfiNativeBuddyResponseBackend({
    read() {
      readCount += 1;
    },
    release() {},
  });

  await assert.rejects(
    () => backend.readResponse({
      prefix: '/cc2s0001',
      segments: [],
      segmentIndex: 256,
      segmentName: '/cc2s0001_d0100',
      offset: 0,
      byteLength: 3,
      dedicated: true,
    }, new Uint8Array(3)),
    C2MemFfiBindingError,
  );
  assert.equal(readCount, 0);
});

test('c2-mem-ffi native buddy response backend rejects blocks outside advertised segments before pool calls', async () => {
  let readCount = 0;
  const backend = createC2MemFfiNativeBuddyResponseBackend({
    read() {
      readCount += 1;
    },
    release() {},
  });

  await assert.rejects(
    () => backend.readResponse({
      prefix: '/cc2s0002',
      segments: [{ name: '/cc2s0002_b0000', size: 16 }],
      segmentIndex: 0,
      segmentName: '/cc2s0002_b0000',
      offset: 15,
      byteLength: 2,
      dedicated: false,
    }, new Uint8Array(2)),
    /exceeds advertised segment/,
  );
  assert.equal(readCount, 0);
});

test('c2-mem-ffi native buddy request backend snapshots metadata and delegates ownership calls', async () => {
  const segments = [{ name: '/cc2n0001_b0000', size: 4096 }];
  const calls = [];
  const backend = createC2MemFfiNativeBuddyRequestBackend({
    prefix: '/cc2n0001',
    segments,
    write(payload) {
      calls.push(['write', Array.from(payload)]);
      return { segmentIndex: 0, offset: 64, byteLength: payload.byteLength, dedicated: false };
    },
    release(block) {
      calls.push(['release', block.offset]);
    },
    forgetConsumed(block) {
      calls.push(['forgetConsumed', block.offset]);
    },
  });
  segments[0] = { name: '/mutated_b0000', size: 1 };

  const block = await backend.writeRequest(new Uint8Array([1, 2, 3]));
  await backend.markRequestConsumed(block);
  await backend.releaseRequest({ segmentIndex: 0, offset: 128, byteLength: 2, dedicated: false });

  assert.equal(backend.prefix, '/cc2n0001');
  assert.deepEqual(backend.segments, [{ name: '/cc2n0001_b0000', size: 4096 }]);
  assert.deepEqual(block, { segmentIndex: 0, offset: 64, byteLength: 3, dedicated: false });
  assert.deepEqual(calls, [
    ['write', [1, 2, 3]],
    ['forgetConsumed', 64],
    ['release', 128],
  ]);
});

test('c2-mem-ffi native buddy request backend releases invalid returned blocks locally', async () => {
  const releases = [];
  const backend = createC2MemFfiNativeBuddyRequestBackend({
    prefix: '/cc2n0002',
    segments: [{ name: '/cc2n0002_b0000', size: 16 }],
    write(payload) {
      return { segmentIndex: 0, offset: 15, byteLength: payload.byteLength, dedicated: false };
    },
    release(block) {
      releases.push(block);
    },
    forgetConsumed() {},
  });

  await assert.rejects(
    () => backend.writeRequest(new Uint8Array([1, 2])),
    /exceeds advertised segment/,
  );
  assert.deepEqual(releases, [{ segmentIndex: 0, offset: 15, byteLength: 2, dedicated: false }]);
});

test('c2-mem-ffi native buddy request backend rejects invalid metadata at construction', () => {
  assert.equal(C2_MEM_FFI_MAX_SHM_PREFIX_BYTES, 24);
  assert.equal(C2_MEM_FFI_MAX_IPC_SHM_SEGMENTS, 16);

  assert.throws(
    () => createC2MemFfiNativeBuddyRequestBackend({
      prefix: '/cc2n_prefix_that_is_too_long',
      segments: [{ name: '/cc2n_prefix_that_is_too_long_b0000', size: 4096 }],
      write() {
        return { segmentIndex: 0, offset: 0, byteLength: 1, dedicated: false };
      },
      release() {},
      forgetConsumed() {},
    }),
    /prefix cannot exceed/,
  );
  assert.throws(
    () => createC2MemFfiNativeBuddyRequestBackend({
      prefix: '/cc2n0004',
      segments: [{ name: '/wrong_b0000', size: 4096 }],
      write() {
        return { segmentIndex: 0, offset: 0, byteLength: 1, dedicated: false };
      },
      release() {},
      forgetConsumed() {},
    }),
    /segment 0 name/,
  );
});

test('c2-mem-ffi native buddy backends close pools idempotently', async () => {
  let requestCloseCount = 0;
  let responseCloseCount = 0;
  const request = createC2MemFfiNativeBuddyRequestBackend({
    prefix: '/cc2n0003',
    segments: [{ name: '/cc2n0003_b0000', size: 4096 }],
    write(payload) {
      return { segmentIndex: 0, offset: 0, byteLength: payload.byteLength, dedicated: false };
    },
    release() {},
    forgetConsumed() {},
    close() {
      requestCloseCount += 1;
    },
  });
  const response = createC2MemFfiNativeBuddyResponseBackend({
    read() {},
    release() {},
    close() {
      responseCloseCount += 1;
    },
  });

  await request.close();
  await request.close();
  await response.close();
  await response.close();

  assert.equal(requestCloseCount, 1);
  assert.equal(responseCloseCount, 1);
});

test('c2-mem-ffi native buddy backends reject calls after close', async () => {
  let requestWriteCount = 0;
  let responseReadCount = 0;
  const request = createC2MemFfiNativeBuddyRequestBackend({
    prefix: '/cc2n0005',
    segments: [{ name: '/cc2n0005_b0000', size: 4096 }],
    write(payload) {
      requestWriteCount += 1;
      return { segmentIndex: 0, offset: 0, byteLength: payload.byteLength, dedicated: false };
    },
    release() {},
    forgetConsumed() {},
    close() {},
  });
  const response = createC2MemFfiNativeBuddyResponseBackend({
    read() {
      responseReadCount += 1;
    },
    release() {},
    close() {},
  });

  await request.close();
  await response.close();

  await assert.rejects(
    () => request.writeRequest(new Uint8Array([1])),
    /closed/,
  );
  await assert.rejects(
    () => response.readResponse({
      prefix: '/cc2s0005',
      segments: [{ name: '/cc2s0005_b0000', size: 4096 }],
      segmentIndex: 0,
      segmentName: '/cc2s0005_b0000',
      offset: 0,
      byteLength: 1,
      dedicated: false,
    }, new Uint8Array(1)),
    /closed/,
  );
  assert.equal(requestWriteCount, 0);
  assert.equal(responseReadCount, 0);
});

test('c2-mem-ffi request pool symbol adapter discovers metadata and delegates lifecycle calls', async () => {
  const calls = [];
  const handle = { kind: 'request' };
  const symbols = {
    c2_mem_ffi_abi_version: abiVersion,
    c2_mem_ffi_request_pool_new(prefix, segmentSize, maxSegments, minBlockSize) {
      calls.push(['new', prefix, segmentSize, maxSegments, minBlockSize]);
      return { status: 0, value: handle };
    },
    c2_mem_ffi_request_pool_destroy(pool) {
      calls.push(['destroy', pool]);
    },
    c2_mem_ffi_request_pool_prefix(pool) {
      calls.push(['prefix', pool]);
      return { status: 0, value: '/cc2n0010' };
    },
    c2_mem_ffi_request_pool_segment_count(pool) {
      calls.push(['segmentCount', pool]);
      return { status: 0, value: 2 };
    },
    c2_mem_ffi_request_pool_segment_name(pool, index) {
      calls.push(['segmentName', index]);
      return { status: 0, value: `/cc2n0010_b000${index}` };
    },
    c2_mem_ffi_request_pool_segment_data_size(pool, index) {
      calls.push(['segmentSize', index]);
      return { status: 0, value: 4096 + index };
    },
    c2_mem_ffi_request_pool_write(pool, payload) {
      calls.push(['write', pool, Array.from(payload)]);
      return { status: 0, value: { segmentIndex: 1, offset: 32, byteLength: payload.byteLength, dedicated: false } };
    },
    c2_mem_ffi_request_pool_release(pool, block) {
      calls.push(['release', pool, block.offset]);
      return { status: 0 };
    },
    c2_mem_ffi_request_pool_forget_consumed(pool, block) {
      calls.push(['forgetConsumed', pool, block.offset]);
      return { status: 0 };
    },
  };

  const pool = await createC2MemFfiRequestPoolFromSymbols(symbols, {
    prefix: '/cc2n0010',
    segmentSize: 4096,
    maxSegments: 2,
    minBlockSize: 512,
  });

  assert.equal(pool.prefix, '/cc2n0010');
  assert.deepEqual(pool.segments, [
    { name: '/cc2n0010_b0000', size: 4096 },
    { name: '/cc2n0010_b0001', size: 4097 },
  ]);
  const block = await pool.write(new Uint8Array([1, 2, 3]));
  await pool.forgetConsumed(block);
  await pool.release({ segmentIndex: 0, offset: 64, byteLength: 1, dedicated: false });
  await pool.close();
  await pool.close();

  assert.deepEqual(calls.map((entry) => entry[0]), [
    'new',
    'prefix',
    'segmentCount',
    'segmentName',
    'segmentSize',
    'segmentName',
    'segmentSize',
    'write',
    'forgetConsumed',
    'release',
    'destroy',
  ]);
});

test('c2-mem-ffi request pool symbol adapter destroys pool when metadata discovery fails', async () => {
  let destroyCount = 0;
  const handle = { kind: 'request' };
  await assert.rejects(
    () => createC2MemFfiRequestPoolFromSymbols({
      c2_mem_ffi_abi_version: abiVersion,
      c2_mem_ffi_request_pool_new() {
        return { status: 0, value: handle };
      },
      c2_mem_ffi_request_pool_destroy(pool) {
        assert.equal(pool, handle);
        destroyCount += 1;
      },
      c2_mem_ffi_request_pool_prefix() {
        return { status: 0, value: '/cc2n0011' };
      },
      c2_mem_ffi_request_pool_segment_count() {
        return { status: 3 };
      },
      c2_mem_ffi_request_pool_segment_name() {
        return { status: 0, value: '/unused_b0000' };
      },
      c2_mem_ffi_request_pool_segment_data_size() {
        return { status: 0, value: 4096 };
      },
      c2_mem_ffi_request_pool_write() {
        return { status: 3 };
      },
      c2_mem_ffi_request_pool_release() {
        return { status: 0 };
      },
      c2_mem_ffi_request_pool_forget_consumed() {
        return { status: 0 };
      },
    }, {
      prefix: '/cc2n0011',
      segmentSize: 4096,
      maxSegments: 1,
      minBlockSize: 512,
    }),
    /POOL_ERROR/,
  );
  assert.equal(destroyCount, 1);
});

test('c2-mem-ffi request pool symbol adapter rejects invalid ownership blocks before pool calls', async () => {
  let releaseCount = 0;
  let forgetConsumedCount = 0;
  const request = await createC2MemFfiRequestPoolFromSymbols({
    c2_mem_ffi_abi_version: abiVersion,
    c2_mem_ffi_request_pool_new() {
      return { status: 0, value: { kind: 'request' } };
    },
    c2_mem_ffi_request_pool_destroy() {},
    c2_mem_ffi_request_pool_prefix() {
      return { status: 0, value: '/cc2n0016' };
    },
    c2_mem_ffi_request_pool_segment_count() {
      return { status: 0, value: 1 };
    },
    c2_mem_ffi_request_pool_segment_name() {
      return { status: 0, value: '/cc2n0016_b0000' };
    },
    c2_mem_ffi_request_pool_segment_data_size() {
      return { status: 0, value: 16 };
    },
    c2_mem_ffi_request_pool_write() {
      return { status: 0, value: { segmentIndex: 0, offset: 0, byteLength: 1, dedicated: false } };
    },
    c2_mem_ffi_request_pool_release() {
      releaseCount += 1;
      return { status: 0 };
    },
    c2_mem_ffi_request_pool_forget_consumed() {
      forgetConsumedCount += 1;
      return { status: 0 };
    },
  }, {
    prefix: '/cc2n0016',
    segmentSize: 4096,
    maxSegments: 1,
    minBlockSize: 512,
  });

  await assert.rejects(
    () => request.release({ segmentIndex: 0, offset: 0, byteLength: 1, dedicated: true }),
    /dedicated/,
  );
  await assert.rejects(
    () => request.release({ segmentIndex: 0, offset: 15, byteLength: 2, dedicated: false }),
    /exceeds advertised segment/,
  );
  await assert.rejects(
    () => request.forgetConsumed({ segmentIndex: 1, offset: 0, byteLength: 1, dedicated: false }),
    /not advertised/,
  );
  await request.close();

  assert.equal(releaseCount, 0);
  assert.equal(forgetConsumedCount, 0);
});

test('c2-mem-ffi response pool symbol adapter delegates read, release, and close', async () => {
  const calls = [];
  const handle = { kind: 'response' };
  const symbols = {
    c2_mem_ffi_abi_version: abiVersion,
    c2_mem_ffi_response_pool_new(prefix, segmentSize, maxSegments, minBlockSize) {
      calls.push(['new', prefix, segmentSize, maxSegments, minBlockSize]);
      return { status: 0, value: handle };
    },
    c2_mem_ffi_response_pool_destroy(pool) {
      calls.push(['destroy', pool]);
    },
    c2_mem_ffi_response_pool_read(pool, block, destination) {
      calls.push(['read', pool, block.offset, destination.byteLength]);
      destination.set([4, 5, 6]);
      return { status: 0 };
    },
    c2_mem_ffi_response_pool_release(pool, block) {
      calls.push(['release', pool, block.offset]);
      return { status: 0 };
    },
  };

  const pool = await createC2MemFfiResponsePoolFromSymbols(symbols, {
    prefix: '/cc2s0010',
    segmentSize: 4096,
    maxSegments: 1,
    minBlockSize: 512,
  });
  const destination = new Uint8Array(3);
  const block = { segmentIndex: 0, offset: 32, byteLength: 3, dedicated: false };
  await pool.read(block, destination);
  await pool.release(block);
  await pool.close();
  await pool.close();

  assert.deepEqual(Array.from(destination), [4, 5, 6]);
  assert.deepEqual(calls.map((entry) => entry[0]), ['new', 'read', 'release', 'destroy']);
});

test('c2-mem-ffi response pool symbol adapter rejects invalid blocks before pool calls', async () => {
  let readCount = 0;
  let releaseCount = 0;
  const response = await createC2MemFfiResponsePoolFromSymbols({
    c2_mem_ffi_abi_version: abiVersion,
    c2_mem_ffi_response_pool_new() {
      return { status: 0, value: { kind: 'response' } };
    },
    c2_mem_ffi_response_pool_destroy() {},
    c2_mem_ffi_response_pool_read() {
      readCount += 1;
      return { status: 0 };
    },
    c2_mem_ffi_response_pool_release() {
      releaseCount += 1;
      return { status: 0 };
    },
  }, {
    prefix: '/cc2s0016',
    segmentSize: 4096,
    maxSegments: 1,
    minBlockSize: 512,
  });

  await assert.rejects(
    () => response.read({ segmentIndex: 0, offset: 0, byteLength: 1, dedicated: true }, new Uint8Array(1)),
    /dedicated/,
  );
  await assert.rejects(
    () => response.read({ segmentIndex: 0, offset: 0, byteLength: 2, dedicated: false }, new Uint8Array(1)),
    /destination length/,
  );
  await assert.rejects(
    () => response.release({ segmentIndex: 0, offset: 0, byteLength: 1, dedicated: true }),
    /dedicated/,
  );
  await response.close();

  assert.equal(readCount, 0);
  assert.equal(releaseCount, 0);
});

test('c2-mem-ffi response pool symbol adapter reports release status failures', async () => {
  const cases = [
    { prefix: '/cc2s0017', releaseStatus: 3, expected: /POOL_ERROR/ },
    { prefix: '/cc2s0018', releaseStatus: 99, expected: /UNKNOWN\(99\)/ },
  ];

  for (const releaseCase of cases) {
    const response = await createC2MemFfiResponsePoolFromSymbols({
      c2_mem_ffi_abi_version: abiVersion,
      c2_mem_ffi_response_pool_new() {
        return { status: 0, value: { kind: releaseCase.prefix } };
      },
      c2_mem_ffi_response_pool_destroy() {},
      c2_mem_ffi_response_pool_read() {
        return { status: 0 };
      },
      c2_mem_ffi_response_pool_release() {
        return { status: releaseCase.releaseStatus };
      },
    }, {
      prefix: releaseCase.prefix,
      segmentSize: 4096,
      maxSegments: 1,
      minBlockSize: 512,
    });

    try {
      await assert.rejects(
        () => response.release({ segmentIndex: 0, offset: 0, byteLength: 1, dedicated: false }),
        releaseCase.expected,
      );
    } finally {
      await response.close();
    }
  }
});

test('c2-mem-ffi symbol adapters reject non-ok statuses and calls after close', async () => {
  const request = await createC2MemFfiRequestPoolFromSymbols({
    c2_mem_ffi_abi_version: abiVersion,
    c2_mem_ffi_request_pool_new() {
      return { status: 0, value: { kind: 'request' } };
    },
    c2_mem_ffi_request_pool_destroy() {},
    c2_mem_ffi_request_pool_prefix() {
      return { status: 0, value: '/cc2n0012' };
    },
    c2_mem_ffi_request_pool_segment_count() {
      return { status: 0, value: 1 };
    },
    c2_mem_ffi_request_pool_segment_name() {
      return { status: 0, value: '/cc2n0012_b0000' };
    },
    c2_mem_ffi_request_pool_segment_data_size() {
      return { status: 0, value: 4096 };
    },
    c2_mem_ffi_request_pool_write() {
      return { status: 4 };
    },
    c2_mem_ffi_request_pool_release() {
      return { status: 0 };
    },
    c2_mem_ffi_request_pool_forget_consumed() {
      return { status: 0 };
    },
  }, {
    prefix: '/cc2n0012',
    segmentSize: 4096,
    maxSegments: 1,
    minBlockSize: 512,
  });

  await assert.rejects(
    () => request.write(new Uint8Array([1])),
    /INSUFFICIENT_BUFFER/,
  );
  await request.close();
  await assert.rejects(
    () => request.release({ segmentIndex: 0, offset: 0, byteLength: 1, dedicated: false }),
    /closed/,
  );

  const response = await createC2MemFfiResponsePoolFromSymbols({
    c2_mem_ffi_abi_version: abiVersion,
    c2_mem_ffi_response_pool_new() {
      return { status: 0, value: { kind: 'response' } };
    },
    c2_mem_ffi_response_pool_destroy() {},
    c2_mem_ffi_response_pool_read() {
      return { status: 4 };
    },
    c2_mem_ffi_response_pool_release() {
      return { status: 0 };
    },
  }, {
    prefix: '/cc2s0012',
    segmentSize: 4096,
    maxSegments: 1,
    minBlockSize: 512,
  });

  const responseBlock = { segmentIndex: 0, offset: 0, byteLength: 1, dedicated: false };
  await assert.rejects(
    () => response.read(responseBlock, new Uint8Array(1)),
    /INSUFFICIENT_BUFFER/,
  );
  await response.close();
  await assert.rejects(
    () => response.release(responseBlock),
    /closed/,
  );
});

test('c2-mem-ffi symbol adapters report unknown status codes explicitly', async () => {
  const request = await createC2MemFfiRequestPoolFromSymbols({
    c2_mem_ffi_abi_version: abiVersion,
    c2_mem_ffi_request_pool_new() {
      return { status: 0, value: { kind: 'request' } };
    },
    c2_mem_ffi_request_pool_destroy() {},
    c2_mem_ffi_request_pool_prefix() {
      return { status: 0, value: '/cc2n0014' };
    },
    c2_mem_ffi_request_pool_segment_count() {
      return { status: 0, value: 1 };
    },
    c2_mem_ffi_request_pool_segment_name() {
      return { status: 0, value: '/cc2n0014_b0000' };
    },
    c2_mem_ffi_request_pool_segment_data_size() {
      return { status: 0, value: 4096 };
    },
    c2_mem_ffi_request_pool_write() {
      return { status: 99 };
    },
    c2_mem_ffi_request_pool_release() {
      return { status: 0 };
    },
    c2_mem_ffi_request_pool_forget_consumed() {
      return { status: 0 };
    },
  }, {
    prefix: '/cc2n0014',
    segmentSize: 4096,
    maxSegments: 1,
    minBlockSize: 512,
  });

  await assert.rejects(
    () => request.write(new Uint8Array([1])),
    /UNKNOWN\(99\)/,
  );

  const responseRead = await createC2MemFfiResponsePoolFromSymbols({
    c2_mem_ffi_abi_version: abiVersion,
    c2_mem_ffi_response_pool_new() {
      return { status: 0, value: { kind: 'response-read' } };
    },
    c2_mem_ffi_response_pool_destroy() {},
    c2_mem_ffi_response_pool_read() {
      return { status: 99 };
    },
    c2_mem_ffi_response_pool_release() {
      return { status: 0 };
    },
  }, {
    prefix: '/cc2s0014',
    segmentSize: 4096,
    maxSegments: 1,
    minBlockSize: 512,
  });

  const responseBlock = { segmentIndex: 0, offset: 0, byteLength: 1, dedicated: false };
  await assert.rejects(
    () => responseRead.read(responseBlock, new Uint8Array(1)),
    /UNKNOWN\(99\)/,
  );
  await responseRead.close();

  const responseRelease = await createC2MemFfiResponsePoolFromSymbols({
    c2_mem_ffi_abi_version: abiVersion,
    c2_mem_ffi_response_pool_new() {
      return { status: 0, value: { kind: 'response-release' } };
    },
    c2_mem_ffi_response_pool_destroy() {},
    c2_mem_ffi_response_pool_read() {
      return { status: 0 };
    },
    c2_mem_ffi_response_pool_release() {
      return { status: 99 };
    },
  }, {
    prefix: '/cc2s0015',
    segmentSize: 4096,
    maxSegments: 1,
    minBlockSize: 512,
  });

  await assert.rejects(
    () => responseRelease.release(responseBlock),
    /UNKNOWN\(99\)/,
  );
  await responseRelease.close();
});

test('c2-mem-ffi request pool symbol adapter reports ownership status failures', async () => {
  const cases = [
    { op: 'release', prefix: '/cc2n0017', releaseStatus: 3, forgetStatus: 0, expected: /POOL_ERROR/ },
    { op: 'release', prefix: '/cc2n0018', releaseStatus: 99, forgetStatus: 0, expected: /UNKNOWN\(99\)/ },
    { op: 'forgetConsumed', prefix: '/cc2n0019', releaseStatus: 0, forgetStatus: 2, expected: /INVALID_ARGUMENT/ },
    { op: 'forgetConsumed', prefix: '/cc2n0020', releaseStatus: 0, forgetStatus: 99, expected: /UNKNOWN\(99\)/ },
  ];

  for (const [index, ownershipCase] of cases.entries()) {
    const prefix = ownershipCase.prefix;
    const request = await createC2MemFfiRequestPoolFromSymbols({
      c2_mem_ffi_abi_version: abiVersion,
      c2_mem_ffi_request_pool_new() {
        return { status: 0, value: { kind: `request-${index}` } };
      },
      c2_mem_ffi_request_pool_destroy() {},
      c2_mem_ffi_request_pool_prefix() {
        return { status: 0, value: prefix };
      },
      c2_mem_ffi_request_pool_segment_count() {
        return { status: 0, value: 1 };
      },
      c2_mem_ffi_request_pool_segment_name() {
        return { status: 0, value: `${prefix}_b0000` };
      },
      c2_mem_ffi_request_pool_segment_data_size() {
        return { status: 0, value: 4096 };
      },
      c2_mem_ffi_request_pool_write(_pool, payload) {
        return { status: 0, value: { segmentIndex: 0, offset: 0, byteLength: payload.byteLength, dedicated: false } };
      },
      c2_mem_ffi_request_pool_release() {
        return { status: ownershipCase.releaseStatus };
      },
      c2_mem_ffi_request_pool_forget_consumed() {
        return { status: ownershipCase.forgetStatus };
      },
    }, {
      prefix,
      segmentSize: 4096,
      maxSegments: 1,
      minBlockSize: 512,
    });

    const block = { segmentIndex: 0, offset: 0, byteLength: 1, dedicated: false };
    try {
      if (ownershipCase.op === 'release') {
        await assert.rejects(
          () => request.release(block),
          ownershipCase.expected,
        );
      } else {
        await assert.rejects(
          () => request.forgetConsumed(block),
          ownershipCase.expected,
        );
      }
    } finally {
      await request.close();
    }
  }
});

test('c2-mem-ffi request pool symbol adapter releases invalid ok blocks locally', async () => {
  const releases = [];
  const request = await createC2MemFfiRequestPoolFromSymbols({
    c2_mem_ffi_abi_version: abiVersion,
    c2_mem_ffi_request_pool_new() {
      return { status: 0, value: { kind: 'request' } };
    },
    c2_mem_ffi_request_pool_destroy() {},
    c2_mem_ffi_request_pool_prefix() {
      return { status: 0, value: '/cc2n0013' };
    },
    c2_mem_ffi_request_pool_segment_count() {
      return { status: 0, value: 1 };
    },
    c2_mem_ffi_request_pool_segment_name() {
      return { status: 0, value: '/cc2n0013_b0000' };
    },
    c2_mem_ffi_request_pool_segment_data_size() {
      return { status: 0, value: 16 };
    },
    c2_mem_ffi_request_pool_write(_pool, payload) {
      return { status: 0, value: { segmentIndex: 0, offset: 15, byteLength: payload.byteLength, dedicated: false } };
    },
    c2_mem_ffi_request_pool_release(_pool, block) {
      releases.push(block);
      return { status: 0 };
    },
    c2_mem_ffi_request_pool_forget_consumed() {
      return { status: 0 };
    },
  }, {
    prefix: '/cc2n0013',
    segmentSize: 4096,
    maxSegments: 1,
    minBlockSize: 512,
  });

  await assert.rejects(
    () => request.write(new Uint8Array([1, 2])),
    /exceeds advertised segment/,
  );
  assert.deepEqual(releases, [{ segmentIndex: 0, offset: 15, byteLength: 2, dedicated: false }]);
});
