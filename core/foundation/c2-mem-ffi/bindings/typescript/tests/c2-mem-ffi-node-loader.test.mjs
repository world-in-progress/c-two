import assert from 'node:assert/strict';
import { copyFileSync, existsSync, mkdirSync, mkdtempSync, readFileSync, rmSync, writeFileSync } from 'node:fs';
import { spawnSync } from 'node:child_process';
import { tmpdir } from 'node:os';
import { resolve } from 'node:path';
import { fileURLToPath } from 'node:url';
import test from 'node:test';

import {
  C2_MEM_FFI_ABI_VERSION,
  C2_MEM_FFI_STATUS_INVALID_ARGUMENT,
  C2NodeIpcConnectionError,
  createBundledC2MemFfiNodeRuntime,
  createC2MemFfiRequestPoolFromSymbols,
  createC2MemFfiResponsePoolFromSymbols,
  createNodeIpcConnect,
  loadBundledC2MemFfiNodeNativeSymbols,
  loadC2MemFfiNodeNativeSymbols,
  resolveBundledC2MemFfiNodeNativeLibraryPath,
  resolveLocalIpcEndpoint,
} from '../dist/index.js';

function libraryPath() {
  return resolveBundledC2MemFfiNodeNativeLibraryPath();
}

function localTestDirectory(prefix) {
  const parent = new URL('../target/node-loader-tests/', import.meta.url);
  mkdirSync(parent, { recursive: true });
  return mkdtempSync(resolve(fileURLToPath(parent), prefix));
}

