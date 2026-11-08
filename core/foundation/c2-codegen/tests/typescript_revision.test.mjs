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
const binding={serverId:'s',serverInstanceId:'i',connectionId:1n};
const receipt={completed:true,error:null,completeItems:0n};
const nativeTarget={...binding,path:'DirectIpc'};
const nextTurn=()=>new Promise(resolve=>setImmediate(resolve));
function deferred(){let resolve;const promise=new Promise(r=>{resolve=r});return {promise,resolve};}

test('719 resolve is typed, pre-dispatch, and a later acquisition can succeed',async()=>{
  const registrations='[{"relay_id":"a","relay_url":"http://localhost:8301","registered_at":1,"relay_status":"Alive"}]';
  const details={route:'route',dispatch_phase:'pre_dispatch',registrations};
  const route={name:'route',relay_url:'http://localhost:8301',crm_ns:'test',crm_name:'Test',crm_ver:'0.1.0',abi_hash:hash,signature_hash:hash,max_payload_size:100};
  let resolves=0,probes=0,posts=0;
  const fetch=async(url)=>{
    if(url.includes('/_resolve/')){
      resolves++;
      return resolves===1
        ? {status:409,async text(){return JSON.stringify({version:1,code:719,name:'RouteConflict',message:'conflict',details})}}
        : {status:200,async text(){return JSON.stringify([route])}};
    }
    if(url.includes('/_probe/')){probes++;return {status:200,async text(){return ''}}}
    posts++;return {status:200,async arrayBuffer(){return Uint8Array.of(8).buffer}};
  };
  const client=transport.createRelayAwareHttpEncodedTransport('http://localhost:8300',{fetch});
  try{
    await assert.rejects(client.call('route',contract,'ping',()=>new Uint8Array()),error=>{
      assert.ok(error instanceof transport.C2RouteConflictError);
      assert.ok(error instanceof transport.C2CoreError);
      assert.equal(error.code,719);
      assert.equal(error.canonicalName,'RouteConflict');
      assert.equal(error.transportPhase,'pre_dispatch');
      assert.equal(error.retrySafe,true);
      assert.deepEqual(error.details,details);
      return true;
    });
    assert.deepEqual([resolves,probes,posts],[1,0,0]);
    assert.equal((await client.call('route',contract,'ping',()=>new Uint8Array()))[0],8);
    assert.deepEqual([resolves,probes,posts],[2,1,1]);
  }finally{await client.close()}
});

test('719 resolution parses a complete canonical error once',async()=>{
  const details={route:'route',dispatch_phase:'pre_dispatch',registrations:'[]'};
  const body=JSON.stringify({version:1,code:719,name:'RouteConflict',message:'conflict',details});
  const client=transport.createRelayAwareHttpEncodedTransport('http://localhost:8300',{
    fetch:async()=>({status:409,async text(){return body}}),
  });
  const originalParse=JSON.parse;
  let parses=0;
  JSON.parse=function(...args){parses++;return originalParse.apply(this,args)};
  try{
    await assert.rejects(client.call('route',contract,'ping',()=>new Uint8Array()),error=>{
      assert.ok(error instanceof transport.C2RouteConflictError);
      assert.equal(error.message,'conflict');
      assert.deepEqual(error.details,details);
      return true;
    });
    assert.equal(parses,1);
  }finally{JSON.parse=originalParse;await client.close()}
});

test('719 with invalid canonical fields keeps the HTTP relay error',async()=>{
  const details={route:'route',dispatch_phase:'pre_dispatch',registrations:'[]'};
  for(const invalid of [
    {message:undefined,details},
    {message:719,details},
    {message:'conflict',details:{...details,registrations:719}},
  ]){
    const body=JSON.stringify({version:1,code:719,name:'RouteConflict',...invalid});
    const client=transport.createRelayAwareHttpEncodedTransport('http://localhost:8300',{
      fetch:async()=>({status:409,async text(){return body}}),
    });
    try{
      await assert.rejects(client.call('route',contract,'ping',()=>new Uint8Array()),error=>{
        assert.ok(error instanceof transport.C2HttpRelayError);
        assert.equal(error instanceof transport.C2RouteConflictError,false);
        assert.equal(error.status,409);
        assert.equal(error.body,body);
        assert.equal(error.retrySafe,false);
        return true;
      });
    }finally{await client.close()}
  }
});

