import assert from 'node:assert/strict';
import { copyFileSync, existsSync, mkdtempSync, rmSync, writeFileSync } from 'node:fs';
import { spawnSync } from 'node:child_process';
import { tmpdir } from 'node:os';
import { resolve } from 'node:path';
import test from 'node:test';

import {
  C2_MEM_FFI_ABI_VERSION,
  captureLocalIpcEndpointContext,
  createNodeIpcConnector,
  loadBundledC2MemFfiNodeNativeSymbols,
  loadC2MemFfiNodeNativeSymbols,
  resolveBundledC2MemFfiNodeNativeLibraryPath,
  resolveLocalIpcEndpoint,
} from '../dist/index.js';

const bundled = () => resolveBundledC2MemFfiNodeNativeLibraryPath();

function compileVersionOnly(directory, version) {
  const source = resolve(directory, 'version.rs');
  const library = resolve(directory, process.platform === 'win32' ? 'version.dll' :
    process.platform === 'darwin' ? 'version.dylib' : 'version.so');
  writeFileSync(source, `#[unsafe(no_mangle)] pub extern "C" fn c2_mem_ffi_abi_version() -> u32 { ${version} }`);
  const result = spawnSync(process.env.RUSTC ?? 'rustc',
    ['--edition=2024', '--crate-type=cdylib', source, '-o', library], { encoding: 'utf8' });
  assert.equal(result.status, 0, result.stderr);
  return library;
}

test('native endpoint resolution validates arguments and derives one OS endpoint', () => {
  assert.throws(() => resolveLocalIpcEndpoint('ipc://bad\0name'), /NUL/);
  assert.throws(() => resolveLocalIpcEndpoint('ipc://invalid-options', null), /options must be an object/);
  const address = `ipc://native-strict-${process.pid}`;
  const name = resolveLocalIpcEndpoint(address);
  assert.equal(name, resolveLocalIpcEndpoint(address));
  if (process.platform === 'win32') assert.match(name, /^\\\\\.\\pipe\\c_two-/);
  else assert.match(name, /^\/tmp\/c2-v3-[0-9a-f]+\/[0-9a-f]{32}$/);
  for (const invalid of ['/tmp/node.sock', 'ipc://../bad']) {
    assert.throws(() => resolveLocalIpcEndpoint(invalid));
  }
});

test('bundled ABI 8 loader resolves the packaged native library', () => {
  const path = bundled();
  assert.equal(existsSync(path), true, `${path} must exist; run npm run build:node-addon`);
  assert.match(path, /dist[\\/]native[\\/](?:libc2_mem_ffi\.(?:dylib|so)|c2_mem_ffi\.dll)$/);
  const { symbols } = loadBundledC2MemFfiNodeNativeSymbols();
  assert.equal(symbols.c2_mem_ffi_abi_version(), C2_MEM_FFI_ABI_VERSION);
  assert.equal(typeof symbols.captureEndpointContext, 'function');
  assert.equal(typeof symbols.createClientDomain, 'function');
});

test('native client domains validate factory policy before any endpoint exists', () => {
  const {symbols} = loadBundledC2MemFfiNodeNativeSymbols();
  const context = symbols.captureEndpointContext(null);
  try {
    for (const policy of [{requestChunkSize: '64'}, {requestShmThreshold: -1}, {requestChunkSize: 64, unknown: 1}]) {
      let domain;
      try {
        assert.throws(() => { domain = symbols.createClientDomain(context, policy); }, /requestChunkSize|requestShmThreshold|unknown/);
      } finally { domain?.close(); }
    }
  } finally { context.close(); }
});

test('native endpoints reject transport policy that belongs to their client domain', () => {
  const {symbols} = loadBundledC2MemFfiNodeNativeSymbols();
  const context = symbols.captureEndpointContext(null);
  const domain = symbols.createClientDomain(context, {requestChunkSize: 64, requestShmThreshold: 1024});
  try {
    for (const options of [{requestChunkSize: 128}, {requestShmThreshold: 2048}]) {
      assert.throws(() => domain.createEndpoint('ipc://factory-policy', options), /unknown option request/);
    }
  } finally { domain.close(); context.close(); }
});