function compileAbi3Fixture(directory, { partialContext = false, ownedContext = false } = {}) {
  const source = readFileSync(new URL('../native/node_c2_mem_ffi_loader.c', import.meta.url), 'utf8');
  const required = [...source.matchAll(/LOAD_REQUIRED\(\w+, "([^"]+)"\)/g)].map((match) => match[1]);
  const body = required.filter((name) => ![
    'c2_mem_ffi_abi_version', 'c2_mem_ffi_local_endpoint_len', 'c2_mem_ffi_local_endpoint_copy',
  ].includes(name)).map((name) =>
    `#[unsafe(no_mangle)] pub extern "C" fn ${name}() { std::process::abort(); }`).join('\n');
  const fixture = resolve(directory, 'abi3.rs');
  writeFileSync(fixture, `
    const NAME: &[u8] = b"/tmp/c2-old-abi3-fixture.sock";
    #[unsafe(no_mangle)] pub extern "C" fn c2_mem_ffi_abi_version() -> u32 { 3 }
    #[unsafe(no_mangle)] pub unsafe extern "C" fn c2_mem_ffi_local_endpoint_len(
        address: *const u8, out: *mut usize) -> u32 {
      if address.is_null() || out.is_null() { return 1; }
      unsafe { *out = NAME.len(); } 0
    }
    #[unsafe(no_mangle)] pub unsafe extern "C" fn c2_mem_ffi_local_endpoint_copy(
        address: *const u8, dst: *mut u8, len: usize, out: *mut usize) -> u32 {
      if address.is_null() || dst.is_null() || out.is_null() { return 1; }
      unsafe { *out = 0; }
      if len < NAME.len() + 1 { return 4; }
      unsafe { std::ptr::copy_nonoverlapping(NAME.as_ptr(), dst, NAME.len());
        *dst.add(NAME.len()) = 0; *out = NAME.len(); } 0
    }
    ${body}
    ${partialContext ? '#[unsafe(no_mangle)] pub extern "C" fn c2_mem_ffi_local_endpoint_context_capture() { std::process::abort(); }' : ''}
    ${ownedContext ? `
      use std::sync::atomic::{AtomicUsize, Ordering};
      static FREES: AtomicUsize = AtomicUsize::new(0);
      pub struct Context(u8);
      unsafe fn copy(value: &str, dst: *mut u8, len: usize, out: *mut usize) -> u32 {
        if dst.is_null() || out.is_null() { return 1; }
        unsafe { *out = 0; }
        if len < value.len() + 1 { return 4; }
        unsafe { std::ptr::copy_nonoverlapping(value.as_ptr(), dst, value.len());
          *dst.add(value.len()) = 0; *out = value.len(); } 0
      }
      #[unsafe(no_mangle)] pub unsafe extern "C" fn c2_mem_ffi_local_endpoint_context_capture(
          _root: *const u8, out: *mut *mut Context) -> u32 {
        if out.is_null() { return 1; }
        unsafe { *out = Box::into_raw(Box::new(Context(1))); } 0
      }
      #[unsafe(no_mangle)] pub unsafe extern "C" fn c2_mem_ffi_local_endpoint_context_free(ptr: *mut Context) {
        if !ptr.is_null() { let owner = unsafe { Box::from_raw(ptr) }; assert_eq!(owner.0, 1);
          FREES.fetch_add(1, Ordering::SeqCst); }
      }
      #[unsafe(no_mangle)] pub unsafe extern "C" fn c2_mem_ffi_local_endpoint_context_name_len(
          ctx: *const Context, address: *const u8, out: *mut usize) -> u32 {
        if ctx.is_null() { return 1; }
        unsafe { c2_mem_ffi_local_endpoint_len(address, out) }
      }
      #[unsafe(no_mangle)] pub unsafe extern "C" fn c2_mem_ffi_local_endpoint_context_name_copy(
          ctx: *const Context, address: *const u8, dst: *mut u8, len: usize, out: *mut usize) -> u32 {
        if ctx.is_null() { return 1; }
        unsafe { c2_mem_ffi_local_endpoint_copy(address, dst, len, out) }
      }
      #[unsafe(no_mangle)] pub unsafe extern "C" fn c2_mem_ffi_local_endpoint_context_namespace_id_len(
          ctx: *const Context, out: *mut usize) -> u32 {
        if ctx.is_null() || out.is_null() { return 1; }
        unsafe { *out = FREES.load(Ordering::SeqCst).to_string().len(); } 0
      }
      #[unsafe(no_mangle)] pub unsafe extern "C" fn c2_mem_ffi_local_endpoint_context_namespace_id_copy(
          ctx: *const Context, dst: *mut u8, len: usize, out: *mut usize) -> u32 {
        if ctx.is_null() { return 1; }
        unsafe { copy(&FREES.load(Ordering::SeqCst).to_string(), dst, len, out) }
      }
    ` : ''}
  `);
  const library = resolve(directory, process.platform === 'win32' ? 'abi3.dll' : process.platform === 'darwin' ? 'abi3.dylib' : 'abi3.so');
  const output = spawnSync(process.env.RUSTC ?? 'rustc',
    ['--edition=2024', '--crate-type=cdylib', fixture, '-o', library],
    { cwd: directory, encoding: 'utf8' });
  assert.equal(output.status, 0, `${output.error ?? ''} ${output.stdout} ${output.stderr}`);
  return library;
}

test('native endpoint resolution rejects NUL and non-object options first', () => {
  assert.throws(() => resolveLocalIpcEndpoint('ipc://bad\0name'), /NUL/);
  assert.throws(() => resolveLocalIpcEndpoint('ipc://invalid-options', null), /options must be an object/);
});
test('logical addresses resolve to one deterministic native OS endpoint', () => {
  const address = `ipc://native-strict-${process.pid}`;
  const endpoint = resolveLocalIpcEndpoint(address);
  if (process.platform === 'win32') {
    assert.ok(endpoint.startsWith('\\\\.\\pipe\\c_two-'));
  } else {
    assert.match(endpoint, /^\/tmp\/c2-[0-9a-f]+\/[0-9a-f]{32}$/);
  }
  assert.equal(endpoint, resolveLocalIpcEndpoint(address));
  const runtime = createBundledC2MemFfiNodeRuntime();
  assert.equal(runtime.resolveEndpoint(address), endpoint);
});