test('native 719 projection has the same typed error',async()=>{
  const details={route:'route',dispatch_phase:'pre_dispatch',registrations:'[]'};
  const nativeDetails={message:'conflict',code:719,canonicalName:'RouteConflict',details,transportPhase:'pre_dispatch'};
  const plain=new Error('conflict');plain.nativeDetails=nativeDetails;
  for(const native of [plain,new transport.C2IpcTransportError('conflict',nativeDetails)]){
    const client=transport.createIpcEncodedTransport('ipc://route',{connect:async()=>({
      connectionSnapshot(){return null},async prepare(){throw native},deactivate(){},async close(){},
    })});
    try{
      await assert.rejects(client.call('route',contract,'ping',()=>new Uint8Array()),error=>{
        assert.ok(error instanceof transport.C2RouteConflictError);
        assert.ok(error instanceof transport.C2CoreError);
        assert.equal(error.code,719);
        assert.equal(error.canonicalName,'RouteConflict');
        assert.equal(error.transportPhase,'pre_dispatch');
        assert.equal(error.retrySafe,true);
        assert.deepEqual(error.details,details);
        return true;
      });
    }finally{await client.close()}
  }
});

test('719 probe stops acquisition before trying another registration',async()=>{
  const details={route:'route',dispatch_phase:'pre_dispatch',registrations:'[]'};
  const routes=[8301,8302].map(port=>({name:'route',relay_url:`http://localhost:${port}`,crm_ns:'test',crm_name:'Test',crm_ver:'0.1.0',abi_hash:hash,signature_hash:hash,max_payload_size:100}));
  let resolves=0,probes=0,posts=0;
  const fetch=async(url)=>{
    if(url.includes('/_resolve/')){resolves++;return {status:200,async text(){return JSON.stringify(routes)}}}
    if(url.includes('/_probe/')){
      probes++;
      return {status:409,async text(){return JSON.stringify({version:1,code:719,name:'RouteConflict',message:'conflict',details})}};
    }
    posts++;throw Error('unexpected POST');
  };
  const client=transport.createRelayAwareHttpEncodedTransport('http://localhost:8300',{fetch});
  try{
    await assert.rejects(client.call('route',contract,'ping',()=>new Uint8Array()),error=>error instanceof transport.C2RouteConflictError && error.retrySafe);
    assert.deepEqual([resolves,probes,posts],[1,1,0]);
  }finally{await client.close()}
});

test('719 without a pre-dispatch proof preserves the native failure phase',async()=>{
  for(const transportPhase of [undefined,'dispatch_uncertain']){
    const native=new Error('resource propagated a conflict');
    native.nativeDetails={message:native.message,code:719,canonicalName:'RouteConflict',details:{route:'route'},transportPhase};
    const client=transport.createIpcEncodedTransport('ipc://route',{connect:async()=>({
      connectionSnapshot(){return null},async prepare(){throw native},deactivate(){},async close(){},
    })});
    try{
      await assert.rejects(client.call('route',contract,'ping',()=>new Uint8Array()),error=>{
        assert.equal(error instanceof transport.C2RouteConflictError,false);
        assert.equal(error.code,719);
        assert.equal(error.retrySafe,false);
        assert.equal(error.transportPhase,transportPhase??null);
        return true;
      });
    }finally{await client.close()}
  }
});

