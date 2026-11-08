import assert from 'node:assert/strict';
import {spawnSync} from 'node:child_process';
import {mkdtempSync,writeFileSync,rmSync} from 'node:fs';
import {tmpdir} from 'node:os';
import {join} from 'node:path';
import {fileURLToPath,pathToFileURL} from 'node:url';
import test from 'node:test';

const directory=mkdtempSync(join(tmpdir(),'c2-ts-revision-'));
let transport;
try {
  writeFileSync(join(directory,'package.json'),'{"type":"module"}');
  const compiler=fileURLToPath(new URL('../../c2-mem-ffi/bindings/typescript/node_modules/typescript/bin/tsc',import.meta.url));
  const source=fileURLToPath(new URL('../assets/typescript_transport.ts',import.meta.url));
  const result=spawnSync(process.execPath,[compiler,'--strict','--target','ES2022','--module','ES2022','--moduleResolution','Bundler','--lib','ES2022,DOM','--rootDir',fileURLToPath(new URL('../assets',import.meta.url)),'--outDir',directory,'--typeRoots',join(directory,'node_modules','@types'),source],{encoding:'utf8'});
  assert.equal(result.status,0,result.stdout+result.stderr);
  transport=await import(pathToFileURL(join(directory,'typescript_transport.js')));
} finally {rmSync(directory,{recursive:true,force:true});}
const hash='a'.repeat(64);
const contract={schema:'c-two.contract.v2',namespace:'test',name:'Test',version:'0.1.0',descriptorSha256:hash,abiHash:hash,signatureHash:hash};
const binding={serverId:'s',serverInstanceId:'i',routeUid:'uid',connectionId:1n};
const receipt={completed:true,error:null,completeItems:0n};
const nextTurn=()=>new Promise(resolve=>setImmediate(resolve));
function deferred(){let resolve;const promise=new Promise(r=>{resolve=r});return {promise,resolve};}

test('unary starts its native deadline before encoding and never dispatches an expired input',async()=>{
  let clock=0,dispatched=0;
  const events=[];
  const endpoint={async prepare(){return binding},deactivate(){},async close(){},
    beginCall(_route,_method,stream,timeout){
      assert.equal(stream,false);assert.equal(timeout,10);events.push('begin');const deadline=clock+timeout;
      const checkDeadline=()=>{if(clock>=deadline){const error=new Error('encoding exhausted deadline');error.nativeDetails={message:error.message,code:715,canonicalName:'CallDeadlineExceeded',transportPhase:'pre_dispatch'};throw error}};
      return {checkDeadline,async chargeInput(){checkDeadline()},async dispatch(){checkDeadline();dispatched++},async next(){return null},cancel(){},close(){events.push('free')},async observe(){return receipt}};
    }};
  const client=transport.createIpcEncodedTransport('ipc://deadline',{connect:async()=>endpoint,callTimeoutMs:10});
  const encode=()=>{events.push('encode');clock+=11;return new Uint8Array()};
  await assert.rejects(client.call('route',contract,'ping',encode),error=>error.code===715&&error.transportPhase==='pre_dispatch'&&error.retrySafe===true);
  assert.deepEqual(events,['begin','encode','free']);assert.equal(dispatched,0);
  await client.close();
});

for(const maxAttempts of [2,3])test(`FallbackDenied retains first local IPC cause across two failed candidates (attempts=${maxAttempts})`,async()=>{
  const failures=['ConnectionRefused','ConnectionReset'].map(ioKind=>{
    const error=new Error(ioKind);error.nativeDetails={message:ioKind,transportPhase:'pre_dispatch',category:'ipc',ioOperation:'connect',ioKind};return error;
  });
  const routes=['a','b'].map((id,index)=>({name:'route',relay_url:`http://localhost:${8301+index}`,route_uid:id,ipc_address:`ipc://${id}`,server_id:id,server_instance_id:'i',crm_ns:'test',crm_name:'Test',crm_ver:'0.1.0',abi_hash:hash,signature_hash:hash,max_payload_size:100}));
  let posts=0;const attempts=[];
  const fetch=async(_url,init)=>{if(init.method==='POST')posts++;return {status:200,async text(){return JSON.stringify(routes)},async arrayBuffer(){return new ArrayBuffer(0)}}};
  const connect=async address=>({async prepare(){const index=address==='ipc://a'?0:1;attempts.push(index);throw failures[index]},beginCall(){throw Error('unexpected dispatch')},async close(){},deactivate(){}});
  const client=transport.createRelayAwareHttpEncodedTransport('http://localhost:8300',{fetch,ipc:{connect},maxAttempts});
  await assert.rejects(client.call('route',contract,'ping',()=>new Uint8Array()),error=>{
    assert.equal(typeof error.body,'string','both failed local tokens leave no independent fallback');
    assert.equal(JSON.parse(error.body).name,'FallbackDenied');
    assert.equal(error.cause.nativeDetails.ioKind,'ConnectionRefused');
    assert.equal(error.cause.cause,failures[0]);return true;
  });
  assert.deepEqual(attempts,[0,1]);assert.equal(posts,0);
});

