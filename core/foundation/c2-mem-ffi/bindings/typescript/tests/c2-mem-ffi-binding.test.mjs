import assert from 'node:assert/strict';
import { EventEmitter } from 'node:events';
import test from 'node:test';

import {
  C2_MEM_FFI_ABI_VERSION,
  C2_MEM_FFI_MAX_SHM_PREFIX_BYTES,
  C2MemFfiBindingError,
  captureLocalIpcEndpointContextFromSymbols,
  createNodeIpcConnect,
  resolveLocalIpcEndpoint,
} from '../dist/index.js';

function abiVersion() {
  return C2_MEM_FFI_ABI_VERSION;
}

class ContextTestSocket extends EventEmitter {
  destroyed = false;

  write(_bytes, callback) {
    callback?.();
    return true;
  }

  pause() { return this; }
  resume() { return this; }

  end() {
    this.emit('end');
    return this;
  }

  destroy() {
    if (!this.destroyed) {
      this.destroyed = true;
      this.emit('close');
    }
    return this;
  }
}

// These fixture strings test snapshot and ownership wiring, not Rust endpoint derivation.
function endpointSymbolsFixture() {
  let configuredRoot = 'configured-first';
  const contexts = [];
  const captureRoots = [];
  const atomicAddresses = [];
  const symbols = {
    c2_mem_ffi_abi_version: abiVersion,
    c2_mem_ffi_local_endpoint(address) {
      atomicAddresses.push(address);
      return { status: 0, value: `${configuredRoot}:${address}` };
    },
    c2_mem_ffi_local_endpoint_context_capture(root) {
      captureRoots.push(root);
      const context = {
        root: root ?? configuredRoot,
        closeCount: 0,
        addresses: [],
        endpointName(address) {
          assert.equal(this.closeCount, 0);
          this.addresses.push(address);
          return { status: 0, value: `${this.root}:${address}` };
        },
        namespaceId() {
          assert.equal(this.closeCount, 0);
          return { status: 0, value: `${this.root}:namespace` };
        },
        close() {
          this.closeCount += 1;
        },
      };
      contexts.push(context);
      return { status: 0, value: context };
    },
  };
  return {
    symbols,
    contexts,
    captureRoots,
    atomicAddresses,
    configureRoot(root) { configuredRoot = root; },
  };
}

function connectedContextSocket() {
  const socket = new ContextTestSocket();
  queueMicrotask(() => socket.emit('connect'));
  return socket;
}

test('endpoint context facade keeps the native snapshot and closes unused owners once', () => {
  const fixture = endpointSymbolsFixture();
  const context = captureLocalIpcEndpointContextFromSymbols(fixture.symbols);
  fixture.configureRoot('configured-later');
  assert.equal(context.endpointName('ipc://snapshot'), 'configured-first:ipc://snapshot');
  assert.equal(context.namespaceId(), 'configured-first:namespace');
  assert.deepEqual(fixture.captureRoots, [null]);
  assert.deepEqual(fixture.atomicAddresses, []);
  context.close();
  context.close();
  assert.equal(fixture.contexts[0].closeCount, 1);
  assert.throws(() => context.endpointName('ipc://snapshot'), /context is closed/);
  assert.throws(() => context.namespaceId(), /context is closed/);
});