for (const relayLocal of [false, true]) for (const stream of [false, true]) {
  test(`expired preparation observes a late native rejection (relayLocal=${relayLocal}, stream=${stream})`, async () => {
    let clock = 0, rejectPreparation, preparations = 0, encoded = 0, begun = 0;
    const late = new Promise((_resolve, reject) => { rejectPreparation = reject; });
    const failure = new Error('connect deadline exceeded');
    failure.nativeDetails = {message: failure.message, code: 715, canonicalName: 'CallDeadlineExceeded', transportPhase: 'pre_dispatch'};
    const descriptor = Object.getOwnPropertyDescriptor(performance, 'now');
    Object.defineProperty(performance, 'now', {configurable: true, value: () => clock});
    const endpoint = {
      connectionSnapshot() { return null; },
      prepare(_route, _contract, timeout) {
        assert.equal(timeout, 10);
        preparations++;
        clock = 11; // Native setup exhausts the budget before returning its Promise.
        return late;
      },
      beginCall() { begun++; throw new Error('expired preparation must not begin a call'); },
      deactivate() {}, async close() {},
    };
    const client = relayLocal
      ? transport.createRelayAwareHttpEncodedTransport('http://localhost:8300', {
          callTimeoutMs: 10, ipc: {connect: async () => endpoint},
          fetch: async url => {
            assert.ok(url.includes('/_resolve/'));
            return {status: 200, async text() { return JSON.stringify([relayRoute(1, true)]); }};
          },
        })
      : transport.createIpcEncodedTransport('ipc://deadline', {connect: async () => endpoint, callTimeoutMs: 10});
    let response;
    try {
      const encode = () => { encoded++; return new Uint8Array(); };
      response = stream ? client.stream('route', contract, 'steps', encode, value => value) : undefined;
      const pending = stream ? response.next() : client.call('route', contract, 'ping', encode);
      await assert.rejects(pending, error => {
        assert.equal(error.message, 'call deadline exceeded');
        assert.equal(error.code, 715);
        assert.equal(error.transportPhase, 'pre_dispatch');
        assert.equal(error.retrySafe, true);
        return true;
      });
      rejectPreparation(failure);
      await nextTurn(); // Node reports an unhandled rejection by this turn.
      assert.deepEqual({preparations, encoded, begun}, {preparations: 1, encoded: 0, begun: 0});
      if (stream) await assert.rejects(response.close(null), error => error.code === 715);
    } finally {
      rejectPreparation(failure);
      if (descriptor === undefined) delete performance.now;
      else Object.defineProperty(performance, 'now', descriptor);
      await client.close();
    }
  });
}

test('expired connector wait preserves the shared acquisition rejection for another waiter', async () => {
  let reads = 0, rejectConnection, connects = 0;
  const late = new Promise((_resolve, reject) => { rejectConnection = reject; });
  const failure = new Error('late connection failure');
  failure.nativeDetails = {message: failure.message, code: 707, canonicalName: 'ResourceUnavailable', transportPhase: 'pre_dispatch'};
  const descriptor = Object.getOwnPropertyDescriptor(performance, 'now');
  Object.defineProperty(performance, 'now', {configurable: true, value: () => reads++ === 0 ? 0 : 11});
  const client = transport.createIpcEncodedTransport('ipc://deadline', {
    callTimeoutMs: 10, connect: () => { connects++; return late; },
  });
  try {
    await assert.rejects(client.call('route', contract, 'ping', () => { throw new Error('must not encode'); }),
      error => error.code === 715 && error.message === 'call deadline exceeded' && error.retrySafe);
    const other = assert.rejects(client.prepare('route', contract), error => {
      assert.equal(error.code, 707);
      assert.equal(error.cause, failure);
      return true;
    });
    rejectConnection(failure);
    await other;
    await nextTurn();
    assert.equal(connects, 1);
  } finally {
    rejectConnection(failure);
    if (descriptor === undefined) delete performance.now;
    else Object.defineProperty(performance, 'now', descriptor);
    await client.close();
  }
});

test('unary starts its native deadline before encoding and never dispatches an expired input',async()=>{
  let clock=0,dispatched=0;
  const clockDescriptor=Object.getOwnPropertyDescriptor(performance,'now');
  Object.defineProperty(performance,'now',{configurable:true,value:()=>clock});
  const events=[];
  const endpoint={connectionSnapshot(){return null},async prepare(){return binding},deactivate(){},async close(){},
    beginCall(_route,_method,stream,timeout){
      assert.equal(stream,false);assert.equal(timeout,10);events.push('begin');const deadline=clock+timeout;
      const checkDeadline=()=>{if(clock>=deadline){const error=new Error('encoding exhausted deadline');error.nativeDetails={message:error.message,code:715,canonicalName:'CallDeadlineExceeded',transportPhase:'pre_dispatch'};throw error}};
      return {checkDeadline,async chargeInput(){checkDeadline()},async dispatch(){checkDeadline();dispatched++;return nativeTarget},async next(){return null},cancel(){},close(){events.push('free')},async observe(){return receipt}};
    }};
  const client=transport.createIpcEncodedTransport('ipc://deadline',{connect:async()=>endpoint,callTimeoutMs:10});
  const encode=()=>{events.push('encode');clock+=11;return new Uint8Array()};
  try {
    await assert.rejects(client.call('route',contract,'ping',encode),error=>error.code===715&&error.transportPhase==='pre_dispatch'&&error.retrySafe===true);
    assert.deepEqual(events,['begin','encode','free']);assert.equal(dispatched,0);
  } finally {
    if(clockDescriptor===undefined)delete performance.now;else Object.defineProperty(performance,'now',clockDescriptor);
    await client.close();
  }
});

