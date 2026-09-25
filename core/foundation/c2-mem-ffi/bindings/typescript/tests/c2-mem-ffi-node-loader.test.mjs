import assert from 'node:assert/strict';
import { copyFileSync, existsSync, mkdtempSync, rmSync } from 'node:fs';
import { spawnSync } from 'node:child_process';
import { tmpdir } from 'node:os';
import { resolve } from 'node:path';
import test from 'node:test';

import {
  C2_MEM_FFI_ABI_VERSION,
  createBundledC2MemFfiNodeRuntime,
  createC2MemFfiRequestPoolFromSymbols,
  createC2MemFfiResponsePoolFromSymbols,
  loadBundledC2MemFfiNodeNativeSymbols,
  loadC2MemFfiNodeNativeSymbols,
  resolveBundledC2MemFfiNodeNativeLibraryPath,
  resolveLocalIpcEndpoint,
} from '../dist/index.js';

function libraryPath() {
  return resolveBundledC2MemFfiNodeNativeLibraryPath();
}

test('c2-mem-ffi bundled Node native loader resolves packaged runtime artifacts', () => {
  const bundled = resolveBundledC2MemFfiNodeNativeLibraryPath();
  assert.equal(existsSync(bundled), true, `${bundled} must exist; run npm run build:node-addon`);
  assert.match(bundled, /dist[\\/]native[\\/](?:libc2_mem_ffi\.(?:dylib|so)|c2_mem_ffi\.dll)$/);
  const { symbols } = loadBundledC2MemFfiNodeNativeSymbols();
  assert.equal(symbols.c2_mem_ffi_abi_version(), C2_MEM_FFI_ABI_VERSION);
});

