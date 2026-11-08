import assert from 'node:assert/strict';
import { mkdtempSync, rmSync, writeFileSync } from 'node:fs';
import { tmpdir } from 'node:os';
import { join } from 'node:path';
import { fileURLToPath, pathToFileURL } from 'node:url';
import { spawnSync } from 'node:child_process';
import test from 'node:test';

const directory = mkdtempSync(join(tmpdir(), 'c2-relay-local-'));
let api;
try {
  writeFileSync(join(directory, 'package.json'), '{"type":"module"}');
  const source = fileURLToPath(new URL('../assets/typescript_transport.ts', import.meta.url));
  const compiler = fileURLToPath(new URL('../../c2-mem-ffi/bindings/typescript/node_modules/typescript/bin/tsc', import.meta.url));
  const compiled = spawnSync(process.execPath, [compiler, '--strict', '--exactOptionalPropertyTypes',
    '--target', 'ES2022', '--module', 'ES2022', '--moduleResolution', 'Bundler', '--lib', 'ES2022,DOM',
    '--outDir', directory, '--typeRoots', join(directory, 'types'), source], {encoding:'utf8'});
  assert.equal(compiled.status, 0, compiled.stdout + compiled.stderr);
  api = await import(pathToFileURL(join(directory, 'typescript_transport.js')));
} finally {
  rmSync(directory, {recursive:true, force:true});
}
const hash = 'a'.repeat(64);
const contract = {schema:'c-two.contract.v2',namespace:'test',name:'Echo',version:'0.1.0',descriptorSha256:hash,abiHash:hash,signatureHash:hash};
const nativeError = (code, canonicalName, details = {}) => new api.C2IpcTransportError(canonicalName, {
  message:canonicalName, code, canonicalName, details, transportPhase:'pre_dispatch', category:'ipc',
});
function client(options = {}) {
  const state = {connections:0, closed:[], prepared:[], calls:[], deactivated:false, resolvedUid:'original', generation:0};
  const transport = api.createRelayAwareHttpEncodedTransport('http://localhost:8300', {
    routeCacheTtlMs:0,
    async fetch(url, init) {
      assert.equal(init.method,'GET','a local failure must not POST to the same endpoint');
      const route = decodeURIComponent(new URL(url).pathname.split('/').pop());
      return {status:200, async text() {return JSON.stringify([{
        name:route,relay_url:'http://localhost:8300',route_uid:state.resolvedUid,
        ipc_address:'ipc://server',server_id:'s',server_instance_id:'i',
        crm_ns:contract.namespace,crm_name:contract.name,crm_ver:contract.version,
        abi_hash:hash,signature_hash:hash,max_payload_size:1024,
      }])}};
    },
    ipc:{async connect(_address,_timeout,nativeOptions) {
      const id = ++state.connections;
      assert.deepEqual(nativeOptions.expectedServerIdentity,{serverId:'s',serverInstanceId:'i'});
      assert.equal('expectedRouteToken' in nativeOptions,false);
      return {
        async prepare(...args) {
          assert.equal(args.length,2);
          const [route] = args;
          await options.onPrepare?.({id,route,state});
          if (state.deactivated) throw nativeError(707,'ResourceClosed');
          state.prepared.push({id,route});
          return {serverId:'s',serverInstanceId:'i',connectionId:String(id)};
        },
        beginCall(route) {return {
          async chargeInput(){}, async dispatch(){state.calls.push(route)},
          async next(){const value=Uint8Array.of(id + state.generation);value.release=()=>{};return value},
          checkDeadline(){},close(){},cancel(){},async observe(){return {completed:true,error:null,completeItems:1n}},
        }},
        deactivate(){state.deactivated=true},
        async close(){state.closed.push(id);await options.drain?.()},
      };
    }},
  });
  const call = route => transport.call(route,contract,'ping',()=>new Uint8Array());
  return {state,transport,call};
}

test('relay-local prepares late routes on one endpoint and closes it once', async () => {
  const {state,transport,call}=client();
  await Promise.all([call('manager'),call('builder')]);
  assert.equal(state.connections,1);
  assert.deepEqual(state.prepared.map(x=>x.route).sort(),['builder','builder','manager','manager']);
  assert.deepEqual(state.calls.sort(),['builder','manager']);
  assert.deepEqual(state.closed,[]);
  await Promise.all([transport.close(),transport.close()]);
  assert.deepEqual(state.closed,[1]);
  await assert.rejects(call('manager'),/deactivated/);
});

test('an older resolved UID still prepares by name on the healthy carrier', async () => {
  const {state,transport,call}=client();
  await call('manager');
  state.resolvedUid='older-builder';
  await call('builder');
  await call('manager');
  assert.equal(state.connections,1);
  assert.deepEqual(state.calls,['manager','builder','manager']);
  await transport.close();
});

test('a deactivated endpoint is discarded without waiting for unrelated work', async () => {
  let drained;
  const gate=new Promise(resolve=>{drained=resolve});
  const {state,transport,call}=client({drain:()=>gate});
  await call('manager');
  state.deactivated=true;
  await assert.rejects(call('manager'),error=>JSON.parse(error.body).code===714 && error.cause.code===707);
  state.deactivated=false;
  await call('manager');
  assert.equal(state.connections,2);
  let completed=false;
  const closed=transport.close().then(()=>{completed=true});
  await new Promise(resolve=>setImmediate(resolve));
  assert.equal(completed,false);
  drained();
  await closed;
  assert.deepEqual(state.closed,[1,2]);
});


test('same-endpoint replacement reaches the current registration without retiring its carrier', async () => {
  const {state,transport,call}=client();
  assert.deepEqual([...await call('builder')],[1]);
  state.resolvedUid='replacement';
  state.generation=1;
  assert.deepEqual([...await call('builder')],[2]);
  await call('manager');
  assert.equal(state.connections,1);
  assert.deepEqual(state.closed,[]);
  await transport.close();
});

test('stream close retires an endpoint when its asynchronous preparation fails', async () => {
  let entered, release;
  const started=new Promise(resolve=>{entered=resolve});
  const gate=new Promise(resolve=>{release=resolve});
  let prepares=0;
  const {state,transport,call}=client({async onPrepare() {
    if (++prepares === 2) {entered();await gate;throw nativeError(707,'ResourceClosed')}
  }});
  const stream=transport.stream('manager',contract,'steps',()=>new Uint8Array(),bytes=>bytes);
  await started;
  const closed=stream.close(null);
  release();
  await assert.rejects(closed,error=>error.code===707);
  await call('manager');
  assert.equal(state.connections,2);
  await transport.close();
});