test('captured native context freezes roots and env files without creating endpoint directories', {
  skip: process.platform === 'win32' && 'Unix roots are unsupported on Windows',
}, () => {
  const { symbols } = loadBundledC2MemFfiNodeNativeSymbols();
  const directory = mkdtempSync(resolve(tmpdir(), 'c2-abi8-snapshot-'));
  const envFile = resolve(directory, 'endpoint.env');
  const oldRoot = process.env.C2_IPC_ROOT;
  const oldFile = process.env.C2_ENV_FILE;
  const suffix = `${process.pid.toString(16)}${Date.now().toString(16).slice(-4)}`;
  const rootA = `/tmp/cna${suffix}`;
  const rootB = `/tmp/cnb${suffix}`;
  const address = 'ipc://node-context-snapshot';
  let context, explicit, fromFile;
  try {
    assert.equal(existsSync(rootA), false);
    assert.equal(existsSync(rootB), false);
    process.env.C2_ENV_FILE = '';
    process.env.C2_IPC_ROOT = rootA;
    context = symbols.captureEndpointContext(null);
    const nameA = context.endpointName(address);
    const namespaceA = context.namespaceId();
    assert.ok(nameA.startsWith(`${rootA}/`));
    process.env.C2_IPC_ROOT = rootB;
    assert.equal(context.endpointName(address), nameA);
    assert.equal(context.namespaceId(), namespaceA);
    assert.ok(resolveLocalIpcEndpoint(address).startsWith(`${rootB}/`));
    explicit = symbols.captureEndpointContext(rootA);
    assert.equal(explicit.endpointName(address), nameA);
    assert.equal(explicit.namespaceId(), namespaceA);

    process.env.C2_ENV_FILE = directory;
    const override = symbols.captureEndpointContext(rootA);
    try { assert.equal(override.endpointName(address), nameA); }
    finally { override.close(); }
    assert.throws(() => symbols.captureEndpointContext(null));

    delete process.env.C2_IPC_ROOT;
    process.env.C2_ENV_FILE = envFile;
    writeFileSync(envFile, `C2_IPC_ROOT=${rootA}\n`);
    fromFile = symbols.captureEndpointContext(null);
    writeFileSync(envFile, `C2_IPC_ROOT=${rootB}\n`);
    assert.equal(fromFile.endpointName(address), nameA);
    assert.ok(resolveLocalIpcEndpoint(address).startsWith(`${rootB}/`));
    for (const invalid of ['', 'relative', '/tmp/../bad']) {
      assert.throws(() => symbols.captureEndpointContext(invalid));
    }
    const longRoot = '/tmp/' + 'x'.repeat(200);
    const longContext = symbols.captureEndpointContext(longRoot);
    try {
      const derived = longContext.endpointName(address);
      assert.ok(derived.startsWith(`${longRoot}/`));
      assert.equal(derived.slice(longRoot.length + 1).length, 32);
      assert.equal(existsSync(longRoot), false);
    } finally { longContext.close(); }
    assert.equal(existsSync(rootA), false);
    assert.equal(existsSync(rootB), false);
  } finally {
    context?.close(); explicit?.close(); fromFile?.close();
    if (oldRoot === undefined) delete process.env.C2_IPC_ROOT;
    else process.env.C2_IPC_ROOT = oldRoot;
    if (oldFile === undefined) delete process.env.C2_ENV_FILE;
    else process.env.C2_ENV_FILE = oldFile;
    rmSync(directory, { recursive: true, force: true });
  }
});

test('native context rejects borrowed receivers and closes idempotently', () => {
  const { symbols } = loadBundledC2MemFfiNodeNativeSymbols();
  const context = symbols.captureEndpointContext(null);
  try {
    assert.throws(() => context.endpointName('/tmp/absolute.sock'));
    assert.throws(() => context.endpointName('ipc://bad\0name'), /NUL/);
    for (const method of ['endpointName', 'namespaceId', 'close']) {
      assert.throws(() => context[method].call({}, 'ipc://borrowed'), /Invalid native receiver/);
    }
    assert.throws(() => symbols.captureEndpointContext('bad\0root'), /NUL/);
    assert.throws(() => symbols.captureEndpointContext(123), /string or null/);
  } finally { context.close(); context.close(); }
  assert.throws(() => context.endpointName('ipc://closed'), /closed/);
  assert.throws(() => context.namespaceId(), /closed/);
});

