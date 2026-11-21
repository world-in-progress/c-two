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
        "requestHeadersForRouteContract",
        "expectedServerIdentity",
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
                    let actual_str = std::str::from_utf8(actual.bytes()).unwrap();
                    let (prefix, suffix) = TRANSPORT
                        .split_once("/* __C2_GENERATED_ERROR_CODE_NAMES__ */ Object.freeze({})")
                        .expect("error-code placeholder in typescript_transport.ts");
                    assert!(actual_str.contains(prefix));
                    assert!(actual_str.contains(suffix));
                    assert!(!actual_str.contains("__C2_GENERATED_ERROR_CODE_NAMES__"));
                    assert!(actual_str.contains("719: \"RouteConflict\""));
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
        "beginCall(routeName, method, true, nativeLimit(budget))",
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
    let descriptor =
        include_str!("../../../../tests/fixtures/contracts/portable-no-payload.contract.json");
    let release = ContractRelease::from_descriptor_json(descriptor.as_bytes()).unwrap();
    let generated_set = compile_contract_artifacts(
        &release,
        ContractCodegenTarget::TypeScript,
        &Default::default(),
    )
    .unwrap();
    let generated_contract = std::str::from_utf8(
        generated_set
            .get("typescript/c_two_contract.ts")
            .unwrap()
            .bytes(),
    )
    .unwrap();
    assert!(!generated_contract.contains("__C2_GENERATED_ERROR_CODE_NAMES__"));
    let transport_start = generated_contract
        .find("export interface C2ContractIdentity {")
        .expect("transport section start");
    let (rendered_transport, _) = generated_contract[transport_start..]
        .split_once("\n\nexport interface C2FastDbCauseFields")
        .expect("contract transport boundary");
    let asset = root.join("typescript_transport.ts");
    std::fs::write(&asset, format!("{rendered_transport}\nconst unaryTransportSmoke: C2EncodedClientTransport = {{ async call() {{ return new Uint8Array(); }} }};\nvoid unaryTransportSmoke;\n")).unwrap();
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
const nativeTarget={path:'DirectIpc',serverId:'s',serverInstanceId:'i',connectionId:'1'};
function buffer(bytes, onRelease) { const value = Uint8Array.from(bytes); value.release = onRelease; return value; }

