import assert from 'node:assert/strict';
import test from 'node:test';
import {createNodeIpcConnector} from '../dist/index.js';

function withWindowsPlatform(run) {
  const original = process.platform;
  Object.defineProperty(process, 'platform', {value: 'win32'});
  try { return run(); }
  finally { Object.defineProperty(process, 'platform', {value: original}); }
}

test('Windows ABI 9 connector rejects raw socket and resolver injection', () => {
  withWindowsPlatform(() => {
    const symbols = {
      c2_mem_ffi_abi_version: () => 9,
      captureEndpointContext: () => ({endpointName: address => address, namespaceId: () => 'n', close() {}}),
      createClientDomain: () => ({createEndpoint() {}, stats() { return {}; }, close() {}}),
    };
    assert.throws(() => createNodeIpcConnector({nativeSymbols: symbols, createConnection() {}}), /unknown option createConnection/);
    assert.throws(() => createNodeIpcConnector({nativeSymbols: symbols, resolveEndpoint() {}}), /unknown option resolveEndpoint/);
  });
});

test('Windows ABI 9 rejects old native symbols before any endpoint context is captured', () => {
  withWindowsPlatform(() => {
    let captured = false;
    const symbols = {c2_mem_ffi_abi_version: () => 7, captureEndpointContext() { captured = true; }};
    assert.throws(() => createNodeIpcConnector({nativeSymbols: symbols}), /expected version 9/);
    assert.equal(captured, false);
  });
});
