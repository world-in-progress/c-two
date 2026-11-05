import assert from 'node:assert/strict';
import test from 'node:test';
import {captureLocalIpcEndpointContextFromSymbols, createNodeIpcConnector} from '../dist/index.js';

test('captured endpoint context flows through native domain, prepare and call', async () => {
  const events = [];
  const nativeContext = {
    endpointName(address) { return `/private/${address.slice(6)}`; },
    namespaceId() { return 'private-namespace'; },
    close() { events.push('context.close'); },
  };
  const nativeCall = {
    async chargeInput(size) { events.push(['charge', size]); },
    async dispatch(bytes) { events.push(['dispatch', Array.from(bytes)]); },
    async next() { return Object.assign(new Uint8Array([9]), {release() { events.push('buffer.release'); }}); },
    cancel() {}, async observe() { return {completed: true, error: null, completeItems: 1n}; },
    checkDeadline() {}, close() { events.push('call.close'); },
  };
  const endpoint = {
    async prepare(route, contract) { events.push(['prepare', route, contract.abiHash]); return {serverId: 'server', serverInstanceId: 'instance', routeUid: 'route', connectionId: 1n}; },
    beginCall(route, method, serverStream) { events.push(['begin', route, method, serverStream]); return nativeCall; },
    deactivate() {}, async close() { events.push('endpoint.close'); },
  };
  const symbols = {
    c2_mem_ffi_abi_version: () => 8,
    captureEndpointContext: root => { events.push(['capture', root]); return nativeContext; },
    createClientDomain: context => {
      assert.equal(context, nativeContext);
      return {createEndpoint(address) { events.push(['endpoint', address]); return endpoint; }, stats: () => ({}), close() { events.push('domain.close'); }};
    },
  };
  const captured = captureLocalIpcEndpointContextFromSymbols(symbols, {ipcRoot: '/private'});
  const connector = createNodeIpcConnector({nativeSymbols: symbols, endpointContext: captured});
  const connection = await connector('ipc://echo');
  captured.close();
  assert.equal(connector.resolveEndpoint('ipc://echo'), '/private/echo');
  const contract = {abiHash: 'a'.repeat(64)};
  await connection.prepare('echo', contract);
  const call = connection.beginCall('echo', 'echo', false, 1000);
  await call.chargeInput(2);
  await call.dispatch(new Uint8Array([1, 2]));
  const response = await call.next();
  assert.deepEqual(Array.from(response), [9]);
  response.release();
  call.close(); await connection.close(); connector.close();
  assert.deepEqual(events, [
    ['capture', '/private'], ['endpoint', 'ipc://echo'], ['prepare', 'echo', contract.abiHash],
    ['begin', 'echo', 'echo', false], ['charge', 2], ['dispatch', [1, 2]],
    'buffer.release', 'call.close', 'endpoint.close', 'domain.close', 'context.close',
  ]);
});