test('one connector keeps its native context snapshot across endpoints and reconnects', {
  skip: process.platform === 'win32' && 'Unix env-root freeze; Windows capture is separately covered',
}, async () => {
  const oldRoot = process.env.C2_IPC_ROOT;
  const oldFile = process.env.C2_ENV_FILE;
  const rootA = `/tmp/c2a${process.pid.toString(16)}`;
  const rootB = `/tmp/c2b${process.pid.toString(16)}`;
  const address = 'ipc://same';
  let first, second;
  try {
    process.env.C2_ENV_FILE = '';
    process.env.C2_IPC_ROOT = rootA;
    const connect = createNodeIpcConnector();
    const before = connect.resolveEndpoint(address);
    process.env.C2_IPC_ROOT = rootB;
    assert.notEqual(connect.resolveEndpoint(address), before, 'queries before first connect are fresh');
    process.env.C2_IPC_ROOT = rootA;
    first = await connect(address);
    process.env.C2_IPC_ROOT = rootB;
    assert.equal(connect.resolveEndpoint(address), before);
    second = await connect('ipc://other');
    assert.ok(connect.resolveEndpoint('ipc://other').startsWith(`${rootA}/`));
    const fresh = createNodeIpcConnector();
    assert.ok(fresh.resolveEndpoint(address).startsWith(`${rootB}/`));
    await first.close(); await second.close();
    connect.close(); connect.close(); fresh.close();
    assert.throws(() => connect.resolveEndpoint(address), /closed/);
    assert.equal(existsSync(rootA), false);
    assert.equal(existsSync(rootB), false);
  } finally {
    if (oldRoot === undefined) delete process.env.C2_IPC_ROOT;
    else process.env.C2_IPC_ROOT = oldRoot;
    if (oldFile === undefined) delete process.env.C2_ENV_FILE;
    else process.env.C2_ENV_FILE = oldFile;
  }
});

test('captured public context remains usable by a connector after caller close', async () => {
  const context = captureLocalIpcEndpointContext();
  const connect = createNodeIpcConnector({ endpointContext: context });
  const address = 'ipc://retained-context';
  const name = context.endpointName(address);
  const endpoint = await connect(address);
  context.close();
  assert.equal(connect.resolveEndpoint(address), name);
  await endpoint.close();
  connect.close();
  assert.throws(() => connect.resolveEndpoint(address), /closed/);
});

test('Windows context retains current-SID pipe defaults and rejects Unix root overrides', {
  skip: process.platform !== 'win32' && 'Requires Windows',
}, () => {
  const { symbols } = loadBundledC2MemFfiNodeNativeSymbols();
  const context = symbols.captureEndpointContext(null);
  try {
    const name = context.endpointName('ipc://windows-context');
    assert.match(name, /^\\\\\.\\pipe\\c_two-/);
    assert.equal(name, resolveLocalIpcEndpoint('ipc://windows-context'));
    for (const root of ['C:\\endpoint-root', '\\\\.\\pipe\\arbitrary', '/tmp/root']) {
      assert.throws(() => symbols.captureEndpointContext(root));
    }
  } finally { context.close(); }
});

test('ABI 8 refuses an incomplete library after checking its version', () => {
  const directory = mkdtempSync(resolve(tmpdir(), 'c2-incomplete-abi8-'));
  try {
    const library = compileVersionOnly(directory, 8);
    assert.throws(() => loadC2MemFfiNodeNativeSymbols(library),
      /Failed to load c2-mem-ffi symbol c2_mem_ffi_local_endpoint_context_capture/);
  } finally { rmSync(directory, { recursive: true, force: true }); }
});

test('ABI 7 is refused before loading any ABI 8 symbol', () => {
  const directory = mkdtempSync(resolve(tmpdir(), 'c2-stale-abi7-'));
  try {
    const library = compileVersionOnly(directory, 7);
    assert.throws(() => loadC2MemFfiNodeNativeSymbols(library), /ABI version 7.*expected version 8/);
  } finally { rmSync(directory, { recursive: true, force: true }); }
});

test('loader accepts a native library in a Unicode directory with spaces', () => {
  const directory = mkdtempSync(resolve(tmpdir(), 'c2-内存 loader-'));
  const library = resolve(directory, process.platform === 'win32' ? 'memory.dll' : 'memory-library');
  copyFileSync(bundled(), library);
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

test('connector exposes a shared native domain and endpoint resolver', async () => {
  const connector = createNodeIpcConnector();
  const address = 'ipc://runtime-abi8';
  assert.equal(typeof connector, 'function');
  assert.equal(typeof connector.stats, 'function');
  assert.equal(connector.resolveEndpoint(address), resolveLocalIpcEndpoint(address));
  const endpoint = await connector(address);
  await endpoint.close();
  connector.close(); connector.close();
  assert.throws(() => connector.resolveEndpoint(address), /closed/);
});