test('Node connector captures one explicit-root snapshot and frees it on factory close', { skip: process.platform === 'win32' && 'Unix-only injection surface; the official Windows connector requires the trusted native entry' }, async () => {
  const fixture = endpointSymbolsFixture();
  const paths = [];
  const connect = createNodeIpcConnect({
    nativeSymbols: fixture.symbols,
    ipcRoot: 'explicit-root',
    createConnection(path) {
      paths.push(path);
      assert.equal(fixture.contexts.length, 1);
      fixture.configureRoot('changed-after-resolution');
      return connectedContextSocket();
    },
  });
  const first = await connect('ipc://same-name');
  const second = await connect('ipc://other-name');
  assert.deepEqual(paths, ['explicit-root:ipc://same-name', 'explicit-root:ipc://other-name']);
  assert.deepEqual(fixture.captureRoots, ['explicit-root']);
  assert.deepEqual(fixture.atomicAddresses, []);
  await first.close();
  await first.close();
  assert.equal(fixture.contexts[0].closeCount, 0);
  await second.write(new Uint8Array([1, 2]));
  await second.close();
  const third = await connect('ipc://same-name');
  assert.equal(connect.resolveEndpoint('ipc://query'), 'explicit-root:ipc://query');
  connect.close(); connect.close();
  assert.equal(fixture.contexts[0].closeCount, 1);
  // Names were copied before opening: snapshot disposal does not own sockets.
  await third.write(new Uint8Array([3]));
  await third.close();
  assert.throws(() => connect.resolveEndpoint('ipc://query'), /connector is closed/);
  await assert.rejects(() => connect('ipc://closed'), /connector is closed/);
});

test('Node connector queries do not freeze but first attempt fixes later addresses and reconnects', { skip: process.platform === 'win32' && 'Unix-only injection surface; the official Windows connector requires the trusted native entry' }, async () => {
  const fixture = endpointSymbolsFixture();
  const paths = [];
  const connect = createNodeIpcConnect({
    nativeSymbols: fixture.symbols,
    createConnection(path) { paths.push(path); return connectedContextSocket(); },
  });
  assert.equal(connect.resolveEndpoint('ipc://query'), 'configured-first:ipc://query');
  fixture.configureRoot('configured-at-connect');
  assert.equal(connect.resolveEndpoint('ipc://query'), 'configured-at-connect:ipc://query');
  assert.deepEqual(fixture.captureRoots, []);
  const first = await connect('ipc://same-name');
  fixture.configureRoot('configured-second');
  const second = await connect('ipc://other-name');
  await first.close();
  const third = await connect('ipc://same-name');
  assert.deepEqual(paths, ['configured-at-connect:ipc://same-name', 'configured-at-connect:ipc://other-name', 'configured-at-connect:ipc://same-name']);
  assert.equal(connect.resolveEndpoint('ipc://query'), 'configured-at-connect:ipc://query');
  assert.deepEqual(fixture.captureRoots, [null]);
  await second.close(); await third.close();
  assert.equal(fixture.contexts[0].closeCount, 0);
  connect.close();
  assert.equal(fixture.contexts[0].closeCount, 1);
});

test('typed context is reusable across factories and closes only after its last reference', { skip: process.platform === 'win32' && 'Unix-only injection surface; the official Windows connector requires the trusted native entry' }, async () => {
  const fixture = endpointSymbolsFixture();
  const context = captureLocalIpcEndpointContextFromSymbols(fixture.symbols);
  fixture.configureRoot('configured-later');
  const connect = createNodeIpcConnect({ endpointContext: context, createConnection: connectedContextSocket });
  const other = createNodeIpcConnect({ endpointContext: context, createConnection: connectedContextSocket });
  const [first, second, third] = await Promise.all([
    connect('ipc://first'), connect('ipc://second'), other('ipc://third'),
  ]);
  assert.equal(context.endpointName('ipc://query'), 'configured-first:ipc://query');
  await first.close();
  assert.equal(fixture.contexts[0].closeCount, 0);
  context.close(); context.close();
  assert.throws(() => context.endpointName('ipc://query'), /context is closed/);
  assert.equal(connect.resolveEndpoint('ipc://query'), 'configured-first:ipc://query');
  const fourth = await connect('ipc://fourth');
  connect.close(); connect.close();
  assert.equal(fixture.contexts[0].closeCount, 0);
  assert.equal(other.resolveEndpoint('ipc://query'), 'configured-first:ipc://query');
  other.close(); other.close();
  assert.equal(fixture.contexts[0].closeCount, 1);
  for (const connection of [second, third, fourth]) {
    await connection.write(new Uint8Array([4]));
    await connection.close();
  }
  const closedHandle = createNodeIpcConnect({ endpointContext: context });
  await assert.rejects(() => closedHandle('ipc://closed-owner'), /context is closed/);
  closedHandle.close();
});