test('stream is immediate; native begin precedes encoding; each hold has a local owner', async () => {
  const events = [];
  let releaseCount = 0;
  let item = 0;
  let cancelled = false;
  const call = {
    async chargeInput(n) { events.push(`charge:${n}`); },
    async dispatch(bytes) { events.push(`dispatch:${bytes.length}`); return nativeTarget; },
    async next() { events.push('next'); return item++ < 2 ? buffer([item], () => {releaseCount++}) : null; },
    cancel() { cancelled = true; events.push('cancel'); },
    async observe(timeout) { events.push(`observe:${timeout}`); return receipt; },
    checkDeadline() { events.push('deadline'); },
    close() { events.push('free'); },
  };
  const endpoint = {
    connectionSnapshot(){return null}, async prepare() { events.push('prepare'); return {serverId:'server',serverInstanceId:'instance',connectionId:1n}; },
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
  assert.deepEqual(events.slice(0,5), ['prepare','begin','encode','deadline','charge:1']);
  assert.deepEqual(await stream.close(0), receipt);
  assert.equal(cancelled,true);
  assert.ok(events.indexOf('cancel') < events.indexOf('observe:0'));
});

test('two unary calls enter native begin without a JS call chain', async () => {
  let begins = 0;
  let releaseCount = 0;
  const pending = [];
  const endpoint = {
    connectionSnapshot(){return null}, async prepare() {return {serverId:'s',serverInstanceId:'i',connectionId:1n}},
    beginCall() {
      begins++;
      let finish;
      const gate = new Promise(resolve => {finish = resolve});
      pending.push(finish);
      return {
        async chargeInput() {}, async dispatch() {await gate;return nativeTarget},
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
    connectionSnapshot(){return null}, async prepare(...args){assert.deepEqual(args,['route',contract,undefined]);return {serverId:'s',serverInstanceId:'i',connectionId:1n}},
    beginCall(){throw Error('unexpected')}, async close(){}, deactivate(){},
  };
  const transport=createIpcEncodedTransport('ipc://test',{
    connect:async (address,timeout,options)=>{seen.push({address,timeout,options});return endpoint},
    connectTimeoutMs:17,
    expectedServerIdentity:{serverId:'s',serverInstanceId:'i'},
  });
  await transport.prepare('route',contract);
  assert.deepEqual(seen,[{address:'ipc://test',timeout:17,options:{
    connectTimeoutMs:17,
    expectedServerIdentity:{serverId:'s',serverInstanceId:'i'},
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
    async chargeInput() {}, async dispatch() { dispatched++; return nativeTarget; }, async next() { return null; },
    cancel() { cancelled++; }, async observe() { return {completed:true,error:null,completeItems:0n}; },
    checkDeadline() {}, close() {},
  };
  const endpoint = {
    connectionSnapshot(){return null}, async prepare() { await gate; return {serverId:'s',serverInstanceId:'i',connectionId:1n}; },
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
    connectionSnapshot(){return null}, async prepare() { await gate; return {serverId:'s',serverInstanceId:'i',connectionId:1n}; },
    beginCall() { return {cancel(){},async chargeInput(){},async dispatch(){return nativeTarget},async next(){return null},async observe(ms){assert.equal(ms,0);return receipt},checkDeadline(){},close(){}}; },
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
  const endpoint={connectionSnapshot(){return null}, async prepare(){await gate;return {serverId:'s',serverInstanceId:'i',connectionId:1n}},
    beginCall(){return {cancel(){},async chargeInput(){},async dispatch(){return nativeTarget},async next(){return null},async observe(){return receipt},checkDeadline(){},close(){}}},
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
  const call={async chargeInput(){},async dispatch(){return nativeTarget},async next(){return buffer([1],()=>released++)},
    cancel(){},async observe(){return receipt},close(){},checkDeadline(){if(expired)throw timeout}};
  const endpoint={connectionSnapshot(){return null}, async prepare(){return {serverId:'s',serverInstanceId:'i',connectionId:1n}},
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
  const call={async chargeInput(){},async dispatch(){return nativeTarget},async next(){throw failure},cancel(){},
    async observe(){return receipt},checkDeadline(){},close(){}};
  const endpoint={connectionSnapshot(){return null}, async prepare(){return {serverId:'s',serverInstanceId:'i',connectionId:1n}},
    beginCall(){return call},async close(){},deactivate(){}};
  const stream=createIpcEncodedTransport('ipc://test',{connect:async()=>endpoint}).stream('route',contract,'steps',()=>Uint8Array.of(),x=>x);
  await assert.rejects(stream.next(),e=>e instanceof C2CoreError && e.code===3 && e.retrySafe===false && e.details.method==='steps');
});

test('deadline stops waiting without cancelling the dispatched native call', async () => {
  let cancellations=0;
  const deadline=new Error('deadline');
  deadline.nativeDetails={message:'deadline',code:715,canonicalName:'CallDeadlineExceeded',transportPhase:'dispatch_uncertain'};
  const call={async chargeInput(){},async dispatch(){return nativeTarget},async next(){throw deadline},cancel(){cancellations++},
    async observe(){return {completed:false,error:null,completeItems:null}},checkDeadline(){},close(){}};
  const endpoint={connectionSnapshot(){return null}, async prepare(){return {serverId:'s',serverInstanceId:'i',connectionId:1n}},
    beginCall(){return call},async close(){},deactivate(){}};
  const stream=createIpcEncodedTransport('ipc://test',{connect:async()=>endpoint}).stream('route',contract,'steps',()=>Uint8Array.of(),x=>x);
  await assert.rejects(stream.next(),e=>e.code===715&&e.transportPhase==='dispatch_uncertain');
  assert.equal(cancellations,0);
  assert.deepEqual(await stream.close(0),{completed:false,error:null,completeItems:null});
  assert.equal(cancellations,1);
});

test('both relay subscription modes preserve unlimited resolution and cap it by a finite caller budget', async () => {
  for (const [factory, mode] of [
    [createHttpRelayEncodedTransport, 'explicit_relay'],
    [createRelayAwareHttpEncodedTransport, 'relay_aware'],
  ]) {
    const limits = [];
    const native = { startSeq: 1n, topic: 'state' };
    const endpoint = {
      async prepare(route, identity, limit) { limits.push(limit); },
      async openSubscription() { return native; },
    };
    const transport = factory('http://localhost:8300', {
      resolveTimeoutMs: 0,
      async connect(address, options, transportOptions) {
        assert.equal(transportOptions.mode, mode);
        return endpoint;
      },
    });
    assert.equal((await transport.subscribe('route', contract, 'state', x => x)).startSeq, 1n);
    assert.equal(limits[0], undefined);
    await transport.subscribe('route', contract, 'state', x => x, { timeoutMs: 5_000 });
    assert.ok(limits[1] > 0 && limits[1] <= 5_000);
  }
});

test('topic deadline after decoding drops owners and leaves cancellation to the caller', async () => {
  for (const hold of [false, true]) {
    let expired = false;
    let cancellations = 0;
    let payloadReleases = 0;
    let valueReleases = 0;
    const native = {
      startSeq: 1n, topic: 'state',
      async next() {
        return { done: false, value: { seq: 1n, dropped: 0n, value: buffer([1], () => payloadReleases++) } };
      },
      checkDeadline() {
        if (expired) {
          const deadline = new Error('deadline during decode');
          deadline.nativeDetails = { message: deadline.message, code: 715, canonicalName: 'CallDeadlineExceeded', transportPhase: 'dispatch_uncertain' };
          throw deadline;
        }
      },
      cancel() { cancellations++; },
      async close() { return receipt; },
    };
    const endpoint = {
      connectionSnapshot() { return null; },
      async prepare() { return nativeTarget; },
      async openSubscription() { return native; },
    };
    const transport = createIpcEncodedTransport('ipc://test', { connect: async () => endpoint });
    const sub = await transport.subscribe('route', contract, 'state', () => {
      expired = true;
      return { release() { valueReleases++; } };
    }, {}, hold);
    await assert.rejects(sub.next(), error => error.code === 715 && error.transportPhase === 'dispatch_uncertain');
    assert.equal(payloadReleases, 1);
    assert.equal(valueReleases, 1);
    assert.equal(cancellations, 0);
    assert.deepEqual(await sub.close(), receipt);
  }
});

test('deadline during dispatch retains native call for later observation', async () => {
  let cancellations=0, observations=0;
  const deadline=new Error('deadline');
  deadline.nativeDetails={message:'deadline',code:715,canonicalName:'CallDeadlineExceeded',transportPhase:'dispatch_uncertain'};
  const call={async chargeInput(){},async dispatch(){throw deadline},async next(){throw Error('not reached')},cancel(){cancellations++},
    async observe(){observations++;return {completed:false,error:null,completeItems:null}},checkDeadline(){},close(){}};
  const endpoint={connectionSnapshot(){return null}, async prepare(){return {serverId:'s',serverInstanceId:'i',connectionId:1n}},
    beginCall(){return call},async close(){},deactivate(){}};
  const stream=createIpcEncodedTransport('ipc://test',{connect:async()=>endpoint}).stream('route',contract,'steps',()=>Uint8Array.of(),x=>x);
  await assert.rejects(stream.next(),e=>e.code===715);
  assert.equal(cancellations,0);
  assert.deepEqual(await stream.close(0),{completed:false,error:null,completeItems:null});
  assert.equal(observations,1);
});

test('relay-aware canonical 701 POST is returned once and the next call resolves again', async () => {
  let resolves=0, posts=0;
  const utf8=new TextEncoder();
  const reply=(status,text,bytes)=>{
    const raw=bytes??utf8.encode(text);
    return {
      status, headers:{get(name){return name==='content-length'?String(raw.byteLength):null}},
      async arrayBuffer(){return raw.slice().buffer},
    };
  };
  const route=()=>({
    name:'route',relay_url:'http://localhost:8301',
    crm_ns:'test',crm_name:'Test',crm_ver:'0.1.0',abi_hash:hash,signature_hash:hash,
    max_payload_size:100,
  });
  const fetch=async (url,init)=>{
    if(url.includes('/_resolve/')) {resolves++;return reply(200,JSON.stringify([route()]));}
    if(url.includes('/_probe/')) return reply(200,'');
    if(init.method==='POST') {
      posts++;
      return posts===1
        ? reply(404,JSON.stringify({version:1,code:701,name:'ResourceNotFound',message:'missing',details:{dispatch_phase:'pre_dispatch'}}))
        : reply(200,'',Uint8Array.of(9));
    }
    throw Error(`unexpected ${url}`);
  };
  const transport=createRelayAwareHttpEncodedTransport('http://localhost:8300',{fetch});
  await assert.rejects(transport.call('route',contract,'ping',()=>Uint8Array.of()), e=>e.status===404&&e.retrySafe===true);
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
  const route={name:'route',relay_url:'http://localhost:8301',
    ipc_address:'ipc://local',server_id:'s',server_instance_id:'i',
    crm_ns:'test',crm_name:'Test',crm_ver:'0.1.0',abi_hash:hash,signature_hash:hash,max_payload_size:100};
  const fetch=async (_url,init)=>{
    if(init.method==='POST') posts++;
    return {status:200, async arrayBuffer(){return new TextEncoder().encode(JSON.stringify([route])).buffer}};
  };
  const connect=async()=>({
    connectionSnapshot(){return null}, async prepare(){throw nativeFailure}, beginCall(){throw Error('unexpected dispatch')},
    async close(){}, deactivate(){},
  });
  const transport=createRelayAwareHttpEncodedTransport('http://localhost:8300',{fetch,ipc:{connect}});
  await assert.rejects(transport.call('route',contract,'ping',()=>Uint8Array.of()), e=>{
    assert.equal(JSON.parse(e.body).name,'FallbackDenied');
    assert.deepEqual(JSON.parse(e.body).details,{route:'route'});
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
    const route={name:'route',relay_url:'http://localhost:8301',
      ipc_address:'ipc://local',server_id:'s',server_instance_id:'i',
      crm_ns:'test',crm_name:'Test',crm_ver:'0.1.0',abi_hash:hash,signature_hash:hash,max_payload_size:100};
    const fetch=async (_url,init)=>{
      if(init.method==='POST') posts++; else resolves++;
      return {status:200,async arrayBuffer(){return new TextEncoder().encode(JSON.stringify(alternatives?[route,{...route,ipc_address:undefined,server_id:undefined,server_instance_id:undefined}]:[route])).buffer}};
    };
    const endpoint={connectionSnapshot(){return null}, async prepare(){throw failure},beginCall(){throw Error('unexpected dispatch')},async close(){if(closeFails)throw cleanup},deactivate(){}};
    const transport=createRelayAwareHttpEncodedTransport('http://localhost:8300',{fetch,ipc:{connect:async()=>endpoint}});
    await assert.rejects(transport.call('route',contract,'ping',()=>new Uint8Array()),e=>e.nativeDetails===failure.nativeDetails && e.cause===failure && (!closeFails||e.cleanupError===cleanup));
    assert.equal(resolves,1); assert.equal(posts,0);
  }
});

test('relay-aware close zero does not wait for route resolution', async () => {
  let resolveRoute;
  const gate=new Promise(resolve=>{resolveRoute=resolve});
  let encoded=0, dispatched=0;
  const route={name:'route',relay_url:'http://localhost:8301',
    ipc_address:'ipc://local',server_id:'s',server_instance_id:'i',
    crm_ns:'test',crm_name:'Test',crm_ver:'0.1.0',abi_hash:hash,signature_hash:hash,max_payload_size:100};
  const fetch=async()=>{await gate;return {status:200,async arrayBuffer(){return new TextEncoder().encode(JSON.stringify([route])).buffer}}};
  const call={async chargeInput(){},async dispatch(){dispatched++;return nativeTarget},async next(){return null},cancel(){},
    async observe(){return {completed:true,error:null,completeItems:0n}},checkDeadline(){},close(){}};
  const endpoint={connectionSnapshot(){return null}, async prepare(){return {serverId:'s',serverInstanceId:'i',connectionId:1n}},
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
  const nativeCall={async chargeInput(){},async dispatch(){return nativeTarget},
    next(){enteredResolve();return new Promise(resolve=>{finishNext=resolve})},
    cancel(){cancelled++;},close(){freed++;finishNext?.(null)},checkDeadline(){},
    async observe(){return {completed:false,error:null,completeItems:null}}};
  const endpoint={connectionSnapshot(){return null}, async prepare(){return {serverId:'s',serverInstanceId:'i',connectionId:1n}},
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

test('generated artifact error-code table validates canonical HTTP relay envelopes at runtime', async () => {
  const route={name:'route',relay_url:'http://localhost:8301',
    crm_ns:'test',crm_name:'Test',crm_ver:'0.1.0',abi_hash:hash,signature_hash:hash,max_payload_size:100};
  const reply=(status,body)=>({status,async arrayBuffer(){return new TextEncoder().encode(body).buffer}});
  for (const [status,body,expectPhase,expectRetry] of [
    [503, JSON.stringify({version:1,code:717,name:'CallCapacityExceeded',message:'cap',details:{dispatch_phase:'pre_dispatch'}}), 'pre_dispatch', true],
    [503, JSON.stringify({version:1,code:717,name:'ResourceNotFound',message:'wrong',details:{dispatch_phase:'pre_dispatch'}}), 'dispatch_uncertain', false],
    [503, '{"version":1,"code":717.5,"name":"CallCapacityExceeded","message":"float","details":{"dispatch_phase":"pre_dispatch"}}', 'dispatch_uncertain', false],
    [503, JSON.stringify([1,717,'CallCapacityExceeded','array',{dispatch_phase:'pre_dispatch'}]), 'dispatch_uncertain', false],
    [503, JSON.stringify({version:1,code:720,name:'FutureCode',message:'future',details:{dispatch_phase:'pre_dispatch'}}), 'dispatch_uncertain', false],
  ]) {
    const transport=createRelayAwareHttpEncodedTransport('http://localhost:8300',{
      fetch:async(url,init)=>{
        if(url.includes('/_resolve/')) return reply(200, JSON.stringify([route]));
        if(url.includes('/_probe/')) return reply(200, '');
        assert.equal(init.method, 'POST');
        return reply(status, body);
      },
    });
    try {
      await assert.rejects(transport.call('route',contract,'ping',()=>new Uint8Array()), err => {
        assert.equal(err.transportPhase, expectPhase);
        assert.equal(err.retrySafe, expectRetry);
        return true;
      });
    } finally {
      await transport.close();
    }
  }
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