test('FallbackDenied retains first local IPC cause across two failed acquisition candidates',async()=>{
  const failures=['ConnectionRefused','ConnectionReset'].map(ioKind=>{
    const error=new Error(ioKind);error.nativeDetails={message:ioKind,transportPhase:'pre_dispatch',category:'ipc',ioOperation:'connect',ioKind};return error;
  });
  const routes=['a','b'].map((id,index)=>({name:'route',relay_url:`http://localhost:${8301+index}`,ipc_address:`ipc://${id}`,server_id:id,server_instance_id:'i',crm_ns:'test',crm_name:'Test',crm_ver:'0.1.0',abi_hash:hash,signature_hash:hash,max_payload_size:100}));
  let posts=0;const attempts=[];
  const fetch=async(_url,init)=>{if(init.method==='POST')posts++;return {status:200,async text(){return JSON.stringify(routes)},async arrayBuffer(){return new ArrayBuffer(0)}}};
  const connect=async address=>({connectionSnapshot(){return null},async prepare(){const index=address==='ipc://a'?0:1;attempts.push(index);throw failures[index]},beginCall(){throw Error('unexpected dispatch')},async close(){},deactivate(){}});
  const client=transport.createRelayAwareHttpEncodedTransport('http://localhost:8300',{fetch,ipc:{connect}});
  await assert.rejects(client.call('route',contract,'ping',()=>new Uint8Array()),error=>{
    assert.equal(typeof error.body,'string','both failed local endpoints leave no independent fallback');
    assert.equal(JSON.parse(error.body).name,'FallbackDenied');
    assert.deepEqual(JSON.parse(error.body).details,{route:'route'});
    assert.equal(error.cause.nativeDetails.ioKind,'ConnectionRefused');
    assert.equal(error.cause.cause,failures[0]);return true;
  });
  assert.deepEqual(attempts,[0,1]);assert.equal(posts,0);
});

test('fresh resolve excludes the same failed local endpoint identity',async()=>{
  const failure=new Error('refused');
  failure.nativeDetails={message:'refused',transportPhase:'pre_dispatch',category:'ipc',ioOperation:'connect',ioKind:'ConnectionRefused'};
  const route={name:'route',relay_url:'http://localhost:8301',ipc_address:'ipc://same',server_id:'s',server_instance_id:'i',crm_ns:'test',crm_name:'Test',crm_ver:'0.1.0',abi_hash:hash,signature_hash:hash,max_payload_size:100};
  let resolves=0,connects=0,posts=0;
  const fetch=async(_url,init)=>{if(init.method==='POST')posts++;else resolves++;return {status:200,async text(){return JSON.stringify([route])},async arrayBuffer(){return new ArrayBuffer(0)}}};
  const connect=async()=>{connects++;return {connectionSnapshot(){return null},async prepare(){throw failure},beginCall(){throw Error('unexpected dispatch')},async close(){},deactivate(){}}};
  const client=transport.createRelayAwareHttpEncodedTransport('http://localhost:8300',{fetch,ipc:{connect}});
  await assert.rejects(client.call('route',contract,'ping',()=>new Uint8Array()),error=>{
    assert.equal(JSON.parse(error.body).name,'FallbackDenied');
    assert.equal(error.cause.nativeDetails.ioKind,'ConnectionRefused');
    return true;
  });
  assert.equal(resolves,2);
  assert.equal(connects,1);
  assert.equal(posts,0);
  await client.close();
});