test('socket end, error and close do not release the connector snapshot', { skip: process.platform === 'win32' && 'Unix-only injection surface; the official Windows connector requires the trusted native entry' }, async () => {
  for (const event of ['end', 'error', 'close']) {
    const fixture = endpointSymbolsFixture();
    const socket = connectedContextSocket();
    const connect = createNodeIpcConnect({ nativeSymbols: fixture.symbols, createConnection: () => socket });
    const connection = await connect('ipc://terminal');
    socket.emit(event, event === 'error' ? new Error('transport failure') : undefined);
    assert.equal(fixture.contexts[0].closeCount, 0, event);
    await connection.close();
    assert.equal(connect.resolveEndpoint('ipc://query'), 'configured-first:ipc://query');
    connect.close(); connect.close();
    assert.equal(fixture.contexts[0].closeCount, 1, event);
  }
});

test('query, socket-open and asynchronous failures retain the fixed factory snapshot until disposal', { skip: process.platform === 'win32' && 'Unix-only injection surface; the official Windows connector requires the trusted native entry' }, async () => {
  for (const failure of ['query', 'invalid-output', 'open', 'error', 'close', 'listener']) {
    const fixture = endpointSymbolsFixture();
    const capture = fixture.symbols.c2_mem_ffi_local_endpoint_context_capture;
    fixture.symbols.c2_mem_ffi_local_endpoint_context_capture = (root) => {
      const result = capture.call(fixture.symbols, root);
      if (failure === 'query') result.value.endpointName = () => ({ status: 2 });
      if (failure === 'invalid-output') result.value.endpointName = () => ({ status: 0, value: 'bad\0name' });
      return result;
    };
    let socket;
    const connect = createNodeIpcConnect({
      nativeSymbols: fixture.symbols,
      createConnection() {
        if (failure === 'open') throw new Error('open failed');
        socket = new ContextTestSocket();
        if (failure === 'listener') socket.once = () => { throw new Error('listener failed'); };
        else queueMicrotask(() => socket.emit(failure, failure === 'error' ? new Error('connect failed') : undefined));
        return socket;
      },
    });
    const expectedError = failure === 'close' ? /closed before connect completed/ : /failed|NUL/;
    await assert.rejects(() => connect('ipc://failure'), expectedError);
    fixture.configureRoot('configured-after-failure');
    await assert.rejects(() => connect('ipc://retry'), expectedError);
    assert.deepEqual(fixture.captureRoots, [null]);
    assert.equal(fixture.contexts[0].closeCount, 0, failure);
    if (socket !== undefined) assert.equal(socket.destroyed, true, failure);
    connect.close(); connect.close();
    assert.equal(fixture.contexts[0].closeCount, 1, failure);
  }
});

test('failed first open fixes the root for successful retries and a new factory can use another root', { skip: process.platform === 'win32' && 'Unix-only injection surface; the official Windows connector requires the trusted native entry' }, async () => {
  const fixture = endpointSymbolsFixture();
  const paths = [];
  const connect = createNodeIpcConnect({
    nativeSymbols: fixture.symbols,
    createConnection(path) {
      paths.push(path);
      if (paths.length === 1) throw new Error('first open failed');
      return connectedContextSocket();
    },
  });
  await assert.rejects(() => connect('ipc://same'), /first open failed/);
  fixture.configureRoot('configured-second');
  assert.equal(connect.resolveEndpoint('ipc://same'), 'configured-first:ipc://same');
  const retry = await connect('ipc://same');
  const fresh = createNodeIpcConnect({ nativeSymbols: fixture.symbols, createConnection: connectedContextSocket });
  const other = await fresh('ipc://same');
  assert.equal(fresh.resolveEndpoint('ipc://same'), 'configured-second:ipc://same');
  assert.deepEqual(paths, ['configured-first:ipc://same', 'configured-first:ipc://same']);
  await retry.close(); await other.close();
  connect.close(); fresh.close();
  assert.deepEqual(fixture.contexts.map((context) => context.closeCount), [1, 1]);
});