for(const path of ['direct','relay-local'])test(`${path} close accepts 2147483648 ms while opening remains pending`,async()=>{
  const opened=deferred();let observes=0;
  const call={cancel(){},async chargeInput(){},async dispatch(){},async next(){return null},async observe(ms){observes++;assert.ok(ms>2147483000);return receipt},checkDeadline(){},close(){}};
  const endpoint={async prepare(){await opened.promise;return binding},beginCall(){return call},deactivate(){},async close(){}};
  const route={name:'route',relay_url:'http://localhost:8301',route_uid:'uid',ipc_address:'ipc://local',server_id:'s',server_instance_id:'i',crm_ns:'test',crm_name:'Test',crm_ver:'0.1.0',abi_hash:hash,signature_hash:hash,max_payload_size:100};
  const client=path==='direct'?transport.createIpcEncodedTransport('ipc://close',{connect:async()=>endpoint}):transport.createRelayAwareHttpEncodedTransport('http://localhost:8300',{ipc:{connect:async()=>endpoint},fetch:async()=>({status:200,async text(){return JSON.stringify([route])},async arrayBuffer(){return new ArrayBuffer(0)}})});
  let clock=0,id=0;const timers=new Map();
  const originalTimeout=globalThis.setTimeout,originalClear=globalThis.clearTimeout;
  const nowDescriptor=Object.getOwnPropertyDescriptor(performance,'now');
  Object.defineProperty(performance,'now',{configurable:true,value:()=>clock});
  // The test clock models Node's documented overflow clamp; advancing it is
  // deterministic and does not delay or order resource work with a sleep.
  globalThis.setTimeout=(callback,delay)=>{const handle=++id;timers.set(handle,{callback,due:clock+(delay>2147483647?1:delay)});return handle};
  globalThis.clearTimeout=handle=>{timers.delete(handle)};
  let close;
  try {
    const stream=client.stream('route',contract,'steps',()=>new Uint8Array(),x=>x);
    let settled=false;close=stream.close(2147483648).then(result=>{settled=true;return result});
    await nextTurn();
    clock=2;
    for(const [handle,timer] of timers){if(timer.due<=clock){timers.delete(handle);timer.callback()}}
    await nextTurn();
    assert.equal(settled,false,'close must not truncate a valid safe-integer budget');
    opened.resolve();assert.deepEqual(await close,receipt);assert.equal(observes,1);
  } finally {
    opened.resolve();await close;
    globalThis.setTimeout=originalTimeout;globalThis.clearTimeout=originalClear;
    if(nowDescriptor===undefined)delete performance.now;else Object.defineProperty(performance,'now',nowDescriptor);
    await client.close();
  }
});

