import assert from 'node:assert/strict';
import test from 'node:test';

import {
  createBundledC2MemFfiNodeRuntime,
  createNodeIpcConnect,
} from '../dist/index.js';

// The official Windows connector refuses every unverified socket source at
// construction time, before any factory call or wire byte. These shape checks
// are pure TypeScript, so they run on every platform through a temporary
// process.platform override.
function withWindowsPlatform(run) {
  const original = process.platform;
  Object.defineProperty(process, 'platform', { value: 'win32' });
  try {
    return run();
  } finally {
    Object.defineProperty(process, 'platform', { value: original });
  }
}

test('Windows connector rejects unverified socket sources at construction', () => {
  withWindowsPlatform(() => {
    const message = /requires the trusted native connector.*not accepted on Windows/;
    assert.throws(() => createNodeIpcConnect({ createConnection: () => assert.fail('must not open a socket') }), message);
    assert.throws(() => createNodeIpcConnect({ resolveEndpoint: () => '\\\\.\\pipe\\c2-forged' }), message);
    assert.throws(() => createNodeIpcConnect({ nativeSymbols: { c2_mem_ffi_abi_version: () => 6 } }), message);
    // Combined with otherwise-legal options the refusal still wins.
    assert.throws(() => createNodeIpcConnect({
      endpointContext: undefined,
      ipcRoot: 'ignored',
      resolveEndpoint: () => '\\\\.\\pipe\\c2-forged',
    }), message);
    // The public factory shares the same construction-time refusal, so a
    // generated-client configuration cannot smuggle an unverified socket.
    assert.throws(() => createBundledC2MemFfiNodeRuntime({ createConnection: () => assert.fail('must not open a socket') }), message);
    assert.throws(() => createBundledC2MemFfiNodeRuntime({ resolveEndpoint: () => '\\\\.\\pipe\\c2-forged' }), message);
    assert.throws(() => createBundledC2MemFfiNodeRuntime({ nativeSymbols: { c2_mem_ffi_abi_version: () => 6 } }), message);
  });
});

test('a missing Windows native runtime never falls back to unverified sockets', async () => {
  const connector = withWindowsPlatform(() => createNodeIpcConnect({ addonPath: './missing-c2-native-addon.node' }));
  await assert.rejects(() => withWindowsPlatform(() => connector('ipc://windows-runtime-missing')), (error) => {
    assert.match(error.message, /c2_mem_ffi\.dll|MODULE_NOT_FOUND|Cannot find/i);
    return true;
  });
  const runtime = withWindowsPlatform(() => createBundledC2MemFfiNodeRuntime({ addonPath: './missing-c2-native-addon.node' }));
  await assert.rejects(() => withWindowsPlatform(() => runtime.connect('ipc://windows-runtime-missing')), (error) => {
    assert.match(error.message, /c2_mem_ffi\.dll|MODULE_NOT_FOUND|Cannot find/i);
    return true;
  });
});

test('Unix platforms keep the injected connector surface', () => {
  if (process.platform === 'win32') return;
  const connect = createNodeIpcConnect({
    resolveEndpoint: () => '/tmp/c2-injected-socket',
    createConnection: () => { throw new Error('construction must not open sockets'); },
  });
  assert.equal(typeof connect, 'function');
  connect.close();
  // The public factory keeps the injected surface on Unix as well.
  const runtime = createBundledC2MemFfiNodeRuntime({
    resolveEndpoint: () => '/tmp/c2-injected-socket',
    createConnection: () => { throw new Error('construction must not open sockets'); },
  });
  assert.equal(typeof runtime.connect, 'function');
  runtime.close();
});