test('c2-mem-ffi bundled Node native loader resolves packaged runtime artifacts', () => {
  const bundled = resolveBundledC2MemFfiNodeNativeLibraryPath();
  assert.equal(existsSync(bundled), true, `${bundled} must exist; run npm run build:node-addon`);
  assert.match(bundled, /dist[\\/]native[\\/](?:libc2_mem_ffi\.(?:dylib|so)|c2_mem_ffi\.dll)$/);
  const { symbols } = loadBundledC2MemFfiNodeNativeSymbols();
  assert.equal(symbols.c2_mem_ffi_abi_version(), C2_MEM_FFI_ABI_VERSION);
});

test('native context snapshots resolver roots and env files without creating endpoint directories', {
  skip: process.platform === 'win32' && 'Unix roots are explicitly unsupported on Windows',
}, () => {
  const { symbols } = loadBundledC2MemFfiNodeNativeSymbols();
  assert.equal(typeof symbols.c2_mem_ffi_local_endpoint_context_capture, 'function');
  const directory = localTestDirectory('snapshot-');
  const envFile = resolve(directory, 'endpoint.env');
  const savedRoot = process.env.C2_IPC_ROOT;
  const savedEnvFile = process.env.C2_ENV_FILE;
  const suffix = `${process.pid.toString(16)}${Date.now().toString(16).slice(-4)}`;
  const rootA = `/tmp/cna${suffix}`;
  const rootB = `/tmp/cnb${suffix}`;
  const address = 'ipc://node-context-snapshot';
  let captured, explicit, fromFile;
  try {
    assert.equal(existsSync(rootA), false);
    assert.equal(existsSync(rootB), false);
    process.env.C2_ENV_FILE = '';
    process.env.C2_IPC_ROOT = rootA;
    const result = symbols.c2_mem_ffi_local_endpoint_context_capture(null);
    assert.equal(result.status, 0);
    captured = result.value;
    const endpointA = captured.endpointName(address).value;
    const namespaceA = captured.namespaceId().value;
    assert.ok(endpointA.startsWith(`${rootA}/`));

    process.env.C2_IPC_ROOT = rootB;
    assert.equal(captured.endpointName(address).value, endpointA);
    assert.equal(captured.namespaceId().value, namespaceA);
    assert.ok(symbols.c2_mem_ffi_local_endpoint(address).value.startsWith(`${rootB}/`));
    explicit = symbols.c2_mem_ffi_local_endpoint_context_capture(rootA).value;
    assert.equal(explicit.endpointName(address).value, endpointA);
    assert.equal(explicit.namespaceId().value, namespaceA);

    process.env.C2_ENV_FILE = directory; // An unreadable-as-file env source.
    const explicitWithoutFile = symbols.c2_mem_ffi_local_endpoint_context_capture(rootA);
    assert.equal(explicitWithoutFile.status, 0);
    try {
      assert.equal(explicitWithoutFile.value.endpointName(address).value, endpointA);
    } finally {
      explicitWithoutFile.value.close();
    }
    assert.deepEqual(symbols.c2_mem_ffi_local_endpoint_context_capture(null), {
      status: C2_MEM_FFI_STATUS_INVALID_ARGUMENT,
    });

    delete process.env.C2_IPC_ROOT;
    process.env.C2_ENV_FILE = envFile;
    writeFileSync(envFile, `C2_IPC_ROOT=${rootA}\n`);
    fromFile = symbols.c2_mem_ffi_local_endpoint_context_capture(null).value;
    writeFileSync(envFile, `C2_IPC_ROOT=${rootB}\n`);
    assert.equal(fromFile.endpointName(address).value, endpointA);
    assert.ok(symbols.c2_mem_ffi_local_endpoint(address).value.startsWith(`${rootB}/`));
    for (const invalid of ['', 'relative', '/tmp/../bad']) {
      assert.deepEqual(symbols.c2_mem_ffi_local_endpoint_context_capture(invalid), {
        status: C2_MEM_FFI_STATUS_INVALID_ARGUMENT,
      });
    }
    const longRoot = '/tmp/' + 'x'.repeat(200);
    const overlong = symbols.c2_mem_ffi_local_endpoint_context_capture(longRoot);
    assert.equal(overlong.status, 0, 'root parsing is pure');
    try {
      const derived = overlong.value.endpointName(address);
      assert.equal(derived.status, 0);
      assert.ok(derived.value.startsWith(`${longRoot}/`));
      assert.equal(derived.value.slice(longRoot.length + 1).length, 32);
      assert.equal(existsSync(longRoot), false);
    } finally {
      overlong.value.close();
    }
    assert.equal(existsSync(rootA), false);
    assert.equal(existsSync(rootB), false);
  } finally {
    captured?.close(); explicit?.close(); fromFile?.close();
    if (savedRoot === undefined) delete process.env.C2_IPC_ROOT;
    else process.env.C2_IPC_ROOT = savedRoot;
    if (savedEnvFile === undefined) delete process.env.C2_ENV_FILE;
    else process.env.C2_ENV_FILE = savedEnvFile;
    rmSync(directory, { recursive: true, force: true });
  }
});

