import assert from 'node:assert/strict';
import { existsSync, mkdtempSync, rmSync, writeFileSync } from 'node:fs';
import { tmpdir } from 'node:os';
import { join } from 'node:path';
import { fileURLToPath, pathToFileURL } from 'node:url';
import { spawnSync } from 'node:child_process';
import test from 'node:test';

async function transportModule() {
  const directory = mkdtempSync(join(tmpdir(), 'c2-consume-'));
  try {
    const source = fileURLToPath(new URL('../assets/typescript_transport.ts', import.meta.url));
    const localCompiler = fileURLToPath(new URL('../../c2-mem-ffi/bindings/typescript/node_modules/typescript/bin/tsc', import.meta.url));
    const compiler = existsSync(localCompiler) ? process.execPath : process.env.C2_TYPESCRIPT_TSC ?? 'tsc';
    writeFileSync(join(directory, 'package.json'), '{"type":"module"}');
    const compile = spawnSync(compiler, [
      ...(existsSync(localCompiler) ? [localCompiler] : []),
      '--strict', '--exactOptionalPropertyTypes', '--target', 'ES2022', '--module', 'ES2022',
      '--moduleResolution', 'Bundler', '--lib', 'ES2022,DOM',
      '--rootDir', fileURLToPath(new URL('../assets/', import.meta.url)),
      '--outDir', directory, '--typeRoots', join(directory, 'node_modules', '@types'), source,
    ], {encoding:'utf8'});
    assert.equal(compile.status, 0, compile.stdout + compile.stderr);
    const location = join(directory, 'typescript_transport.js');
    return await import(pathToFileURL(location));
  } finally {
    rmSync(directory, {recursive: true, force: true});
  }
}
const transport = await transportModule();
const hash = 'a'.repeat(64);
const contract = {schema:'c-two.contract.v2', namespace:'test', name:'Echo', version:'0.1.0', descriptorSha256:hash, abiHash:hash, signatureHash:hash};

function nativeEndpoint(next, prepare = async () => ({serverId:'s',serverInstanceId:'i',connectionId:1n})) {
  return {
    prepare,
    beginCall() { return {
      async chargeInput() {}, async dispatch() {}, next, cancel() {},
      async observe() {return {completed:true,error:null,completeItems:1n}},
      checkDeadline() {}, close() {},
    }; },
    deactivate() {}, async close() {},
  };
}

test('provider allocation copies one native local buffer and releases its owner', async () => {
  let releases = 0;
  const response = Uint8Array.of(4,5,6);
  response.release = () => { releases++; };
  const payload = {byteLength:3, release() {}};
  const endpoint = nativeEndpoint(async () => response);
  const client = transport.createIpcEncodedTransport('ipc://consumer', {
    connect: async () => endpoint,
    responsePayloadAllocator(length) {return {view:new Uint8Array(length),payload};},
  });
  const result = await client.call('route', contract, 'echo', () => Uint8Array.of(1));
  assert.equal(result, payload);
  assert.equal(releases, 1);
  await client.close();
});

test('IPC observation exposes route and server identity without a route UID', async () => {
  const observations = [];
  const response = Uint8Array.of(4); response.release = () => {};
  const client = transport.createIpcEncodedTransport('ipc://consumer', {
    connect: async () => nativeEndpoint(async () => response),
    observe: value => observations.push(value),
  });
  await client.call('route', contract, 'echo', () => Uint8Array.of(1));
  assert.equal(observations.length, 1);
  assert.deepEqual(Object.keys(observations[0]).sort(),
    ['ipcAddress', 'path', 'requests', 'responses', 'routeName', 'serverId', 'serverInstanceId']);
  await client.close();
});

test('HTTP observation omits the internal route UID', async () => {
  const observations = [];
  const route = {name:'route',relay_url:'http://localhost:8301',route_uid:'internal',
    crm_ns:contract.namespace,crm_name:contract.name,crm_ver:contract.version,
    abi_hash:hash,signature_hash:hash,max_payload_size:100};
  const fetch = async (url, init) => {
    if (url.includes('/_resolve/')) return {status:200,async text(){return JSON.stringify([route])}};
    if (url.includes('/_probe/')) return {status:200,async text(){return ''}};
    assert.equal(init.method, 'POST');
    assert.equal(init.headers['x-c2-route-uid'], 'internal');
    return {status:200,async arrayBuffer(){return Uint8Array.of(4).buffer}};
  };
  const client = transport.createHttpRelayEncodedTransport('http://localhost:8300', {
    fetch, observe: value => observations.push(value),
  });
  await client.call('route', contract, 'echo', () => Uint8Array.of(1));
  assert.equal(observations.length, 1);
  assert.deepEqual(Object.keys(observations[0]).sort(),
    ['path', 'relayUrl', 'requests', 'responses', 'routeName']);
});

test('native pre-dispatch facts remain typed and retry-safe', async () => {
  const native = new Error('route removed');
  native.nativeDetails = {message:'route removed',code:708,canonicalName:'ResourceRemoved',transportPhase:'pre_dispatch',details:{route:'route'}};
  const endpoint = nativeEndpoint(async () => null, async () => {throw native});
  const client = transport.createIpcEncodedTransport('ipc://consumer', {connect:async()=>endpoint});
  await assert.rejects(client.prepare('route',contract), error => {
    assert.ok(error instanceof transport.C2IpcTransportError);
    assert.equal(error.nativeDetails.code,708);
    assert.equal(error.transportPhase,'pre_dispatch');
    assert.equal(error.retrySafe,true);
    assert.deepEqual(error.details,{route:'route'});
    return true;
  });
  await client.close();
});


test('provider allocator throw releases the native response and leaves the endpoint usable', async () => {
  let releases=0, attempts=0;
  const endpoint=nativeEndpoint(async()=>{
    const response=Uint8Array.of(1,2,3);response.release=()=>{releases++};return response;
  });
  const failure=new Error('provider allocation failed');
  const client=transport.createIpcEncodedTransport('ipc://consumer',{connect:async()=>endpoint,
    responsePayloadAllocator(length){if(attempts++===0)throw failure;return {view:new Uint8Array(length),payload:new Uint8Array(length)}}});
  await assert.rejects(client.call('route',contract,'echo',()=>new Uint8Array()),error=>error===failure);
  assert.equal(releases,1);
  await client.call('route',contract,'echo',()=>new Uint8Array());
  assert.equal(releases,2);
  await client.close();
});

test('provider copy failure releases both local owners and preserves the original error', async () => {
  let nativeReleases=0,providerReleases=0;
  const endpoint=nativeEndpoint(async()=>{
    const response=Uint8Array.of(1,2,3);response.release=()=>{nativeReleases++};return response;
  });
  const failure=new Error('provider copy failed');
  const client=transport.createIpcEncodedTransport('ipc://consumer',{connect:async()=>endpoint,
    responsePayloadAllocator(length){const view=new Uint8Array(length);view.set=()=>{throw failure};return {view,payload:view,release(){providerReleases++}}}});
  await assert.rejects(client.call('route',contract,'echo',()=>new Uint8Array()),error=>error===failure);
  assert.equal(nativeReleases,1);assert.equal(providerReleases,1);
  await client.close();
});