test('c2-mem-ffi Node native loader wraps the real request pool C ABI', async () => {
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
  assert.match(pool.prefix, /^\/cc2nnode1_[0-9a-f]{40}$/);
  assert.equal(pool.segments.length, 1);
  assert.equal(pool.segments[0].size, 65536);

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

test('c2-mem-ffi Node native loader composes real request and response pools', async () => {
  const { requestSymbols, responseSymbols } = loadC2MemFfiNodeNativeSymbols(libraryPath());
  const fakeServerPool = await createC2MemFfiRequestPoolFromSymbols(requestSymbols, {
    prefix: '/cc2snode1',
    segmentSize: 65536,
    maxSegments: 1,
    minBlockSize: 4096,
  });
  const responsePool = await createC2MemFfiResponsePoolFromSymbols(responseSymbols, {
    prefix: fakeServerPool.prefix,
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

test('bundled Node runtime exposes generated-transport compatible IPC support', async () => {
  const { requestSymbols } = loadBundledC2MemFfiNodeNativeSymbols();
  const fakeServerPool = await createC2MemFfiRequestPoolFromSymbols(requestSymbols, {
    prefix: '/cc2snode3',
    segmentSize: 65536,
    maxSegments: 1,
    minBlockSize: 4096,
  });
  const runtime = createBundledC2MemFfiNodeRuntime();
  assert.equal(typeof runtime.connect, 'function');
  assert.equal(
    typeof runtime.responsePoolFactory.createResponsePool,
    'function',
  );
  const responsePool = await runtime.responsePoolFactory.createResponsePool({
    prefix: fakeServerPool.prefix,
    segmentSize: 65536,
    maxSegments: 1,
    minBlockSize: 4096,
  });
  const payload = new Uint8Array([6, 5, 4]);
  const block = await fakeServerPool.write(payload);
  await fakeServerPool.forgetConsumed(block);
  const destination = new Uint8Array(payload.byteLength);
  await responsePool.read(block, destination);
  assert.deepEqual(Array.from(destination), Array.from(payload));
  await responsePool.release(block);
  await responsePool.close?.();
  await fakeServerPool.close?.();
});

test('c2-mem-ffi Node native loader rejects values before lossy native narrowing', async () => {
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
        segmentIndex: 0, generation: 7,
        offset: 0x1_0000_0000,
        byteLength: 1,
        dedicated: false,
      }),
      /offset/,
    );
    assert.throws(
      () => symbols.c2_mem_ffi_request_pool_release(handle, {
        segmentIndex: 0, generation: 7,
        offset: 0,
        byteLength: 1.5,
        dedicated: false,
      }),
      /byteLength/,
    );
    assert.throws(
      () => symbols.c2_mem_ffi_request_pool_release(handle, {
        segmentIndex: 65536, generation: 7,
        offset: 0,
        byteLength: 1,
        dedicated: false,
      }),
      /segmentIndex/,
    );
    assert.throws(
      () => symbols.c2_mem_ffi_request_pool_release(handle, {
        segmentIndex: 0.5, generation: 7,
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

test('native endpoint resolution preserves logical identity and rejects path input', () => {
  const address = `ipc://node-native-${process.pid}`;
  const endpoint = resolveLocalIpcEndpoint(address);
  assert.equal(endpoint, resolveLocalIpcEndpoint(address));
  if (process.platform === 'win32') {
    assert.match(endpoint, /^\\\\\.\\pipe\\c_two-/);
  } else {
    assert.equal(endpoint, `/tmp/c_two_ipc/node-native-${process.pid}.sock`);
  }
  for (const invalid of ['/tmp/node.sock', 'ipc://../bad', 'ipc://bad\0name']) {
    assert.throws(() => resolveLocalIpcEndpoint(invalid));
  }
});

test('native loader accepts a library in a Unicode directory with spaces', () => {
  const directory = mkdtempSync(resolve(tmpdir(), 'c2-内存 loader-'));
  const library = resolve(directory, process.platform === 'win32' ? 'memory.dll' : 'memory-library');
  copyFileSync(libraryPath(), library);
  try {
    const moduleUrl = new URL('../dist/index.js', import.meta.url).href;
    const script = `import { C2_MEM_FFI_ABI_VERSION, loadC2MemFfiNodeNativeSymbols } from ${JSON.stringify(moduleUrl)};
      const { symbols } = loadC2MemFfiNodeNativeSymbols(process.argv[1]);
      if (symbols.c2_mem_ffi_abi_version() !== C2_MEM_FFI_ABI_VERSION) process.exit(1);`;
    const result = spawnSync(process.execPath, ['--input-type=module', '-e', script, library], { encoding: 'utf8' });
    assert.equal(result.status, 0, result.stdout + result.stderr);
  } finally {
    // A child process owns the loaded DLL so Windows can delete it after exit.
    rmSync(directory, { recursive: true, force: true });
  }
});


test('native request and response paths reject a different backing generation', async () => {
  const { symbols, responseSymbols } = loadBundledC2MemFfiNodeNativeSymbols();
  const result = await symbols.c2_mem_ffi_request_pool_new('/c2_generation', 65536, 1, 4096);
  assert.equal(result.status, 0);
  const handle = result.value;
  let response;
  let block;
  try {
    const prefix = (await symbols.c2_mem_ffi_request_pool_prefix(handle)).value;
    block = (await symbols.c2_mem_ffi_request_pool_write(handle, new Uint8Array([3, 2, 1]))).value;
    assert.ok(block.generation > 0);
    const stale = { ...block, generation: block.generation + 1 };
    const destination = new Uint8Array(3);
    assert.notEqual((await symbols.c2_mem_ffi_request_pool_read_local(handle, stale, destination)).status, 0);
    assert.notEqual((await symbols.c2_mem_ffi_request_pool_release(handle, stale)).status, 0);
    response = await createC2MemFfiResponsePoolFromSymbols(responseSymbols, {
      prefix, segmentSize: 65536, maxSegments: 1, minBlockSize: 4096,
    });
    await assert.rejects(() => response.read(stale, destination), /POOL_ERROR/);
    await response.read(block, destination);
    assert.deepEqual(Array.from(destination), [3, 2, 1]);
    await symbols.c2_mem_ffi_request_pool_forget_consumed(handle, block);
    await response.release(block);
    block = undefined;
  } finally {
    await response?.close();
    if (block !== undefined) await symbols.c2_mem_ffi_request_pool_release(handle, block);
    await symbols.c2_mem_ffi_request_pool_destroy(handle);
  }
});

test('bundled runtime defers native loading until an IPC operation', () => {
  const runtime = createBundledC2MemFfiNodeRuntime({ addonPath: '/missing-c2-addon.node' });
  assert.equal(typeof runtime.connect, 'function');
  assert.throws(() => runtime.resolveEndpoint('ipc://lazy-native'), /Cannot find module|missing-c2-addon/);
});


test('native dedicated requests and responses complete repeatedly without exhausting the owner', async () => {
  const { requestSymbols, responseSymbols } = loadBundledC2MemFfiNodeNativeSymbols();
  const owner = await createC2MemFfiRequestPoolFromSymbols(requestSymbols, {
    prefix: '/c2_dedicated', segmentSize: 65536, maxSegments: 1, minBlockSize: 4096,
  });
  const peer = await createC2MemFfiResponsePoolFromSymbols(responseSymbols, {
    prefix: owner.prefix, segmentSize: 65536, maxSegments: 1, minBlockSize: 4096,
  });
  try {
    // More requests than the native dedicated concurrency limit proves completion
    // drops local authority and allows acknowledged creator mappings to be reclaimed.
    for (let index = 0; index < 8; index += 1) {
      const payload = new Uint8Array(128 * 1024 + 3).fill(index + 1);
      const block = await owner.write(payload);
      assert.equal(block.dedicated, true);
      assert.equal(block.generation, 0);
      assert.equal(block.offset, 0);
      await owner.forgetConsumed(block);
      if (index % 2 === 0) {
        const destination = new Uint8Array(payload.byteLength);
        await peer.read(block, destination);
        assert.deepEqual(destination, payload);
      }
      await peer.release(block);
      await assert.rejects(() => peer.release(block));
    }
  } finally {
    await peer.close();
    await owner.close();
  }
});