test('native context is opaque, validates receivers and closes idempotently', () => {
  const { symbols } = loadBundledC2MemFfiNodeNativeSymbols();
  const result = symbols.c2_mem_ffi_local_endpoint_context_capture(null);
  assert.equal(result.status, 0);
  const context = result.value;
  try {
    assert.deepEqual(Object.keys(context), []);
    assert.deepEqual(context.endpointName('/tmp/absolute.sock'), { status: C2_MEM_FFI_STATUS_INVALID_ARGUMENT });
    assert.throws(() => context.endpointName('ipc://bad\0name'), /NUL/);
    assert.throws(() => context.endpointName.call({}, 'ipc://borrowed'), /context receiver/);
    assert.throws(() => context.namespaceId.call({}), /context receiver/);
    assert.throws(() => context.close.call({}), /context receiver/);
    assert.equal(Reflect.deleteProperty(context, 'close'), false);
    assert.throws(() => symbols.c2_mem_ffi_local_endpoint_context_capture('bad\0root'), /NUL/);
    assert.throws(() => symbols.c2_mem_ffi_local_endpoint_context_capture(123), /string or null/);
  } finally {
    context.close();
    context.close();
  }
  assert.throws(() => context.endpointName('ipc://closed'), /context is closed/);
  assert.throws(() => context.namespaceId(), /context is closed/);
});