for (const response of ['empty', 'not-found']) test(`empty fresh resolve retains first local cause (${response})`,async()=>{
  const first=new Error('first refused');
  first.nativeDetails={message:'first refused',transportPhase:'pre_dispatch',category:'ipc',ioOperation:'connect',ioKind:'ConnectionRefused'};
  const route={name:'route',relay_url:'http://localhost:8301',ipc_address:'ipc://same',server_id:'s',server_instance_id:'i',crm_ns:'test',crm_name:'Test',crm_ver:'0.1.0',abi_hash:hash,signature_hash:hash,max_payload_size:100};
  let resolves=0,posts=0;
  const fetch=async(_url,init)=>{
    if(init.method==='POST'){posts++;throw Error('unexpected POST')}
    resolves++;
    if(resolves===1)return {status:200,async text(){return JSON.stringify([route])}};
    return response==='empty' ? {status:200,async text(){return '[]'}} :
      {status:404,async text(){return JSON.stringify({version:1,code:701,name:'ResourceNotFound',message:'missing',details:{dispatch_phase:'pre_dispatch'}})}};
  };
  const connect=async()=>({connectionSnapshot(){return null},async prepare(){throw first},beginCall(){throw Error('unexpected dispatch')},async close(){},deactivate(){}});
  const client=transport.createRelayAwareHttpEncodedTransport('http://localhost:8300',{fetch,ipc:{connect}});
  await assert.rejects(client.call('route',contract,'ping',()=>new Uint8Array()),error=>{
    assert.equal(JSON.parse(error.body).name,'FallbackDenied');
    assert.equal(error.cause.nativeDetails.ioKind,'ConnectionRefused');
    assert.equal(error.cause.cause,first);
    return true;
  });
  assert.equal(posts,0);
  assert.equal(resolves,2);
  await client.close();
});

test('canonical 701 POST invalidates its target without replaying that call',async()=>{
  const route={name:'route',relay_url:'http://localhost:8301',crm_ns:'test',crm_name:'Test',crm_ver:'0.1.0',abi_hash:hash,signature_hash:hash,max_payload_size:100};
  let resolves=0,posts=0,encodes=0;
  const fetch=async(url,init)=>{
    if(url.includes('/_resolve/')){resolves++;return {status:200,async text(){return JSON.stringify([route])}}}
    if(url.includes('/_probe/'))return {status:200,async text(){return ''}};
    posts++;
    if(posts===1)return {status:404,async text(){return JSON.stringify({version:1,code:701,name:'ResourceNotFound',message:'missing',details:{dispatch_phase:'pre_dispatch'}})}};
    return {status:200,async arrayBuffer(){return Uint8Array.of(8).buffer}};
  };
  const client=transport.createRelayAwareHttpEncodedTransport('http://localhost:8300',{fetch});
  const encode=()=>{encodes++;return new Uint8Array()};
  await assert.rejects(client.call('route',contract,'ping',encode),error=>error.status===404&&error.retrySafe===true);
  assert.deepEqual([resolves,posts,encodes],[1,1,1]);
  assert.equal((await client.call('route',contract,'ping',encode))[0],8);
  assert.deepEqual([resolves,posts,encodes],[2,2,2]);
  await client.close();
});

