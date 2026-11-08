import assert from 'node:assert/strict';
import test from 'node:test';
import {createNodeIpcConnector} from '../dist/index.js';

function fixture() {
  const events = [];
  const call = {
    async chargeInput(size) { events.push(['charge', size]); },
    async dispatch(bytes) { events.push(['dispatch', bytes]); },
    async next() { events.push('next'); return null; },
    cancel() { events.push('cancel'); },
    async observe(timeout) { events.push(['observe', timeout]); return {completed: true, error: null, completeItems: 0n}; },
    checkDeadline() { events.push('deadline'); },
    close() { events.push('call.close'); },
  };
  const endpoint = {
    async prepare(route, contract) { events.push(['prepare', route, contract]); return {serverId: 'server', serverInstanceId: 'instance', connectionId: 7n}; },
    beginCall(route, method, stream, timeout) { events.push(['begin', route, method, stream, timeout]); return call; },
    deactivate() { events.push('deactivate'); },
    async close() { events.push('endpoint.close'); },
  };
  const symbols = {
    c2_mem_ffi_abi_version: () => 8,
    captureEndpointContext: () => ({endpointName: address => address, namespaceId: () => 'n', close() {}}),
    createClientDomain: () => ({createEndpoint: () => endpoint, stats: () => ({}), close() {}}),
  };
  return {events, endpoint, call, symbols};
}

test('endpoint and call methods pass through route, method and deadline exactly', async () => {
  const {events, symbols} = fixture();
  const connector = createNodeIpcConnector({nativeSymbols: symbols});
  const endpoint = await connector('ipc://x');
  const contract = {abiHash: 'a'};
  assert.deepEqual(await endpoint.prepare('echo', contract), {serverId: 'server', serverInstanceId: 'instance', connectionId: 7n});
  const call = endpoint.beginCall('echo', 'steps', true, null);
  await call.chargeInput(3n);
  const input = new Uint8Array([1, 2, 3]);
  await call.dispatch(input);
  assert.equal(await call.next(), null);
  assert.deepEqual(await call.observe(0), {completed: true, error: null, completeItems: 0n});
  call.checkDeadline(); call.cancel(); call.close(); endpoint.deactivate(); await endpoint.close(); connector.close();
  assert.deepEqual(events, [
    ['prepare', 'echo', contract], ['begin', 'echo', 'steps', true, null], ['charge', 3n],
    ['dispatch', input], 'next', ['observe', 0], 'deadline', 'cancel', 'call.close', 'deactivate', 'endpoint.close',
  ]);
});

test('nativeBytes response remains one native buffer and releases its owner', async () => {
  const {call, symbols} = fixture();
  let released = 0;
  const buffer = Object.assign(new Uint8Array([4, 5, 6]), {release() { released++; }});
  call.next = async () => buffer;
  const connector = createNodeIpcConnector({nativeSymbols: symbols});
  const response = await (await connector('ipc://x')).beginCall('echo', 'echo', false, null).next();
  assert.equal(response, buffer, 'binding does not copy the native local owner');
  assert.deepEqual(Array.from(response), [4, 5, 6]);
  response.release();
  assert.equal(released, 1);
  connector.close();
});