test('Runtime shares its first-attempt native snapshot with queries and reconnects in an isolated environment', {
  skip: process.platform === 'win32' && 'Unix env-root freeze; Windows root rejection remains separately covered',
}, () => {
  const moduleUrl = new URL('../dist/index.js', import.meta.url).href;
  const script = `
    import assert from 'node:assert/strict';
    import net from 'node:net';
    import { EventEmitter } from 'node:events';
    import { existsSync } from 'node:fs';
    import { createBundledC2MemFfiNodeRuntime } from ${JSON.stringify(moduleUrl)};
    const rootA = '/tmp/c2ra' + process.pid;
    const rootB = '/tmp/c2rb' + process.pid;
    assert.equal(existsSync(rootA), false); assert.equal(existsSync(rootB), false);
    process.env.C2_ENV_FILE = '';
    process.env.C2_IPC_ROOT = rootA;
    const paths = [], writes = [];
    let fail = false;
    class Socket extends EventEmitter {
      destroyed = false;
      write(bytes, callback) { writes.push(bytes); callback?.(); return true; }
      end() { this.emit('end'); return this; }
      destroy() { if (!this.destroyed) { this.destroyed = true; this.emit('close'); } return this; }
    }
    // Actual native derivation, mocked sockets only; this is not transport evidence.
    const mockConnect = (path) => {
      paths.push(path);
      const socket = new Socket();
      const failing = fail;
      queueMicrotask(() => socket.emit(failing ? 'error' : 'connect', new Error('first attempt failed')));
      return socket;
    };
    const runtime = createBundledC2MemFfiNodeRuntime({ createConnection: mockConnect });
    const queryA = runtime.resolveEndpoint('ipc://same');
    process.env.C2_IPC_ROOT = rootB;
    const queryB = runtime.resolveEndpoint('ipc://same');
    assert.notEqual(queryA, queryB, 'pure queries must not freeze');
    process.env.C2_IPC_ROOT = rootA;
    const first = await runtime.connect('ipc://same');
    process.env.C2_IPC_ROOT = rootB;
    assert.equal(runtime.resolveEndpoint('ipc://same'), queryA);
    const second = await runtime.connect('ipc://other');
    assert.ok(paths.at(-1).startsWith(rootA + '/'));
    first.close();
    const retry = await runtime.connect('ipc://same');
    assert.equal(paths.at(-1), queryA);
    const fresh = createBundledC2MemFfiNodeRuntime({ createConnection: mockConnect });
    const fromB = await fresh.connect('ipc://same');
    assert.equal(paths.at(-1), queryB);
    runtime.close(); runtime.close();
    const payload = new Uint8Array([1, 2, 3]);
    await second.write(payload);
    assert.equal(writes.at(-1), payload, 'snapshot disposal must not alter payload writes');
    second.close(); retry.close(); fromB.close(); fresh.close();
    assert.throws(() => runtime.resolveEndpoint('ipc://same'), /connector is closed/);

    process.env.C2_IPC_ROOT = rootA;
    const failed = createBundledC2MemFfiNodeRuntime({ createConnection: mockConnect });
    fail = true;
    await assert.rejects(() => failed.connect('ipc://same'), /first attempt failed/);
    process.env.C2_IPC_ROOT = rootB;
    assert.equal(failed.resolveEndpoint('ipc://same'), queryA);
    fail = false;
    const afterFailure = await failed.connect('ipc://same');
    assert.equal(paths.at(-1), queryA);
    afterFailure.close();
    const again = await failed.connect('ipc://different');
    assert.ok(paths.at(-1).startsWith(rootA + '/'));
    again.close(); failed.close();
    assert.equal(existsSync(rootA), false); assert.equal(existsSync(rootB), false);
  `;
  const output = spawnSync(process.execPath, ['--input-type=module', '-e', script], { encoding: 'utf8' });
  assert.equal(output.status, 0, output.stdout + output.stderr);
});

test('Windows context keeps current-SID pipe defaults and rejects Unix roots natively', {
  skip: process.platform !== 'win32' && 'Requires the real Windows platform',
}, () => {
  const { symbols } = loadBundledC2MemFfiNodeNativeSymbols();
  const savedRoot = process.env.C2_IPC_ROOT;
  const savedEnvFile = process.env.C2_ENV_FILE;
  let context;
  try {
    delete process.env.C2_IPC_ROOT;
    process.env.C2_ENV_FILE = '';
    const result = symbols.c2_mem_ffi_local_endpoint_context_capture(null);
    assert.equal(result.status, 0);
    context = result.value;
    const endpoint = context.endpointName('ipc://windows-context').value;
    assert.ok(endpoint.startsWith('\\\\.\\pipe\\c_two-'));
    assert.equal(endpoint, symbols.c2_mem_ffi_local_endpoint('ipc://windows-context').value);
    for (const root of ['C:\\endpoint-root', '\\\\.\\pipe\\arbitrary', '/tmp/root']) {
      assert.deepEqual(symbols.c2_mem_ffi_local_endpoint_context_capture(root), {
        status: C2_MEM_FFI_STATUS_INVALID_ARGUMENT,
      });
    }
    process.env.C2_IPC_ROOT = 'C:\\endpoint-root';
    assert.deepEqual(symbols.c2_mem_ffi_local_endpoint_context_capture(null), {
      status: C2_MEM_FFI_STATUS_INVALID_ARGUMENT,
    });
    assert.equal(context.endpointName('ipc://windows-context').value, endpoint);
  } finally {
    context?.close();
    if (savedRoot === undefined) delete process.env.C2_IPC_ROOT;
    else process.env.C2_IPC_ROOT = savedRoot;
    if (savedEnvFile === undefined) delete process.env.C2_ENV_FILE;
    else process.env.C2_ENV_FILE = savedEnvFile;
  }
});

