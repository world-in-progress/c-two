import assert from 'node:assert/strict';
import { existsSync } from 'node:fs';
import { resolve } from 'node:path';
import test from 'node:test';

import {
  C2_MEM_FFI_ABI_VERSION,
  createC2MemFfiRequestPoolFromSymbols,
  createC2MemFfiResponsePoolFromSymbols,
  loadBundledC2MemFfiNodeNativeSymbols,
  loadC2MemFfiNodeNativeSymbols,
  resolveBundledC2MemFfiNodeNativeLibraryPath,
} from '../dist/index.js';

function libraryPath() {
  const ext = process.platform === 'darwin' ? 'dylib' : 'so';
  return resolve(process.cwd(), '..', '..', '..', '..', 'target', 'debug', `libc2_mem_ffi.${ext}`);
}

test('c2-mem-ffi bundled Node native loader resolves packaged runtime artifacts', (t) => {
  if (process.platform === 'win32') {
    t.skip('c2-mem-ffi Node native loader requires POSIX dlopen and SHM');
    return;
  }
  const bundled = resolveBundledC2MemFfiNodeNativeLibraryPath();
  assert.equal(existsSync(bundled), true, `${bundled} must exist; run npm run build:node-addon`);
  assert.match(bundled, /dist\/native\/libc2_mem_ffi\.(dylib|so)$/);
  const { symbols } = loadBundledC2MemFfiNodeNativeSymbols();
  assert.equal(symbols.c2_mem_ffi_abi_version(), C2_MEM_FFI_ABI_VERSION);
});

test('c2-mem-ffi Node native loader wraps the real request pool C ABI', async (t) => {
  if (process.platform === 'win32') {
    t.skip('c2-mem-ffi Node native loader requires POSIX dlopen and SHM');
    return;
  }
  const dylib = libraryPath();
  assert.equal(existsSync(dylib), true, `${dylib} must exist; run npm run build:rust`);
  const { requestSymbols, symbols } = loadC2MemFfiNodeNativeSymbols(dylib);
  assert.equal(symbols.c2_mem_ffi_abi_version(), C2_MEM_FFI_ABI_VERSION);

  const pool = await createC2MemFfiRequestPoolFromSymbols(requestSymbols, {
    prefix: '/cc2nnode1',
    segmentSize: 65536,
    maxSegments: 1,
    minBlockSize: 4096,
  });
  assert.equal(pool.prefix, '/cc2nnode1');
  assert.equal(pool.segments.length, 1);
  assert.equal(pool.segments[0].name, '/cc2nnode1_b0000');

  const payload = new Uint8Array([10, 20, 30, 40]);
  const block = await pool.write(payload);
  assert.equal(block.dedicated, false);
  assert.equal(block.byteLength, payload.byteLength);

  const handle = (await symbols.c2_mem_ffi_request_pool_new('/cc2nnode2', 65536, 1, 4096)).value;
  const rawBlock = (await symbols.c2_mem_ffi_request_pool_write(handle, payload)).value;
  const destination = new Uint8Array(payload.byteLength);
  const readResult = await symbols.c2_mem_ffi_request_pool_read_local(handle, rawBlock, destination);
  assert.equal(readResult.status, 0);
  assert.equal(readResult.value, payload.byteLength);
  assert.deepEqual(Array.from(destination), Array.from(payload));
  await symbols.c2_mem_ffi_request_pool_release(handle, rawBlock);
  await symbols.c2_mem_ffi_request_pool_destroy(handle);
  await assert.rejects(
    async () => symbols.c2_mem_ffi_request_pool_prefix(handle),
    /closed/,
  );

  await pool.release(block);
  await pool.close?.();
});

test('c2-mem-ffi Node native loader composes real request and response pools', async (t) => {
  if (process.platform === 'win32') {
    t.skip('c2-mem-ffi Node native loader requires POSIX dlopen and SHM');
    return;
  }
  const { requestSymbols, responseSymbols } = loadC2MemFfiNodeNativeSymbols(libraryPath());
  const fakeServerPool = await createC2MemFfiRequestPoolFromSymbols(requestSymbols, {
    prefix: '/cc2snode1',
    segmentSize: 65536,
    maxSegments: 1,
    minBlockSize: 4096,
  });
  const responsePool = await createC2MemFfiResponsePoolFromSymbols(responseSymbols, {
    prefix: '/cc2snode1',
    segmentSize: 65536,
    maxSegments: 1,
    minBlockSize: 4096,
  });
  const payload = new Uint8Array([5, 4, 3, 2, 1]);
  const block = await fakeServerPool.write(payload);
  await fakeServerPool.forgetConsumed(block);

  const destination = new Uint8Array(payload.byteLength);
  await responsePool.read(block, destination);
  assert.deepEqual(Array.from(destination), Array.from(payload));
  await responsePool.release(block);

  await responsePool.close?.();
  await fakeServerPool.close?.();
});

test('c2-mem-ffi Node native loader rejects values before lossy native narrowing', async (t) => {
  if (process.platform === 'win32') {
    t.skip('c2-mem-ffi Node native loader requires POSIX dlopen and SHM');
    return;
  }
  const { symbols } = loadC2MemFfiNodeNativeSymbols(libraryPath());

  assert.throws(
    () => symbols.c2_mem_ffi_request_pool_new('/cc2nnode3', 65536, 65537, 4096),
    /maxSegments/,
  );
  assert.throws(
    () => symbols.c2_mem_ffi_request_pool_new('/cc2nnode5', 65536, 1.5, 4096),
    /maxSegments/,
  );

  const handle = (await symbols.c2_mem_ffi_request_pool_new('/cc2nnode4', 65536, 1, 4096)).value;
  try {
    assert.throws(
      () => symbols.c2_mem_ffi_request_pool_release(handle, {
        segmentIndex: 0,
        offset: 0x1_0000_0000,
        byteLength: 1,
        dedicated: false,
      }),
      /offset/,
    );
    assert.throws(
      () => symbols.c2_mem_ffi_request_pool_release(handle, {
        segmentIndex: 0,
        offset: 0,
        byteLength: 1.5,
        dedicated: false,
      }),
      /byteLength/,
    );
    assert.throws(
      () => symbols.c2_mem_ffi_request_pool_release(handle, {
        segmentIndex: 65536,
        offset: 0,
        byteLength: 1,
        dedicated: false,
      }),
      /segmentIndex/,
    );
    assert.throws(
      () => symbols.c2_mem_ffi_request_pool_release(handle, {
        segmentIndex: 0.5,
        offset: 0,
        byteLength: 1,
        dedicated: false,
      }),
      /segmentIndex/,
    );
  } finally {
    await symbols.c2_mem_ffi_request_pool_destroy(handle);
  }
});