test('close keeps its original budget across multiple timer intervals',async()=>{
  const opened=deferred();let clock=0,id=0;const timers=new Map();
  const originalTimeout=globalThis.setTimeout,originalClear=globalThis.clearTimeout;
  const nowDescriptor=Object.getOwnPropertyDescriptor(performance,'now');
  Object.defineProperty(performance,'now',{configurable:true,value:()=>clock});
  globalThis.setTimeout=(callback,delay)=>{const handle=++id;assert.ok(delay<=2147483647);timers.set(handle,{callback,due:clock+delay});return handle};
  globalThis.clearTimeout=handle=>{timers.delete(handle)};
  const call={cancel(){},async chargeInput(){},async dispatch(){},async next(){return null},async observe(){return receipt},checkDeadline(){},close(){}};
  const endpoint={async prepare(){await opened.promise;return binding},beginCall(){return call},deactivate(){},async close(){}};
  const client=transport.createIpcEncodedTransport('ipc://segments',{connect:async()=>endpoint});
  let stream;
  try {
    stream=client.stream('route',contract,'steps',()=>new Uint8Array(),x=>x);
    let settled=false;const close=stream.close(2147483648).then(result=>{settled=true;return result});
    await nextTurn();
    clock=2147483647;
    for(const [handle,timer]of [...timers])if(timer.due<=clock){timers.delete(handle);timer.callback()}
    await nextTurn();assert.equal(settled,false);assert.equal(timers.size,1);
    clock++;
    for(const [handle,timer]of [...timers])if(timer.due<=clock){timers.delete(handle);timer.callback()}
    assert.deepEqual(await close,{completed:false,error:null,completeItems:null});
  } finally {
    opened.resolve();await stream?.close(null);await client.close();
    globalThis.setTimeout=originalTimeout;globalThis.clearTimeout=originalClear;
    if(nowDescriptor===undefined)delete performance.now;else Object.defineProperty(performance,'now',nowDescriptor);
  }
});

for(const relayAware of [false,true])test(`HTTP unary encoding uses its original budget (relayAware=${relayAware})`,async()=>{
  let clock=0,encoded=0,posts=0,fetches=0;
  const descriptor=Object.getOwnPropertyDescriptor(performance,'now');
  Object.defineProperty(performance,'now',{configurable:true,value:()=>clock});
  const routes=[{name:'route',relay_url:'http://localhost:8301',route_uid:'uid',crm_ns:'test',crm_name:'Test',crm_ver:'0.1.0',abi_hash:hash,signature_hash:hash,max_payload_size:100}];
  const fetch=async(_url,init)=>{fetches++;if(init.method==='POST')posts++;return {status:200,async text(){return JSON.stringify(routes)},async arrayBuffer(){return new ArrayBuffer(0)}}};
  const factory=relayAware?transport.createRelayAwareHttpEncodedTransport:transport.createHttpRelayEncodedTransport;
  const client=factory('http://localhost:8300',{fetch,callTimeoutMs:10});
  try {
    await assert.rejects(client.call('route',contract,'ping',()=>{encoded++;clock+=11;return new Uint8Array()}),error=>error.code===715&&error.transportPhase==='pre_dispatch'&&error.retrySafe===true);
    assert.equal(encoded,1);assert.equal(posts,0);assert.equal(fetches,relayAware?1:0);
  } finally {
    if(descriptor===undefined)delete performance.now;else Object.defineProperty(performance,'now',descriptor);
    await client.close?.();
  }
});

for(const relayAware of [false,true])test(`HTTP pre-dispatch probes never restart the input budget (relayAware=${relayAware})`,async()=>{
  let clock=0,encoded=0,posts=0,probes=0;
  const descriptor=Object.getOwnPropertyDescriptor(performance,'now');
  Object.defineProperty(performance,'now',{configurable:true,value:()=>clock});
  const routes=[0,1].map(index=>({name:'route',relay_url:`http://localhost:${8301+index}`,route_uid:`uid-${index}`,crm_ns:'test',crm_name:'Test',crm_ver:'0.1.0',abi_hash:hash,signature_hash:hash,max_payload_size:100}));
  const fetch=async(url,init)=>{
    if(init.method==='POST'){posts++;throw Error('expired input must not dispatch')}
    if(url.includes('/_probe/')){
      probes++;clock+=probes===1?4:5;
      return {status:probes===1?409:200,async text(){return JSON.stringify({version:1,code:704,name:'RouteStale',message:'stale',details:{}})},async arrayBuffer(){return new ArrayBuffer(0)}};
    }
    clock+=2;return {status:200,async text(){return JSON.stringify(routes)},async arrayBuffer(){return new ArrayBuffer(0)}};
  };
  const factory=relayAware?transport.createRelayAwareHttpEncodedTransport:transport.createHttpRelayEncodedTransport;
  const client=factory('http://localhost:8300',{fetch,callTimeoutMs:10});
  try {
    await assert.rejects(client.call('route',contract,'ping',()=>{encoded++;clock+=2;return new Uint8Array()}),error=>error.code===715&&error.transportPhase==='pre_dispatch');
    assert.equal(encoded,1);assert.equal(probes,2);assert.equal(posts,0);
  } finally {
    if(descriptor===undefined)delete performance.now;else Object.defineProperty(performance,'now',descriptor);
    await client.close?.();
  }
});