test('older and partially extended ABI3 libraries preserve default endpoints but expose no context capability', () => {
  for (const partialContext of [false, true]) {
    const directory = localTestDirectory(partialContext ? 'partial-abi3-' : 'old-abi3-');
    try {
      const library = compileAbi3Fixture(directory, { partialContext });
      const moduleUrl = new URL('../dist/index.js', import.meta.url).href;
      const script = `
        import assert from 'node:assert/strict';
        import { EventEmitter } from 'node:events';
        import { loadC2MemFfiNodeNativeSymbols, captureLocalIpcEndpointContextFromSymbols, createNodeIpcConnect } from ${JSON.stringify(moduleUrl)};
        const { symbols } = loadC2MemFfiNodeNativeSymbols(process.argv[1]);
        assert.equal(symbols.c2_mem_ffi_abi_version(), 3);
        assert.equal(symbols.c2_mem_ffi_local_endpoint_context_capture, undefined);
        assert.deepEqual(symbols.c2_mem_ffi_local_endpoint('ipc://older'), {status: 0, value: '/tmp/c2-old-abi3-fixture.sock'});
        assert.throws(() => captureLocalIpcEndpointContextFromSymbols(symbols), /unsupported|does not support/i);
        assert.throws(() => captureLocalIpcEndpointContextFromSymbols(symbols, {ipcRoot: '/tmp/root'}), /unsupported|does not support/i);
        let opened = 0;
        const open = (endpoint) => {
          opened += 1;
          assert.equal(endpoint, '/tmp/c2-old-abi3-fixture.sock');
          const socket = new EventEmitter();
          socket.destroyed = false;
          socket.destroy = () => { socket.destroyed = true; return socket; };
          socket.end = () => socket;
          queueMicrotask(() => socket.emit('connect'));
          return socket;
        };
        const connection = await createNodeIpcConnect({nativeSymbols: symbols, createConnection: open})('ipc://older');
        await connection.close();
        assert.equal(opened, 1);
        await assert.rejects(() => createNodeIpcConnect({nativeSymbols: symbols, ipcRoot: '/tmp/root', createConnection: open})('ipc://older'), /unsupported|does not support/i);
        assert.equal(opened, 1);
      `;
      const output = spawnSync(process.execPath, ['--input-type=module', '-e', script, library], { encoding: 'utf8' });
      assert.equal(output.status, 0, output.stdout + output.stderr);
    } finally {
      // A separate process releases the fixture DLL before Windows cleanup.
      rmSync(directory, { recursive: true, force: true });
    }
  }
});

test('opaque native context frees exactly once on close or GC and detached methods cannot retain a dangling pointer', () => {
  const directory = localTestDirectory('context-ownership-');
  try {
    // This instrumented library measures addon ownership, not transport behavior.
    const library = compileAbi3Fixture(directory, { ownedContext: true });
    const moduleUrl = new URL('../dist/index.js', import.meta.url).href;
    const script = `
      import assert from 'node:assert/strict';
      import { loadC2MemFfiNodeNativeSymbols } from ${JSON.stringify(moduleUrl)};
      const { symbols } = loadC2MemFfiNodeNativeSymbols(process.argv[1]);
      const capture = () => symbols.c2_mem_ffi_local_endpoint_context_capture(null).value;
      const counter = capture();
      let owner = capture();
      assert.equal(counter.namespaceId().value, '0');
      owner.close(); owner.close();
      assert.equal(counter.namespaceId().value, '1');
      const detached = owner.endpointName;
      owner = null;
      let abandoned = capture();
      const detachedAbandoned = abandoned.endpointName;
      abandoned = null;
      for (let attempt = 0; attempt < 100 && counter.namespaceId().value !== '2'; attempt += 1) {
        global.gc();
        await new Promise((resolve) => setTimeout(resolve, 5));
      }
      assert.equal(counter.namespaceId().value, '2', 'closed context GC must not free twice; abandoned context GC must free');
      assert.throws(() => detached.call({}, 'ipc://closed'), /context receiver/);
      assert.throws(() => detachedAbandoned.call({}, 'ipc://collected'), /context receiver/);
      counter.close();
    `;
    const output = spawnSync(process.execPath, ['--expose-gc', '--input-type=module', '-e', script, library], { encoding: 'utf8' });
    assert.equal(output.status, 0, output.stdout + output.stderr);
  } finally {
    rmSync(directory, { recursive: true, force: true });
  }
});

