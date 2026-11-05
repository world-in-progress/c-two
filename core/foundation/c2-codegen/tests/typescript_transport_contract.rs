use c2_codegen::{ContractCodegenTarget, compile_contract_artifacts};
use c2_contract::ContractRelease;
use std::path::Path;
use std::process::Command;

const TRANSPORT: &str = include_str!("../assets/typescript_transport.ts");

#[test]
fn generated_contract_keeps_http_route_binding_and_fastdb_error_projection() {
    let descriptor =
        include_str!("../../../../tests/fixtures/contracts/portable-release.contract.json");
    let release = ContractRelease::from_descriptor_json(descriptor.as_bytes()).unwrap();
    let set = compile_contract_artifacts(
        &release,
        ContractCodegenTarget::TypeScript,
        &Default::default(),
    )
    .unwrap();
    let source =
        std::str::from_utf8(set.get("typescript/c_two_contract.ts").unwrap().bytes()).unwrap();
    for required in [
        "requestHeadersForResolvedRoute",
        "x-c2-route-uid",
        "expectedServerIdentity",
        "expectedRouteToken",
        "same-path fallback denied",
        "C2PayloadAdapterError",
        "cause_owner",
        "fastdb_code",
        "fastdb_details_json",
        "fastdb_message",
        "fastdb_path",
        "fastdb_symbol",
        "error instanceof PayloadError",
        "DirectIpc",
        "ExplicitRelay",
        "RelayAwareLocalIpc",
        "RelayAwareHttp",
        "routeUid",
        "/_probe/",
        "ipcAddress",
        "serverId",
        "serverInstanceId",
    ] {
        assert!(
            source.contains(required),
            "generated contract lacks {required}"
        );
    }
}

#[test]
fn transport_embedding_preserves_contract_release_payload_and_other_language_bytes() {
    let baseline: serde_json::Value =
        serde_json::from_str(include_str!("fixtures/unary-artifacts.json")).unwrap();
    for row in baseline.as_array().unwrap() {
        let fixture = row["fixture"].as_str().unwrap();
        let descriptor = std::fs::read(
            Path::new(env!("CARGO_MANIFEST_DIR"))
                .join("../../../tests/fixtures/contracts")
                .join(format!("{fixture}.contract.json")),
        )
        .unwrap();
        let release = ContractRelease::from_descriptor_json(&descriptor).unwrap();
        let target = match row["target"].as_str().unwrap() {
            "rust" => ContractCodegenTarget::Rust,
            "python" => ContractCodegenTarget::Python,
            "typescript" => ContractCodegenTarget::TypeScript,
            _ => unreachable!(),
        };
        let set = compile_contract_artifacts(&release, target, &Default::default()).unwrap();
        assert_eq!(
            set.artifacts().len(),
            row["artifacts"].as_array().unwrap().len()
        );
        for expected in row["artifacts"].as_array().unwrap() {
            let path = expected["path"].as_str().unwrap();
            let actual = set.get(path).unwrap();
            if target == ContractCodegenTarget::TypeScript {
                if path == "typescript/c_two_contract.ts" {
                    assert!(
                        std::str::from_utf8(actual.bytes())
                            .unwrap()
                            .contains(TRANSPORT)
                    );
                    continue;
                }
                // The composition manifest includes the changed source's hash and byte count.
                if path == "metadata/composition-manifest.json" {
                    continue;
                }
            }
            assert_eq!(
                actual.sha256_hex(),
                expected["sha256"].as_str().unwrap(),
                "{fixture}: {path}"
            );
            assert_eq!(
                actual.bytes().len() as u64,
                expected["bytes"].as_u64().unwrap(),
                "{fixture}: {path}"
            );
        }
    }
}

