import assert from 'node:assert/strict';
import {spawnSync} from 'node:child_process';
import test from 'node:test';
import {
  C2_MEM_FFI_ABI_VERSION, C2NativeError, captureLocalIpcEndpointContextFromSymbols,
  createNodeIpcConnector, resolveLocalIpcEndpoint,
} from '../dist/index.js';

function fixture() {
  const events = [];
  let root = '/first';
  const nativeContext = {
    endpointName(address) { return `${root}/${address.slice(6)}`; },
    namespaceId() { return `namespace:${root}`; },
    close() { events.push('context.close'); },
  };
  const nativeCall = {
    chargeInput: async () => {}, dispatch: async () => {}, next: async () => null,
    cancel() {}, observe: async () => ({completed: true, error: null, completeItems: 0n}),
    checkDeadline() {}, close() {},
  };
  const nativeEndpoint = {
    prepare: async () => ({serverId: 's', serverInstanceId: 'i', connectionId: 1n}),
    beginCall: () => nativeCall, deactivate() {}, close: async () => {},
  };
  const domain = {
    createEndpoint(address, options) { events.push(['endpoint', address, options]); return nativeEndpoint; },
    stats() { return {connections: events.filter(e => Array.isArray(e) && e[0] === 'endpoint').length}; },
    close() { events.push('domain.close'); },
  };
  const symbols = {
    c2_mem_ffi_abi_version() { events.push('version'); return 8; },
    captureEndpointContext(ipcRoot) { events.push(['capture', ipcRoot]); root = ipcRoot ?? '/default'; return nativeContext; },
    createClientDomain(context, options) { events.push(['domain', context, options]); return domain; },
  };
  return {symbols, events, domain, nativeContext, nativeEndpoint, nativeCall};
}

test('ABI 8 connector retains one native domain across endpoint reconnects', async () => {
  const {symbols, events, nativeContext} = fixture();
  const connect = createNodeIpcConnector({nativeSymbols: symbols, ipcRoot: '/one', requestChunkSize: 128});
  assert.equal(connect.resolveEndpoint('ipc://first'), '/one/first');
  assert.equal(events.filter(e => Array.isArray(e) && e[0] === 'domain').length, 0, 'query does not freeze');
  const first = await connect('ipc://one', 250);
  const second = await connect('ipc://two', 500);
  assert.equal(typeof first.beginCall, 'function');
  assert.notEqual(first, second);
  assert.equal(events.filter(e => Array.isArray(e) && e[0] === 'domain').length, 1);
  assert.equal(events.find(e => Array.isArray(e) && e[0] === 'domain')[1], nativeContext);
  assert.deepEqual(events.find(e => Array.isArray(e) && e[0] === 'domain')[2], {requestChunkSize: 128});
  assert.deepEqual(events.filter(e => Array.isArray(e) && e[0] === 'endpoint').map(e => e.slice(1)), [
    ['ipc://one', {connectTimeoutMs: 250}],
    ['ipc://two', {connectTimeoutMs: 500}],
  ]);
  assert.deepEqual(connect.stats(), {connections: 2});
  connect.close(); connect.close();
  assert.equal(events.filter(e => e === 'domain.close').length, 1);
  assert.equal(events.filter(e => e === 'context.close').length, 2, 'the pure query and frozen snapshot each close');
  await assert.rejects(() => connect('ipc://three'), /closed/);
});

test('per-transport native options override factory defaults and explicit timeout wins', async () => {
  const {symbols, events} = fixture();
  const connect = createNodeIpcConnector({
    nativeSymbols: symbols, connectTimeoutMs: 100, requestChunkSize: 64,
    expectedServerIdentity: {serverId: 'factory', serverInstanceId: 'i'},
  });
  await connect('ipc://route', 300, {
    connectTimeoutMs: 200,
    expectedServerIdentity: {serverId: 'transport', serverInstanceId: 'i2'},
  });
  await connect('ipc://second', undefined, {connectTimeoutMs: 250});
  assert.deepEqual(events.filter(e => Array.isArray(e) && e[0] === 'endpoint').map(e => e[2]), [
    {connectTimeoutMs: 300,
      expectedServerIdentity: {serverId: 'transport', serverInstanceId: 'i2'}},
    {connectTimeoutMs: 250, expectedServerIdentity: {serverId: 'factory', serverInstanceId: 'i'}},
  ]);
  connect.close();
});

test('each preparation supplies only route and contract to the shared endpoint', async () => {
  const {symbols, events, nativeEndpoint} = fixture();
  const prepared = [];
  nativeEndpoint.prepare = async (...args) => {
    prepared.push(args);
    return {serverId: 's', serverInstanceId: 'i', connectionId: 1n};
  };
  const connect = createNodeIpcConnector({nativeSymbols: symbols});
  try {
    const endpoint = await connect('ipc://shared');
    const contract = {namespace: 'example'};
    await endpoint.prepare('one', contract);
    await endpoint.prepare('two', contract);
    assert.deepEqual(prepared, [
      ['one', contract],
      ['two', contract],
    ]);
    assert.deepEqual(events.find(e => Array.isArray(e) && e[0] === 'endpoint')[2], {});
  } finally { connect.close(); }
  assert.throws(() => createNodeIpcConnector({nativeSymbols: symbols, expectedRouteToken: {routeUid: 'old'}}), /unknown option/);
  const another = createNodeIpcConnector({nativeSymbols: symbols});
  try {
    await assert.rejects(another('ipc://shared', undefined, {expectedRouteToken: {routeUid: 'old'}}), /unknown option/);
  } finally { another.close(); }
});