for(const path of ['direct','relay-local'])test(`${path} close accepts 2147483648 ms while opening remains pending`,async()=>{
  const opened=deferred();let observes=0;
  const call={cancel(){},async chargeInput(){},async dispatch(){return nativeTarget},async next(){return null},async observe(ms){observes++;assert.ok(ms>2147483000);return receipt},checkDeadline(){},close(){}};
  const endpoint={connectionSnapshot(){return null},async prepare(){await opened.promise;return binding},beginCall(){return call},deactivate(){},async close(){}};
  const route={name:'route',relay_url:'http://localhost:8301',ipc_address:'ipc://local',server_id:'s',server_instance_id:'i',crm_ns:'test',crm_name:'Test',crm_ver:'0.1.0',abi_hash:hash,signature_hash:hash,max_payload_size:100};
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
  const call={cancel(){},async chargeInput(){},async dispatch(){return nativeTarget},async next(){return null},async observe(){return receipt},checkDeadline(){},close(){}};
  const endpoint={connectionSnapshot(){return null},async prepare(){await opened.promise;return binding},beginCall(){return call},deactivate(){},async close(){}};
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
  const routes=[{name:'route',relay_url:'http://localhost:8301',crm_ns:'test',crm_name:'Test',crm_ver:'0.1.0',abi_hash:hash,signature_hash:hash,max_payload_size:100}];
  const fetch=async(_url,init)=>{fetches++;if(init.method==='POST')posts++;return {status:200,async text(){return JSON.stringify(routes)},async arrayBuffer(){return new ArrayBuffer(0)}}};
  const factory=relayAware?transport.createRelayAwareHttpEncodedTransport:transport.createHttpRelayEncodedTransport;
  const client=factory('http://localhost:8300',{fetch,callTimeoutMs:10});
  try {
    await assert.rejects(client.call('route',contract,'ping',()=>{encoded++;clock+=11;return new Uint8Array()}),error=>error.code===715&&error.transportPhase==='pre_dispatch'&&error.retrySafe===true);
    assert.equal(encoded,1);assert.equal(posts,0);assert.equal(fetches,2);
  } finally {
    if(descriptor===undefined)delete performance.now;else Object.defineProperty(performance,'now',descriptor);
    await client.close?.();
  }
});

for(const relayAware of [false,true])test(`HTTP pre-dispatch probes never restart the input budget (relayAware=${relayAware})`,async()=>{
  let clock=0,encoded=0,posts=0,probes=0;
  const descriptor=Object.getOwnPropertyDescriptor(performance,'now');
  Object.defineProperty(performance,'now',{configurable:true,value:()=>clock});
  const routes=[0,1].map(index=>({name:'route',relay_url:`http://localhost:${8301+index}`,crm_ns:'test',crm_name:'Test',crm_ver:'0.1.0',abi_hash:hash,signature_hash:hash,max_payload_size:100}));
  const fetch=async(url,init)=>{
    if(init.method==='POST'){posts++;throw Error('expired input must not dispatch')}
    if(url.includes('/_probe/')){
      probes++;clock+=probes===1?4:5;
      return {status:probes===1?404:200,async text(){return JSON.stringify({version:1,code:701,name:'ResourceNotFound',message:'missing',details:{dispatch_phase:'pre_dispatch'}})},async arrayBuffer(){return new ArrayBuffer(0)}};
    }
    clock+=2;return {status:200,async text(){return JSON.stringify(routes)},async arrayBuffer(){return new ArrayBuffer(0)}};
  };
  const factory=relayAware?transport.createRelayAwareHttpEncodedTransport:transport.createHttpRelayEncodedTransport;
  const client=factory('http://localhost:8300',{fetch,callTimeoutMs:10});
  try {
    await assert.rejects(client.call('route',contract,'ping',()=>{encoded++;clock+=2;return new Uint8Array()}),error=>error.code===715&&error.transportPhase==='pre_dispatch');
    assert.equal(encoded,0);assert.equal(probes,2);assert.equal(posts,0);
  } finally {
    if(descriptor===undefined)delete performance.now;else Object.defineProperty(performance,'now',descriptor);
    await client.close?.();
  }
});

// ADR 0004: "代理留在服务它当前那次注册的端点或 relay 上".
function relayRoute(id = 1, local = false) {
  return {name:'route',relay_url:`http://localhost:${8300+id}`,
    ...(local?{ipc_address:`ipc://s${id}`,server_id:`s${id}`,server_instance_id:`i${id}`} : {}),
    crm_ns:'test',crm_name:'Test',crm_ver:'0.1.0',abi_hash:hash,signature_hash:hash,max_payload_size:1};
}
function relayResponse(status = 200, value = Uint8Array.of(9), details = {}) {
  return {status, async arrayBuffer(){return value.buffer}, async text(){return JSON.stringify(details)}};
}
for(const explicit of [false,true]) test(`relay target is acquired once and keeps only its URL (explicit=${explicit})`,async()=>{
  let resolves=0,probes=0,posts=0;
  const factory=explicit?transport.createHttpRelayEncodedTransport:transport.createRelayAwareHttpEncodedTransport;
  const client=factory('http://localhost:8300',{fetch:async(url,init)=>{
    if(url.includes('/_resolve/')){resolves++;return {status:200,async text(){return JSON.stringify([relayRoute()])}}}
    if(url.includes('/_probe/')){probes++;return relayResponse()}
    posts++;assert.equal(init.body.length,4);return relayResponse();
  }});
  for(let i=0;i<3;i++)assert.equal((await client.call('route',contract,'ping',()=>Uint8Array.of(1,2,3,4)))[0],9);
  assert.deepEqual({resolves,probes,posts},{resolves:1,probes:1,posts:3});
  await client.close?.();
});