test('typed shared snapshot frees after its last factory or accessor owner, including GC fallback', () => {
  const directory = localTestDirectory('shared-context-ownership-');
  try {
    const library = compileAbi3Fixture(directory, { ownedContext: true });
    const moduleUrl = new URL('../dist/index.js', import.meta.url).href;
    const script = `
      import assert from 'node:assert/strict';
      import { EventEmitter } from 'node:events';
      import { loadC2MemFfiNodeNativeSymbols, captureLocalIpcEndpointContextFromSymbols, createNodeIpcConnect } from ${JSON.stringify(moduleUrl)};
      const { symbols } = loadC2MemFfiNodeNativeSymbols(process.argv[1]);
      const counter = symbols.c2_mem_ffi_local_endpoint_context_capture(null).value;
      const open = () => {
        const socket = new EventEmitter();
        socket.destroyed = false;
        socket.end = () => socket;
        socket.destroy = () => { socket.destroyed = true; socket.emit('close'); return socket; };
        queueMicrotask(() => socket.emit('connect'));
        return socket;
      };
      const context = captureLocalIpcEndpointContextFromSymbols(symbols);
      const a = createNodeIpcConnect({endpointContext: context, createConnection: open});
      const b = createNodeIpcConnect({endpointContext: context, createConnection: open});
      const first = await a('ipc://first'), second = await b('ipc://second');
      first.close(); second.close();
      context.close(); context.close(); a.close(); a.close();
      assert.equal(counter.namespaceId().value, '0');
      assert.equal(b.resolveEndpoint('ipc://third'), '/tmp/c2-old-abi3-fixture.sock');
      b.close(); b.close();
      assert.equal(counter.namespaceId().value, '1');

      let handle = captureLocalIpcEndpointContextFromSymbols(symbols);
      const weakHandle = new WeakRef(handle);
      const retained = createNodeIpcConnect({endpointContext: handle, createConnection: open});
      const retainedSocket = await retained('ipc://retained');
      handle = null;
      for (let attempt = 0; attempt < 10; attempt += 1) {
        await new Promise(r => setTimeout(r, 5)); global.gc();
      }
      assert.equal(weakHandle.deref(), undefined, 'factory retains the snapshot, not the disposable caller handle');
      assert.equal(counter.namespaceId().value, '1');
      retained.close(); retainedSocket.close();
      assert.equal(counter.namespaceId().value, '2', 'last explicit factory close releases an abandoned caller reference');

      let abandoned = createNodeIpcConnect({nativeSymbols: symbols, createConnection: open});
      const connection = await abandoned('ipc://gc');
      let query = abandoned.resolveEndpoint;
      abandoned = null;
      for (let attempt = 0; attempt < 3; attempt += 1) { global.gc(); await new Promise(r => setTimeout(r, 5)); }
      assert.equal(counter.namespaceId().value, '2', 'detached query must retain a live snapshot');
      assert.equal(query('ipc://live'), '/tmp/c2-old-abi3-fixture.sock');
      query = null;
      connection.close();
      for (let attempt = 0; attempt < 100 && counter.namespaceId().value !== '3'; attempt += 1) {
        global.gc(); await new Promise(r => setTimeout(r, 5));
      }
      assert.equal(counter.namespaceId().value, '3', 'unreachable factory snapshot must free once');
      counter.close();
    `;
    const output = spawnSync(process.execPath, ['--expose-gc', '--input-type=module', '-e', script, library], { encoding: 'utf8' });
    assert.equal(output.status, 0, output.stdout + output.stderr);
  } finally {
    rmSync(directory, { recursive: true, force: true });
  }
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

test('c2-mem-ffi response pool bootstraps a differently sized owner backing', async () => {
  const { requestSymbols, responseSymbols } = loadC2MemFfiNodeNativeSymbols(libraryPath());
  const owner = await createC2MemFfiRequestPoolFromSymbols(requestSymbols, {
    prefix: '/cc2snode2',
    segmentSize: 1024 * 1024,
    maxSegments: 1,
    minBlockSize: 4096,
  });
  // The reader only declares the minimum legal peer geometry: the real backing
  // geometry must come from the mapped segment, never from this bootstrap floor.
  const bootstrapFloor = 2 * 4096;
  const responsePool = await createC2MemFfiResponsePoolFromSymbols(responseSymbols, {
    prefix: owner.prefix,
    segmentSize: bootstrapFloor,
    maxSegments: 1,
    minBlockSize: 4096,
  });
  assert.ok(owner.segments[0].size >= 1024 * 1024);

  const payload = new Uint8Array(4096).fill(0x5a);
  const block = await owner.write(payload);
  await owner.forgetConsumed(block);

  const destination = new Uint8Array(payload.byteLength);
  await responsePool.read(block, destination);
  assert.deepEqual(Array.from(destination), Array.from(payload));
  await responsePool.release(block);
  await assert.rejects(
    () => responsePool.release(block),
    /INVALID_ARGUMENT|POOL_ERROR/,
  );

  await assert.rejects(
    () => responsePool.read(
      { ...block, generation: block.generation + 1 },
      new Uint8Array(payload.byteLength),
    ),
    /POOL_ERROR/,
    'an unbacked generation must never be fabricated',
  );

  await responsePool.close?.();
  await owner.close?.();
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
    assert.match(endpoint, /^\/tmp\/c2-[0-9a-f]+\/[0-9a-f]{32}$/);
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


test('native addon rejects a stale core ABI before exposing endpoint or pool calls', () => {
  const directory = mkdtempSync(resolve(tmpdir(), 'c2-stale-abi-'));
  try {
    const source = readFileSync(new URL('../native/node_c2_mem_ffi_loader.c', import.meta.url), 'utf8');
    const symbols = [...source.matchAll(/LOAD_REQUIRED\(\w+, "([^"]+)"\)/g)].map((match) => match[1]);
    const body = symbols.map((name) => name === 'c2_mem_ffi_abi_version'
      ? `#[unsafe(no_mangle)] pub extern "C" fn ${name}() -> u32 { 2 }`
      : `#[unsafe(no_mangle)] pub extern "C" fn ${name}() { std::process::abort(); }`).join('\n');
    const fixture = resolve(directory, 'stale.rs');
    writeFileSync(fixture, body);
    const library = resolve(directory, process.platform === 'win32' ? 'stale.dll' : process.platform === 'darwin' ? 'stale.dylib' : 'stale.so');
    const output = spawnSync(process.env.RUSTC ?? 'rustc',
      ['--edition=2024', '--crate-type=cdylib', fixture, '-o', library],
      { cwd: directory, encoding: 'utf8' });
    assert.equal(output.status, 0, `${output.error ?? ''} ${output.stdout} ${output.stderr}`);
    assert.throws(() => loadC2MemFfiNodeNativeSymbols(library), /ABI version 2.*expected version 3/);
  } finally {
    rmSync(directory, { recursive: true, force: true });
  }
});