test('factory transport policy is captured once before any endpoint can choose it', async () => {
  const {symbols, events} = fixture();
  const options = {nativeSymbols: symbols, requestShmThreshold: 1, requestChunkSize: 64};
  const connect = createNodeIpcConnector(options);
  options.requestShmThreshold = 4096;
  options.requestChunkSize = 128;
  try {
    const first = await connect('ipc://first');
    const second = await connect('ipc://second');
    await first.prepare('one', {});
    await second.prepare('two', {});
    const domains = events.filter(e => Array.isArray(e) && e[0] === 'domain');
    assert.equal(domains.length, 1);
    assert.deepEqual(domains[0][2], {requestShmThreshold: 1, requestChunkSize: 64});
    assert.deepEqual(events.filter(e => Array.isArray(e) && e[0] === 'endpoint').map(e => e[2]), [{}, {}]);
  } finally { connect.close(); }
});

test('connection deadlines reject non-positive or unsafe JS milliseconds before native capture', async () => {
  const {symbols, events} = fixture();
  for (const value of [0, -1, 0.1, NaN, Infinity, Number.MAX_SAFE_INTEGER + 1, null, '1']) {
    assert.throws(() => createNodeIpcConnector({nativeSymbols:symbols,connectTimeoutMs:value}), /positive safe integer/);
    const connector=createNodeIpcConnector({nativeSymbols:symbols});
    await assert.rejects(connector('ipc://x',value), /positive safe integer/);
    await assert.rejects(connector('ipc://x',undefined,{connectTimeoutMs:value}), /positive safe integer/);
    connector.close();
  }
  assert.equal(events.some(event => Array.isArray(event)&&event[0]==='capture'),false);
});

test('captured native configuration stays alive while a connector uses it', async () => {
  const {symbols, events} = fixture();
  const context = captureLocalIpcEndpointContextFromSymbols(symbols, {ipcRoot: '/shared'});
  assert.equal(context.namespaceId(), 'namespace:/shared');
  const connect = createNodeIpcConnector({nativeSymbols: symbols, endpointContext: context});
  await connect('ipc://first');
  context.close(); context.close();
  assert.equal(events.filter(e => e === 'context.close').length, 0);
  assert.equal(connect.resolveEndpoint('ipc://second'), '/shared/second');
  connect.close();
  assert.equal(events.filter(e => e === 'context.close').length, 1);
});

test('a typed context conflicts with an explicit root before native capture', () => {
  const {symbols,events}=fixture();
  const context=captureLocalIpcEndpointContextFromSymbols(symbols);
  const captures=events.filter(e=>Array.isArray(e)&&e[0]==='capture').length;
  try {
    assert.throws(()=>createNodeIpcConnector({nativeSymbols:symbols,endpointContext:context,ipcRoot:'/other'}),/mutually exclusive/);
    assert.throws(()=>resolveLocalIpcEndpoint('ipc://x',{nativeSymbols:symbols,endpointContext:context,ipcRoot:'/other'}),/mutually exclusive/);
    assert.equal(events.filter(e=>Array.isArray(e)&&e[0]==='capture').length,captures);
  } finally { context.close(); }
});

test('native resolver uses a temporary context and closes it', () => {
  const {symbols, events} = fixture();
  assert.equal(resolveLocalIpcEndpoint('ipc://x', {nativeSymbols: symbols, ipcRoot: '/query'}), '/query/x');
  assert.equal(events.filter(e => e === 'context.close').length, 1);
});

test('first capture failure is retained and a new connector can retry', async () => {
  const {symbols} = fixture();
  let captures = 0;
  symbols.captureEndpointContext = () => { captures++; throw new Error('capture failed'); };
  const connect = createNodeIpcConnector({nativeSymbols: symbols});
  await assert.rejects(() => connect('ipc://x'), /capture failed/);
  await assert.rejects(() => connect('ipc://x'), /capture failed/);
  assert.equal(captures, 1);
  connect.close();
});

test('invalid partial native owners close before the failure is reported', async () => {
  const {symbols} = fixture();
  let contextsClosed = 0;
  symbols.captureEndpointContext = () => ({close() { contextsClosed++; }});
  assert.throws(() => captureLocalIpcEndpointContextFromSymbols(symbols), /invalid owner/);
  assert.equal(contextsClosed, 1);
  const clean = fixture();
  let domainsClosed = 0;
  clean.symbols.createClientDomain = () => ({close() { domainsClosed++; }});
  const connect = createNodeIpcConnector({nativeSymbols: clean.symbols});
  await assert.rejects(() => connect('ipc://x'), /invalid owner/);
  assert.equal(domainsClosed, 1);
  connect.close();
});