for(const explicit of [false,true]) test(`HTTP refusal follows on the next call without replay (explicit=${explicit})`,async()=>{
  let selected=1,resolves=0,probes=0,posts=0,encoded=0;
  const urls=[];
  const factory=explicit?transport.createHttpRelayEncodedTransport:transport.createRelayAwareHttpEncodedTransport;
  const client=factory('http://localhost:8300',{fetch:async(url,init)=>{
    if(url.includes('/_resolve/')){resolves++;return {status:200,async text(){return JSON.stringify([relayRoute(selected)])}}}
    if(url.includes('/_probe/')){probes++;return relayResponse()}
    posts++;urls.push(url);
    if(posts===2)return relayResponse(404,new Uint8Array(),{version:1,code:701,name:'ResourceNotFound',message:'gone',details:{dispatch_phase:'pre_dispatch'}});
    return relayResponse();
  }});
  const call=()=>client.call('route',contract,'ping',()=>{encoded++;return new Uint8Array()});
  await call();selected=2;
  await assert.rejects(call(),e=>e.retrySafe===true);
  assert.deepEqual({resolves,posts,encoded},{resolves:1,posts:2,encoded:2});
  await call();assert.deepEqual({resolves,probes,posts,encoded},{resolves:2,probes:2,posts:3,encoded:3});
  assert.ok(urls[1].startsWith('http://localhost:8301/'));
  assert.ok(urls[2].startsWith('http://localhost:8302/'));
  await client.close?.();
});

for(const code of [705,715])test(`HTTP pre-dispatch ${code} keeps its current target`,async()=>{
  let resolves=0,posts=0;
  const client=transport.createRelayAwareHttpEncodedTransport('http://localhost:8300',{fetch:async(url)=>{
    if(url.includes('/_resolve/')){resolves++;return {status:200,async text(){return JSON.stringify([relayRoute()])}}}
    if(url.includes('/_probe/'))return relayResponse();
    if(++posts===2)return relayResponse(503,new Uint8Array(),{version:1,code,name:code===715?'CallDeadlineExceeded':'ResourceOverloaded',message:'refused',details:{dispatch_phase:'pre_dispatch'}});
    return relayResponse();
  }});
  await client.call('route',contract,'ping',()=>new Uint8Array());
  await assert.rejects(client.call('route',contract,'ping',()=>new Uint8Array()));
  await client.call('route',contract,'ping',()=>new Uint8Array());
  assert.equal(resolves,1);
  await client.close();
});

test('eight concurrent calls share one target acquisition',async()=>{
  const reached=deferred(), release=deferred();let resolves=0,posts=0;
  const client=transport.createRelayAwareHttpEncodedTransport('http://localhost:8300',{fetch:async(url)=>{
    if(url.includes('/_resolve/')){resolves++;reached.resolve();await release.promise;return {status:200,async text(){return JSON.stringify([relayRoute()])}}}
    if(url.includes('/_probe/'))return relayResponse();posts++;return relayResponse();
  }});
  const calls=Array.from({length:8},()=>client.call('route',contract,'ping',()=>new Uint8Array()));
  await reached.promise;release.resolve();await Promise.all(calls);
  assert.equal(resolves,1);assert.equal(posts,8);await client.close();
});