test('pure explicit-root queries use temporary contexts and close before first IO remains lazy', { skip: process.platform === 'win32' && 'Unix-only injection surface; the official Windows connector requires the trusted native entry' }, async () => {
  const fixture = endpointSymbolsFixture();
  const connect = createNodeIpcConnect({ nativeSymbols: fixture.symbols, ipcRoot: 'explicit-root' });
  assert.equal(connect.resolveEndpoint('ipc://query'), 'explicit-root:ipc://query');
  assert.equal(connect.resolveEndpoint('ipc://query'), 'explicit-root:ipc://query');
  assert.deepEqual(fixture.contexts.map((context) => context.closeCount), [1, 1]);
  connect.close(); connect.close();
  await assert.rejects(() => connect('ipc://closed'), /connector is closed/);
  assert.deepEqual(fixture.captureRoots, ['explicit-root', 'explicit-root']);
  const unused = createNodeIpcConnect({ nativeSymbols: fixture.symbols });
  unused.close();
  assert.equal(fixture.captureRoots.length, 2);
});

test('native capture failure is remembered after first attempt and partial owners close once', { skip: process.platform === 'win32' && 'Unix-only injection surface; the official Windows connector requires the trusted native entry' }, async () => {
  const fixture = endpointSymbolsFixture();
  let captures = 0, closes = 0;
  fixture.symbols.c2_mem_ffi_local_endpoint_context_capture = () => {
    captures += 1;
    return { status: 2, value: { close() { closes += 1; } } };
  };
  const connect = createNodeIpcConnect({ nativeSymbols: fixture.symbols });
  await assert.rejects(() => connect('ipc://first'), /capture failed/);
  fixture.configureRoot('configured-second');
  await assert.rejects(() => connect('ipc://retry'), /capture failed/);
  assert.throws(() => connect.resolveEndpoint('ipc://query'), /capture failed/);
  connect.close(); connect.close();
  assert.equal(captures, 1); assert.equal(closes, 1);
});

test('Node connector rejects missing context symbols before resolving or opening a socket', { skip: process.platform === 'win32' && 'Unix-only injection surface; the official Windows connector requires the trusted native entry' }, async () => {
  const fixture = endpointSymbolsFixture();
  delete fixture.symbols.c2_mem_ffi_local_endpoint_context_capture;
  let socketOpens = 0;
  const options = {
    nativeSymbols: fixture.symbols,
    createConnection() { socketOpens += 1; return connectedContextSocket(); },
  };
  const missingContext = /must provide c2_mem_ffi_local_endpoint_context_capture/;
  assert.throws(() => captureLocalIpcEndpointContextFromSymbols(fixture.symbols), missingContext);
  for (const ipcRoot of [undefined, 'requested-root']) {
    const connect = createNodeIpcConnect({ ...options, ipcRoot });
    await assert.rejects(() => connect('ipc://missing-context'), missingContext);
    connect.close();
  }
  assert.equal(socketOpens, 0);
  assert.deepEqual(fixture.atomicAddresses, []);
});