#[test]
fn generated_transport_uses_native_endpoint_and_has_no_wire_reader() {
    for symbol in [
        "C2NativeEndpointContext",
        "C2NativeCall",
        "C2ResponseStream",
        "beginCall(routeName, method, true, callTimeoutMs)",
        "call.chargeInput(request.byteLength)",
        "call.dispatch(request)",
        "C2UnsupportedCallModeError",
    ] {
        assert!(
            TRANSPORT.contains(symbol),
            "missing native projection {symbol}"
        );
    }
    for retired in [
        "callChain",
        "readExactly(",
        "encodeIpcFrame(",
        "decodeIpcCatalogReply(",
        "writeIpcFrame(",
    ] {
        assert!(
            !TRANSPORT.contains(retired),
            "retired codec remains: {retired}"
        );
    }
}

#[test]
fn native_transport_facade_behaves_under_node() {
    let directory = tempfile::tempdir().expect("temporary TypeScript project");
    let root = directory.path();
    let repository = Path::new(env!("CARGO_MANIFEST_DIR"))
        .ancestors()
        .nth(3)
        .expect("repository root");
    let compiler = repository
        .join("core/foundation/c2-mem-ffi/bindings/typescript/node_modules/typescript/bin/tsc");
    assert!(
        compiler.is_file(),
        "build the pinned TypeScript dependency before this suite: {}",
        compiler.display()
    );
    std::fs::write(root.join("package.json"), r#"{"type":"module"}"#).unwrap();
    let asset = root.join("typescript_transport.ts");
    std::fs::write(&asset, format!("{TRANSPORT}\nconst unaryTransportSmoke: C2EncodedClientTransport = {{ async call() {{ return new Uint8Array(); }} }};\nvoid unaryTransportSmoke;\n")).unwrap();
    let compile = Command::new("node")
        .arg(&compiler)
        .args([
            "--strict",
            "--target",
            "ES2022",
            "--module",
            "ES2022",
            "--moduleResolution",
            "Bundler",
            "--lib",
            "ES2022,DOM",
            "--rootDir",
            asset.parent().unwrap().to_str().unwrap(),
            "--outDir",
            root.to_str().unwrap(),
            asset.to_str().unwrap(),
        ])
        .arg("--typeRoots")
        .arg(root.join("node_modules/@types"))
        .output()
        .expect("run TypeScript compiler");
    assert!(
        compile.status.success(),
        "TypeScript facade failed to compile:\n{}\n{}",
        String::from_utf8_lossy(&compile.stdout),
        String::from_utf8_lossy(&compile.stderr),
    );
    std::fs::write(root.join("native-transport.test.mjs"), r#"
import test from 'node:test';
import assert from 'node:assert/strict';
import {
  createIpcEncodedTransport, createHttpRelayEncodedTransport, createRelayAwareHttpEncodedTransport,
  C2UnsupportedCallModeError, C2CoreError,
} from './typescript_transport.js';
const hash = 'a'.repeat(64);
const contract = {schema:'c-two.contract.v2', namespace:'test', name:'Test', version:'0.1.0', descriptorSha256:hash, abiHash:hash, signatureHash:hash};
const receipt = {completed:true, error:null, completeItems:2n};
function buffer(bytes, onRelease) { const value = Uint8Array.from(bytes); value.release = onRelease; return value; }

test('stream is immediate; native begin precedes encoding; each hold has a local owner', async () => {
  const events = [];
  let releaseCount = 0;
  let item = 0;
  let cancelled = false;
  const call = {
    async chargeInput(n) { events.push(`charge:${n}`); },
    async dispatch(bytes) { events.push(`dispatch:${bytes.length}`); },
    async next() { events.push('next'); return item++ < 2 ? buffer([item], () => {releaseCount++}) : null; },
    cancel() { cancelled = true; events.push('cancel'); },
    async observe(timeout) { events.push(`observe:${timeout}`); return receipt; },
    checkDeadline() { events.push('deadline'); },
    close() { events.push('free'); },
  };
  const endpoint = {
    async prepare() { events.push('prepare'); return {serverId:'server',serverInstanceId:'instance',routeUid:'uid',connectionId:1n}; },
    beginCall(_route,_method,serverStream) { assert.equal(serverStream,true); events.push('begin'); return call; },
    async close() {}, deactivate() {},
  };
  const transport = createIpcEncodedTransport('ipc://test', {connect:async () => endpoint});
  const stream = transport.stream('route',contract,'steps',() => {events.push('encode'); return Uint8Array.of(3)}, bytes => ({value:bytes[0]}), true);
  assert.equal(stream[Symbol.asyncIterator](), stream);
  const first = await stream.next();
  assert.equal(first.value.value.value,1);
  assert.equal(releaseCount,0);
  first.value.release();
  assert.equal(releaseCount,1);
  const second = await stream.next();
  assert.equal(second.value.value.value,2);
  second.value.release();
  assert.deepEqual(await stream.next(), {done:true,value:undefined});
  assert.deepEqual(events.slice(0,4), ['prepare','begin','encode','charge:1']);
  assert.deepEqual(await stream.close(0), receipt);
  assert.equal(cancelled,true);
  assert.ok(events.indexOf('cancel') < events.indexOf('observe:0'));
});

test('two unary calls enter native begin without a JS call chain', async () => {
  let begins = 0;
  let releaseCount = 0;
  const pending = [];
  const endpoint = {
    async prepare() {return {serverId:'s',serverInstanceId:'i',routeUid:'u',connectionId:1n}},
    beginCall() {
      begins++;
      let finish;
      const gate = new Promise(resolve => {finish = resolve});
      pending.push(finish);
      return {
        async chargeInput() {}, async dispatch() {await gate},
        async next() {return buffer([7], () => {releaseCount++})},
        close() {}, cancel() {}, async observe() {return receipt}, checkDeadline() {},
      };
    },
    async close() {}, deactivate() {},
  };
  const transport = createIpcEncodedTransport('ipc://test',{connect:async()=>endpoint});
  const a = transport.call('route',contract,'ping',()=>Uint8Array.of());
  const b = transport.call('route',contract,'ping',()=>Uint8Array.of());
  await new Promise(resolve => setImmediate(resolve));
  assert.equal(begins,2);
  pending.forEach(finish=>finish());
  const results = await Promise.all([a,b]);
  assert.deepEqual(results.map(x=>x[0]),[7,7]);
  results.forEach(x=>x.release());
  assert.equal(releaseCount,2);
});

test('IPC endpoint options reach the native connector once', async () => {
  const seen=[];
  const endpoint={
    async prepare(){return {serverId:'s',serverInstanceId:'i',routeUid:'u',connectionId:1n}},
    beginCall(){throw Error('unexpected')}, async close(){}, deactivate(){},
  };
  const transport=createIpcEncodedTransport('ipc://test',{
    connect:async (address,timeout,options)=>{seen.push({address,timeout,options});return endpoint},
    connectTimeoutMs:17,
    expectedServerIdentity:{serverId:'s',serverInstanceId:'i'}, expectedRouteToken:{routeUid:'u'},
  });
  await transport.prepare('route',contract);
  assert.deepEqual(seen,[{address:'ipc://test',timeout:17,options:{
    connectTimeoutMs:17,
    expectedServerIdentity:{serverId:'s',serverInstanceId:'i'},expectedRouteToken:{routeUid:'u'},
  }}]);
});

test('IPC connection deadlines require positive safe milliseconds at the public boundary', () => {
  for (const connectTimeoutMs of [0,-1,0.1,NaN,Infinity,Number.MAX_SAFE_INTEGER+1,null,'1']) {
    assert.throws(()=>createIpcEncodedTransport('ipc://test',{connect:async()=>{},connectTimeoutMs}),/positive safe integer/);
  }
});

test('HTTP stream refusal happens before fetch or input encoding', () => {
  let fetches=0, encoded=0;
  const transport=createHttpRelayEncodedTransport('http://localhost:8300',{fetch:async()=>{fetches++;throw Error('unexpected')}});
  assert.throws(()=>transport.stream('route',contract,'steps',()=>{encoded++;return Uint8Array.of()},x=>x), e=>e instanceof C2UnsupportedCallModeError && e.nativeDetails.transportPhase==='pre_dispatch');
  assert.equal(fetches,0); assert.equal(encoded,0);
});

test('return before native admission cancels without encoding or dispatch', async () => {
  let allowPrepare;
  const gate = new Promise(resolve => { allowPrepare = resolve; });
  let encoded = 0;
  let dispatched = 0;
  let cancelled = 0;
  const call = {
    async chargeInput() {}, async dispatch() { dispatched++; }, async next() { return null; },
    cancel() { cancelled++; }, async observe() { return {completed:true,error:null,completeItems:0n}; },
    checkDeadline() {}, close() {},
  };
  const endpoint = {
    async prepare() { await gate; return {serverId:'s',serverInstanceId:'i',routeUid:'u',connectionId:1n}; },
    beginCall() { return call; }, async close() {}, deactivate() {},
  };
  const transport=createIpcEncodedTransport('ipc://test',{connect:async()=>endpoint});
  const stream=transport.stream('route',contract,'steps',()=>{encoded++;return Uint8Array.of()},x=>x);
  assert.deepEqual(await stream.return(),{done:true,value:undefined});
  allowPrepare();
  assert.deepEqual(await stream.close(null),{completed:true,error:null,completeItems:0n});
  assert.equal(encoded,0); assert.equal(dispatched,0); assert.ok(cancelled>0);
});

test('close zero observes without waiting for native preparation', async () => {
  let allowPrepare;
  const gate = new Promise(resolve => { allowPrepare = resolve; });
  const endpoint = {
    async prepare() { await gate; return {serverId:'s',serverInstanceId:'i',routeUid:'u',connectionId:1n}; },
    beginCall() { return {cancel(){},async chargeInput(){},async dispatch(){},async next(){return null},async observe(ms){assert.equal(ms,0);return receipt},checkDeadline(){},close(){}}; },
    async close(){},deactivate(){},
  };
  const stream=createIpcEncodedTransport('ipc://test',{connect:async()=>endpoint}).stream('route',contract,'steps',()=>Uint8Array.of(),x=>x);
  assert.deepEqual(await stream.close(0),{completed:false,error:null,completeItems:null});
  allowPrepare();
  await new Promise(resolve => setImmediate(resolve));
  assert.deepEqual(await stream.close(0),receipt);
});

test('finite close budget includes native preparation', async () => {
  let allowPrepare;
  const gate = new Promise(resolve => {allowPrepare=resolve});
  const endpoint={async prepare(){await gate;return {serverId:'s',serverInstanceId:'i',routeUid:'u',connectionId:1n}},
    beginCall(){return {cancel(){},async chargeInput(){},async dispatch(){},async next(){return null},async observe(){return receipt},checkDeadline(){},close(){}}},
    async close(){},deactivate(){}};
  const stream=createIpcEncodedTransport('ipc://test',{connect:async()=>endpoint}).stream('route',contract,'steps',()=>Uint8Array.of(),x=>x);
  const started=performance.now();
  assert.deepEqual(await stream.close(10),{completed:false,error:null,completeItems:null});
  assert.ok(performance.now()-started<500);
  allowPrepare();
  assert.deepEqual(await stream.close(null),receipt);
});

test('deadline after synchronous decode prevents publishing the item', async () => {
  let expired=false, released=0, disposed=0;
  const timeout=new Error('deadline');
  timeout.nativeDetails={message:'deadline',code:715,canonicalName:'CallDeadlineExceeded',transportPhase:'dispatch_uncertain'};
  const call={async chargeInput(){},async dispatch(){},async next(){return buffer([1],()=>released++)},
    cancel(){},async observe(){return receipt},close(){},checkDeadline(){if(expired)throw timeout}};
  const endpoint={async prepare(){return {serverId:'s',serverInstanceId:'i',routeUid:'u',connectionId:1n}},
    beginCall(){return call},async close(){},deactivate(){}};
  const stream=createIpcEncodedTransport('ipc://test',{connect:async()=>endpoint}).stream('route',contract,'steps',()=>{return Uint8Array.of()},()=>{
    expired=true; return {dispose(){disposed++}};
  });
  await assert.rejects(stream.next(),e=>e.transportPhase==='dispatch_uncertain'&&e.retrySafe===false);
  assert.equal(released,1); assert.equal(disposed,1);
});

test('resource error projects separately from uncertain transport failure', async () => {
  const failure=new Error('resource failed');
  failure.nativeDetails={message:'resource failed',code:3,canonicalName:'ResourceFunctionExecuting',details:{method:'steps'}};
  const call={async chargeInput(){},async dispatch(){},async next(){throw failure},cancel(){},
    async observe(){return receipt},checkDeadline(){},close(){}};
  const endpoint={async prepare(){return {serverId:'s',serverInstanceId:'i',routeUid:'u',connectionId:1n}},
    beginCall(){return call},async close(){},deactivate(){}};
  const stream=createIpcEncodedTransport('ipc://test',{connect:async()=>endpoint}).stream('route',contract,'steps',()=>Uint8Array.of(),x=>x);
  await assert.rejects(stream.next(),e=>e instanceof C2CoreError && e.code===3 && e.retrySafe===false && e.details.method==='steps');
});

test('deadline stops waiting without cancelling the dispatched native call', async () => {
  let cancellations=0;
  const deadline=new Error('deadline');
  deadline.nativeDetails={message:'deadline',code:715,canonicalName:'CallDeadlineExceeded',transportPhase:'dispatch_uncertain'};
  const call={async chargeInput(){},async dispatch(){},async next(){throw deadline},cancel(){cancellations++},
    async observe(){return {completed:false,error:null,completeItems:null}},checkDeadline(){},close(){}};
  const endpoint={async prepare(){return {serverId:'s',serverInstanceId:'i',routeUid:'u',connectionId:1n}},
    beginCall(){return call},async close(){},deactivate(){}};
  const stream=createIpcEncodedTransport('ipc://test',{connect:async()=>endpoint}).stream('route',contract,'steps',()=>Uint8Array.of(),x=>x);
  await assert.rejects(stream.next(),e=>e.code===715&&e.transportPhase==='dispatch_uncertain');
  assert.equal(cancellations,0);
  assert.deepEqual(await stream.close(0),{completed:false,error:null,completeItems:null});
  assert.equal(cancellations,1);
});

test('deadline during dispatch retains native call for later observation', async () => {
  let cancellations=0, observations=0;
  const deadline=new Error('deadline');
  deadline.nativeDetails={message:'deadline',code:715,canonicalName:'CallDeadlineExceeded',transportPhase:'dispatch_uncertain'};
  const call={async chargeInput(){},async dispatch(){throw deadline},async next(){throw Error('not reached')},cancel(){cancellations++},
    async observe(){observations++;return {completed:false,error:null,completeItems:null}},checkDeadline(){},close(){}};
  const endpoint={async prepare(){return {serverId:'s',serverInstanceId:'i',routeUid:'u',connectionId:1n}},
    beginCall(){return call},async close(){},deactivate(){}};
  const stream=createIpcEncodedTransport('ipc://test',{connect:async()=>endpoint}).stream('route',contract,'steps',()=>Uint8Array.of(),x=>x);
  await assert.rejects(stream.next(),e=>e.code===715);
  assert.equal(cancellations,0);
  assert.deepEqual(await stream.close(0),{completed:false,error:null,completeItems:null});
  assert.equal(observations,1);
});

test('relay-aware stale POST is returned once and the next call resolves again', async () => {
  let resolves=0, posts=0;
  const reply=(status,text,bytes=new Uint8Array())=>({
    status, headers:{get(name){return name==='content-length'?String(bytes.length):null}},
    async text(){return text}, async arrayBuffer(){return bytes.buffer},
  });
  const route=token=>({
    name:'route',relay_url:'http://localhost:8301',route_uid:token,
    crm_ns:'test',crm_name:'Test',crm_ver:'0.1.0',abi_hash:hash,signature_hash:hash,
    max_payload_size:100,
  });
  const fetch=async (url,init)=>{
    if(url.includes('/_resolve/')) return reply(200,JSON.stringify([route(++resolves===1?'old':'new')]));
    if(url.includes('/_probe/')) return reply(200,'');
    if(init.method==='POST') {
      posts++;
      return posts===1
        ? reply(409,JSON.stringify({version:1,code:704,name:'RouteStale',message:'stale',details:{}}))
        : reply(200,'',Uint8Array.of(9));
    }
    throw Error(`unexpected ${url}`);
  };
  const transport=createRelayAwareHttpEncodedTransport('http://localhost:8300',{fetch});
  await assert.rejects(transport.call('route',contract,'ping',()=>Uint8Array.of()), e=>e.status===409&&e.retrySafe===true);
  assert.equal(posts,1);
  assert.equal(resolves,1);
  const result=await transport.call('route',contract,'ping',()=>Uint8Array.of());
  assert.equal(result[0],9);
  assert.equal(posts,2);
  assert.equal(resolves,2);
});

test('same-path fallback denial retains typed first native cause', async () => {
  let posts=0;
  const nativeFailure=new Error('native connection refused');
  nativeFailure.nativeDetails={message:'native connection refused',transportPhase:'pre_dispatch',category:'ipc',ioOperation:'connect',ioKind:'ConnectionRefused',rawOsError:61};
  const route={name:'route',relay_url:'http://localhost:8301',route_uid:'old',
    ipc_address:'ipc://local',server_id:'s',server_instance_id:'i',
    crm_ns:'test',crm_name:'Test',crm_ver:'0.1.0',abi_hash:hash,signature_hash:hash,max_payload_size:100};
  const fetch=async (_url,init)=>{
    if(init.method==='POST') posts++;
    return {status:200, async text(){return JSON.stringify([route])}, async arrayBuffer(){return new ArrayBuffer(0)}};
  };
  const connect=async()=>({
    async prepare(){throw nativeFailure}, beginCall(){throw Error('unexpected dispatch')},
    async close(){}, deactivate(){},
  });
  const transport=createRelayAwareHttpEncodedTransport('http://localhost:8300',{fetch,ipc:{connect}});
  await assert.rejects(transport.call('route',contract,'ping',()=>Uint8Array.of()), e=>{
    assert.equal(JSON.parse(e.body).name,'FallbackDenied');
    assert.equal(e.cause.nativeDetails.ioKind,'ConnectionRefused');
    assert.equal(e.cause.nativeDetails.rawOsError,61);
    assert.equal(e.cause.nativeDetails.transportPhase,'pre_dispatch');
    return true;
  });
  assert.equal(posts,0);
});

test('relay local trust and deadline failures never select another candidate', async () => {
  for (const alternatives of [false,true]) for (const closeFails of [false,true]) for (const nativeDetails of [
    {code:709,canonicalName:'ContractMismatch'},
    {code:710,canonicalName:'IdentityMismatch'},
    {code:713,canonicalName:'ProtocolViolation'},
    {code:715,canonicalName:'CallDeadlineExceeded'},
    {category:'protocol_version_mismatch',localProtocolVersion:16,peerProtocolVersion:15},
    {category:'runtime_retired'},
  ]) {
    let resolves=0,posts=0;
    const cleanup=new Error('local close failure');
    const failure=new Error('terminal local failure');
    failure.nativeDetails={message:failure.message,transportPhase:'pre_dispatch',...nativeDetails};
    const route={name:'route',relay_url:'http://localhost:8301',route_uid:'local',
      ipc_address:'ipc://local',server_id:'s',server_instance_id:'i',
      crm_ns:'test',crm_name:'Test',crm_ver:'0.1.0',abi_hash:hash,signature_hash:hash,max_payload_size:100};
    const fetch=async (_url,init)=>{
      if(init.method==='POST') posts++; else resolves++;
      return {status:200,async text(){return JSON.stringify(alternatives?[route,{...route,route_uid:'remote',ipc_address:undefined,server_id:undefined,server_instance_id:undefined}]:[route])},async arrayBuffer(){return new ArrayBuffer(0)}};
    };
    const endpoint={async prepare(){throw failure},beginCall(){throw Error('unexpected dispatch')},async close(){if(closeFails)throw cleanup},deactivate(){}};
    const transport=createRelayAwareHttpEncodedTransport('http://localhost:8300',{fetch,ipc:{connect:async()=>endpoint}});
    await assert.rejects(transport.call('route',contract,'ping',()=>new Uint8Array()),e=>e.nativeDetails===failure.nativeDetails && e.cause===failure && (!closeFails||e.cleanupError===cleanup));
    assert.equal(resolves,1); assert.equal(posts,0);
  }
});

test('relay-aware close zero does not wait for route resolution', async () => {
  let resolveRoute;
  const gate=new Promise(resolve=>{resolveRoute=resolve});
  let encoded=0, dispatched=0;
  const route={name:'route',relay_url:'http://localhost:8301',route_uid:'uid',
    ipc_address:'ipc://local',server_id:'s',server_instance_id:'i',
    crm_ns:'test',crm_name:'Test',crm_ver:'0.1.0',abi_hash:hash,signature_hash:hash,max_payload_size:100};
  const fetch=async()=>{await gate;return {status:200,async text(){return JSON.stringify([route])},async arrayBuffer(){return new ArrayBuffer(0)}}};
  const call={async chargeInput(){},async dispatch(){dispatched++},async next(){return null},cancel(){},
    async observe(){return {completed:true,error:null,completeItems:0n}},checkDeadline(){},close(){}};
  const endpoint={async prepare(){return {serverId:'s',serverInstanceId:'i',routeUid:'uid',connectionId:1n}},
    beginCall(){return call},async close(){},deactivate(){}};
  const transport=createRelayAwareHttpEncodedTransport('http://localhost:8300',{fetch,ipc:{connect:async()=>endpoint}});
  const stream=transport.stream('route',contract,'steps',()=>{encoded++;return Uint8Array.of()},x=>x);
  assert.deepEqual(await stream.close(0),{completed:false,error:null,completeItems:null});
  resolveRoute();
  assert.deepEqual(await stream.close(null),{completed:true,error:null,completeItems:0n});
  assert.equal(encoded,0); assert.equal(dispatched,0);
});

test('collecting the public stream while next waits only submits cancellation', async () => {
  let enteredResolve, finishNext, freed=0, cancelled=0;
  const entered=new Promise(resolve=>{enteredResolve=resolve});
  const nativeCall={async chargeInput(){},async dispatch(){},
    next(){enteredResolve();return new Promise(resolve=>{finishNext=resolve})},
    cancel(){cancelled++;},close(){freed++;finishNext?.(null)},checkDeadline(){},
    async observe(){return {completed:false,error:null,completeItems:null}}};
  const endpoint={async prepare(){return {serverId:'s',serverInstanceId:'i',routeUid:'uid',connectionId:1n}},
    beginCall(){return nativeCall},async close(){},deactivate(){}};
  const transport=createIpcEncodedTransport('ipc://gc',{connect:async()=>endpoint});
  let stream=transport.stream('route',contract,'steps',()=>new Uint8Array(),x=>Array.from(x));
  const pending=stream.next();
  await entered;
  stream=null;
  for(let i=0;i<100&&freed===0;i++){global.gc();await new Promise(resolve=>setImmediate(resolve));}
  const beforeResult=freed;
  finishNext(null);
  await pending;
  assert.equal(beforeResult,1,'a pending next must not keep the public facade alive');
  assert.ok(cancelled>0);
  await transport.close();
});
"#).unwrap();
    let run = Command::new("node")
        .args(["--expose-gc", "--test", "native-transport.test.mjs"])
        .current_dir(root)
        .output()
        .expect("run Node tests");
    assert!(
        run.status.success(),
        "Node transport tests failed:\n{}\n{}",
        String::from_utf8_lossy(&run.stdout),
        String::from_utf8_lossy(&run.stderr),
    );
}
