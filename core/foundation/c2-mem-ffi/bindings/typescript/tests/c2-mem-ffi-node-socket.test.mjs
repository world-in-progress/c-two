import assert from 'node:assert/strict';
import test from 'node:test';
import * as binding from '../dist/index.js';

test('ABI 8 binding has no raw socket reader or socket injection', () => {
  assert.equal('createNodeIpcConnect' in binding, false);
  assert.equal('NodeIpcConnection' in binding, false);
  const symbols = {
    c2_mem_ffi_abi_version: () => 8,
    captureEndpointContext: () => ({endpointName: address => address, namespaceId: () => 'n', close() {}}),
    createClientDomain: () => ({createEndpoint() {}, stats() { return {}; }, close() {}}),
  };
  assert.throws(() => binding.createNodeIpcConnector({nativeSymbols: symbols, createConnection() {}}), /createConnection|unknown/i);
  assert.throws(() => binding.createNodeIpcConnector({nativeSymbols: symbols, resolveEndpoint() {}}), /resolveEndpoint|unknown/i);
});