test('ABI 7 is rejected before capture or domain creation', () => {
  const {symbols, events} = fixture();
  symbols.c2_mem_ffi_abi_version = () => 7;
  assert.equal(C2_MEM_FFI_ABI_VERSION, 8);
  assert.throws(() => createNodeIpcConnector({nativeSymbols: symbols}), /ABI version 7; expected version 8/);
  assert.equal(events.length, 0);
});

test('native errors retain structured details through endpoint, call and receipt', async () => {
  const {symbols, nativeEndpoint, nativeCall} = fixture();
  const details = {message: 'peer ended', code: 701, canonicalName: 'DispatchUncertain', transportPhase: 'dispatch_uncertain', details: {route: 'echo'}};
  const failure = Object.assign(new Error('peer ended'), {nativeDetails: details});
  nativeEndpoint.prepare = async () => { throw failure; };
  nativeCall.observe = async () => ({completed: false, error: failure, completeItems: 2n});
  const connect = createNodeIpcConnector({nativeSymbols: symbols});
  const endpoint = await connect('ipc://x');
  await assert.rejects(endpoint.prepare('echo', {}), error => error instanceof C2NativeError &&
    error.nativeDetails === details && error.code === 701 && error.canonicalName === 'DispatchUncertain' &&
    error.transportPhase === 'dispatch_uncertain' && error.details.route === 'echo');
  const receipt = await endpoint.beginCall('echo', 'steps', true, null).observe(0);
  assert.equal(receipt.completed, false);
  assert.equal(receipt.completeItems, 2n);
  assert.ok(receipt.error instanceof C2NativeError);
  assert.equal(receipt.error.nativeDetails.transportPhase, 'dispatch_uncertain');
  connect.close();
});

test('synchronous addon failures keep nativeDetails when projected as async methods', async () => {
  const {symbols, nativeCall} = fixture();
  const details = {message: 'deadline elapsed', canonicalName: 'CallDeadlineExceeded', transportPhase: 'pre_dispatch'};
  nativeCall.chargeInput = () => { throw Object.assign(new Error(details.message), {nativeDetails: details}); };
  const connect = createNodeIpcConnector({nativeSymbols: symbols});
  const call = (await connect('ipc://x')).beginCall('echo', 'echo', false, 0);
  await assert.rejects(() => call.chargeInput(1), error => error instanceof C2NativeError && error.nativeDetails === details);
  connect.close();
});

test('connector exposes native endpoint resolution and domain statistics', async () => {
  const {symbols} = fixture();
  const connector = createNodeIpcConnector({nativeSymbols: symbols});
  await connector('ipc://x');
  assert.deepEqual(connector.stats(), {connections: 1});
  assert.equal(connector.resolveEndpoint('ipc://x'), '/default/x');
  connector.close();
});


test('shared captured context releases after the last explicit or collected factory owner', () => {
  const script=`
    import assert from 'node:assert/strict';
    import {captureLocalIpcEndpointContextFromSymbols,createNodeIpcConnector} from ${JSON.stringify(new URL('../dist/index.js',import.meta.url).href)};
    let contexts=0,domains=0;
    const nativeContext={endpointName(){return 'endpoint'},namespaceId(){return 'namespace'},close(){contexts++}};
    const endpoint={async prepare(){},beginCall(){},deactivate(){},async close(){}};
    const symbols={c2_mem_ffi_abi_version(){return 8},captureEndpointContext(){return nativeContext},
      createClientDomain(){return {createEndpoint(){return endpoint},stats(){return {}},close(){domains++}}}};
    const context=captureLocalIpcEndpointContextFromSymbols(symbols);
    const a=createNodeIpcConnector({nativeSymbols:symbols,endpointContext:context});
    let b=createNodeIpcConnector({nativeSymbols:symbols,endpointContext:context});
    await a('ipc://one');await b('ipc://two');
    context.close();a.close();
    assert.equal(contexts,0);assert.equal(domains,1);
    let accessor=b.resolveEndpoint;
    b=null;
    for(let n=0;n<5;n++){global.gc();await new Promise(resolve=>setImmediate(resolve));}
    assert.equal(contexts,0,'a detached accessor retains the factory snapshot');
    assert.equal(accessor('ipc://still-live'),'endpoint');
    accessor=null;
    for(let n=0;n<100&&contexts===0;n++){global.gc();await new Promise(resolve=>setImmediate(resolve));}
    assert.equal(contexts,1);assert.equal(domains,2);
    context.close();a.close();global.gc();await new Promise(resolve=>setImmediate(resolve));
    assert.equal(contexts,1);assert.equal(domains,2);
  `;
  const result=spawnSync(process.execPath,['--expose-gc','--input-type=module','-e',script],{encoding:'utf8'});
  assert.equal(result.status,0,result.stdout+result.stderr);
});