test('Node connector validates conflicts and native capture output before opening a socket', { skip: process.platform === 'win32' && 'Unix-only injection surface; the official Windows connector requires the trusted native entry' }, async () => {
  const fixture = endpointSymbolsFixture();
  const context = captureLocalIpcEndpointContextFromSymbols(fixture.symbols);
  for (const options of [
    { ipcRoot: 'root', endpointContext: context },
    { ipcRoot: 'root', resolveEndpoint: () => 'ignored' },
    { endpointContext: context, resolveEndpoint: () => 'ignored' },
    { nativeSymbols: fixture.symbols, resolveEndpoint: () => 'ignored' },
    { nativeSymbols: fixture.symbols, endpointContext: context },
  ]) {
    assert.throws(() => createNodeIpcConnect(options), /mutually exclusive|cannot be combined/);
  }
  for (const root of [null, 7, 'root\0suffix']) {
    assert.throws(() => createNodeIpcConnect({ ipcRoot: root }), /ipcRoot must be a string without NUL/);
    assert.throws(() => captureLocalIpcEndpointContextFromSymbols(fixture.symbols, { ipcRoot: root }), /ipcRoot must be a string without NUL/);
  }
  assert.throws(() => createNodeIpcConnect({ endpointContext: { endpointName() {} } }), /must be created by capture/);
  context.close();
  assert.equal(fixture.contexts[0].closeCount, 1);
  const malformedResults = [undefined, { status: 2 }, { status: 0 }, { status: 0, value: {} }];
  for (const result of malformedResults) {
    const symbols = { ...fixture.symbols, c2_mem_ffi_local_endpoint_context_capture: () => result };
    await assert.rejects(() => createNodeIpcConnect({ nativeSymbols: symbols, createConnection: () => assert.fail('must not open socket') })('ipc://invalid-capture'), /context capture failed/);
  }
  let closeCount = 0;
  for (const status of [0, 2]) {
    const symbols = {
      ...fixture.symbols,
      c2_mem_ffi_local_endpoint_context_capture: () => ({ status, value: { close() { closeCount += 1; } } }),
    };
    assert.throws(() => captureLocalIpcEndpointContextFromSymbols(symbols), /context capture failed/);
  }
  assert.equal(closeCount, 2);
  await assert.rejects(
    () => createNodeIpcConnect({ nativeSymbols: { ...fixture.symbols, c2_mem_ffi_abi_version: () => 2 } })('ipc://wrong-abi'),
    /ABI version 2.*expected version 7/,
  );
});

test('injected resolvers remain supported and invalid addresses do not consume shared contexts', { skip: process.platform === 'win32' && 'Unix-only injection surface; the official Windows connector requires the trusted native entry' }, async () => {
  let path;
  const injected = createNodeIpcConnect({
    resolveEndpoint: () => 'injected-endpoint',
    createConnection(value) { path = value; return connectedContextSocket(); },
  });
  const connection = await injected('ipc://injected');
  assert.equal(path, 'injected-endpoint');
  await connection.close(); injected.close();
  const fixture = endpointSymbolsFixture();
  const context = captureLocalIpcEndpointContextFromSymbols(fixture.symbols);
  const connect = createNodeIpcConnect({ endpointContext: context, createConnection: connectedContextSocket });
  await assert.rejects(() => connect('ipc://bad\0address'), /without NUL/);
  assert.equal(fixture.contexts[0].closeCount, 0);
  assert.deepEqual(fixture.contexts[0].addresses, []);
  assert.equal(context.namespaceId(), 'configured-first:namespace');
  const retry = await connect('ipc://valid');
  await retry.close(); connect.close();
  assert.equal(fixture.contexts[0].closeCount, 0);
  context.close();
  assert.equal(fixture.contexts[0].closeCount, 1);
});

test('createNodeIpcConnect rejects sockets that close before connect', { skip: process.platform === 'win32' && 'Unix-only injection surface; the official Windows connector requires the trusted native entry' }, async () => {
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
    () => createNodeIpcConnect({ createConnection: () => fakeSocket })('ipc://closed-before-connect'),
    /closed before connect completed/,
  );
});

test('ABI 6 endpoint symbols are rejected before capture or connect', () => {
  const fixture = endpointSymbolsFixture();
  fixture.symbols.c2_mem_ffi_abi_version = () => 6;
  assert.throws(() => captureLocalIpcEndpointContextFromSymbols(fixture.symbols), /ABI version 6.*expected version 7/);
  assert.deepEqual(fixture.captureRoots, []);
});