test('relay-local observes peer end before preparing with the old identity',async()=>{
  let generation=1,resolves=0,connections=0;const preparations=[];
  const observed=[];
  const client=transport.createRelayAwareHttpEncodedTransport('http://localhost:8300',{
    observe:x=>observed.push(x),
    fetch:async(url)=>{assert.ok(url.includes('/_resolve/'));resolves++;return {status:200,async text(){return JSON.stringify([relayRoute(generation,true)])}}},
    ipc:{connect:async(_address,_timeout,options)=>{
      connections++;const own=generation;const binding={serverId:`s${own}`,serverInstanceId:`i${own}`,connectionId:String(own)};
      return {async prepare(){preparations.push(own);return binding},
        connectionSnapshot(){return {...binding,endReason:generation===own?null:'peer_eof'}},
        beginCall(){return {async chargeInput(){},async dispatch(){return {...binding,path:'DirectIpc'}},async next(){const value=Uint8Array.of(own);value.release=()=>{};return value},checkDeadline(){},cancel(){},close(){},async observe(){return receipt}}},
        deactivate(){},async close(){},
      };
    }},
  });
  await client.call('route',contract,'ping',()=>new Uint8Array());generation=2;
  assert.equal((await client.call('route',contract,'ping',()=>new Uint8Array()))[0],2);
  assert.deepEqual({resolves,connections},{resolves:2,connections:2});
  assert.deepEqual(preparations,[1,2]);assert.equal(observed.at(-1).serverInstanceId,'i2');
  await client.close();
});

test('HTTP acquisition consumes the original call deadline before encoding',async()=>{
  let clock=0,encoded=0,posts=0;
  const descriptor=Object.getOwnPropertyDescriptor(performance,'now');
  Object.defineProperty(performance,'now',{configurable:true,value:()=>clock});
  const client=transport.createRelayAwareHttpEncodedTransport('http://localhost:8300',{callTimeoutMs:10,fetch:async(url,init)=>{
    if(init.method==='POST')posts++;
    if(url.includes('/_resolve/'))clock+=11;
    return {status:200,async text(){return JSON.stringify([relayRoute()])},async arrayBuffer(){return new ArrayBuffer(0)}};
  }});
  try {
    await assert.rejects(client.call('route',contract,'ping',()=>{encoded++;return new Uint8Array()}),e=>e.code===715&&e.transportPhase==='pre_dispatch');
    assert.equal(encoded,0);assert.equal(posts,0);
  } finally {if(descriptor===undefined)delete performance.now;else Object.defineProperty(performance,'now',descriptor);await client.close();}
});

test('observation uses the actual dispatch target rather than its preparation',async()=>{
  const observations=[];
  const target={path:'DirectIpc',serverId:'s',serverInstanceId:'actual',connectionId:'2'};
  const endpoint={async prepare(){return binding},connectionSnapshot(){return null},deactivate(){},async close(){},
    beginCall(){return {async chargeInput(){},async dispatch(){return target},async next(){const b=Uint8Array.of(1);b.release=()=>{};return b},checkDeadline(){},cancel(){},close(){},async observe(){return receipt}}}};
  const client=transport.createIpcEncodedTransport('ipc://observation',{connect:async()=>endpoint,observe:o=>observations.push(o)});
  await client.call('route',contract,'ping',()=>new Uint8Array());
  assert.equal(observations[0].serverInstanceId,'actual');
  await client.close();
});

test('late response keeps its own dispatch identity after a newer response',async()=>{
  const firstEntered=deferred(),firstReleased=deferred();const observations=[];let calls=0;
  const endpoint={async prepare(){return binding},connectionSnapshot(){return null},deactivate(){},async close(){},
    beginCall(){const id=++calls;return {async chargeInput(){},async dispatch(){return {path:'DirectIpc',serverId:'s',serverInstanceId:`actual-${id}`,connectionId:String(id)}},
      async next(){if(id===1){firstEntered.resolve();await firstReleased.promise}const b=Uint8Array.of(id);b.release=()=>{};return b},checkDeadline(){},cancel(){},close(){},async observe(){return receipt}}}};
  const client=transport.createIpcEncodedTransport('ipc://observation',{connect:async()=>endpoint,observe:o=>observations.push(o)});
  const old=client.call('route',contract,'ping',()=>new Uint8Array());await firstEntered.promise;
  await client.call('route',contract,'ping',()=>new Uint8Array());
  firstReleased.resolve();await old;
  assert.deepEqual(observations.map(o=>o.serverInstanceId),['actual-2','actual-1']);
  await client.close();
});
