import assert from 'node:assert/strict';
import { spawnSync } from 'node:child_process';
import { mkdtempSync, readFileSync, rmSync, writeFileSync } from 'node:fs';
import { createRequire } from 'node:module';
import { tmpdir } from 'node:os';
import { resolve } from 'node:path';
import test from 'node:test';

const require = createRequire(import.meta.url);
const addon = require('../dist/native/c2_mem_ffi_node.node');
const nativeLibrary = resolve(import.meta.dirname, '../dist/native',
  process.platform === 'win32' ? 'c2_mem_ffi.dll' :
    process.platform === 'darwin' ? 'libc2_mem_ffi.dylib' : 'libc2_mem_ffi.so');

test('ABI 7 is rejected before any other symbol is loaded', () => {
  const directory = mkdtempSync(resolve(tmpdir(), 'c2-abi7-addon-'));
  try {
    const source = resolve(directory, 'stale.rs');
    const library = resolve(directory, process.platform === 'win32' ? 'stale.dll' :
      process.platform === 'darwin' ? 'stale.dylib' : 'stale.so');
    writeFileSync(source, '#[unsafe(no_mangle)] pub extern "C" fn c2_mem_ffi_abi_version() -> u32 { 7 }');
    const result = spawnSync(process.env.RUSTC ?? 'rustc',
      ['--edition=2024', '--crate-type=cdylib', source, '-o', library], { encoding: 'utf8' });
    assert.equal(result.status, 0, result.stderr);
    assert.throws(() => addon.load(library), /ABI version 7.*expected version 8/);
  } finally {
    rmSync(directory, { recursive: true, force: true });
  }
});

test('ABI 8 exposes native domain, endpoint, call and captured configuration', async () => {
  const symbols = addon.load(nativeLibrary);
  assert.equal(symbols.c2_mem_ffi_abi_version(), 8);
  const context = symbols.captureEndpointContext(null);
  assert.equal(typeof context.endpointName('ipc://addon-shape'), 'string');
  assert.equal(typeof context.namespaceId(), 'string');
  const domain = symbols.createClientDomain(context, {});
  try {
    assert.equal(typeof domain.stats(), 'object');
    const endpoint = domain.createEndpoint('ipc://addon-shape', {});
    try {
      assert.equal(typeof endpoint.prepare, 'function');
      assert.equal(typeof endpoint.beginCall, 'function');
      assert.equal(typeof endpoint.deactivate, 'function');
      assert.equal(typeof endpoint.close, 'function');
    } finally {
      endpoint.deactivate();
      await endpoint.close();
    }
  } finally {
    domain.close();
    context.close();
  }
});

test('native next uses an external buffer and releases it once after detaching aliases', {
  skip: process.platform === 'win32' && 'The C fixture uses pthreads; the real Windows addon is covered by native integration.',
}, async () => {
  const directory = mkdtempSync(resolve(tmpdir(), 'c2-abi8-buffer-'));
  try {
    const addonSource = readFileSync(new URL('../native/node_c2_mem_ffi_loader.c', import.meta.url), 'utf8');
    const required = [...addonSource.matchAll(/LOAD_REQUIRED\(\w+, "([^"]+)"\)/g)].map(match => match[1]);
    const custom = new Set([
      'c2_mem_ffi_abi_version', 'c2_mem_ffi_client_domain_new', 'c2_mem_ffi_client_domain_free',
      'c2_mem_ffi_client_domain_stats', 'c2_mem_ffi_endpoint_context_new',
      'c2_mem_ffi_call_begin', 'c2_mem_ffi_call_next', 'c2_mem_ffi_buffer_data',
      'c2_mem_ffi_buffer_release', 'c2_mem_ffi_result_free', 'c2_mem_ffi_error_parts',
      'c2_mem_ffi_error_free', 'c2_mem_ffi_call_check_deadline', 'c2_mem_ffi_call_observe',
      'c2_mem_ffi_call_free',
    ]);
    const source = resolve(directory, 'fixture.c');
    const library = resolve(directory, process.platform === 'darwin' ? 'fixture.dylib' : 'fixture.so');
    writeFileSync(source, `
      #include <stdint.h>
      #include <stddef.h>
      #include <stdio.h>
      #include <stdlib.h>
      #include <pthread.h>
      typedef struct { void *buffer; void *error; const char *json; } Result;
      typedef void (*Complete)(void *, Result *);
      static unsigned char bytes[] = {9,8,7};
      static unsigned releases = 0;
      static unsigned call_frees = 0;
      static char stats[80];
      uint32_t c2_mem_ffi_abi_version(void) { return 8; }
      uint32_t c2_mem_ffi_client_domain_new(void *ctx, const char *options, void **out, void **error) {
        (void)ctx; (void)options; *out = (void *)1; *error = NULL; return 0;
      }
      void c2_mem_ffi_client_domain_free(void *domain) { (void)domain; }
      uint32_t c2_mem_ffi_client_domain_stats(void *domain, Result **out) {
        (void)domain; sprintf(stats, "{\\"releases\\":%u,\\"callFrees\\":%u,\\"first\\":%u}", releases, call_frees, bytes[0]);
        *out = calloc(1, sizeof(Result)); (*out)->json = stats; return 0;
      }
      uint32_t c2_mem_ffi_endpoint_context_new(void *domain, const char *address, const char *options,
          void **out, void **error) {
        (void)domain; (void)address; (void)options; *out=(void *)2; *error=NULL; return 0;
      }
      typedef struct {
        pthread_mutex_t mutex;
        pthread_cond_t released;
        unsigned references;
        uint8_t stream;
        int closed;
      } Call;
      static void drop_call(Call *call) {
        pthread_mutex_lock(&call->mutex);
        int destroy = --call->references == 0;
        pthread_mutex_unlock(&call->mutex);
        if (destroy) {
          pthread_cond_destroy(&call->released);
          pthread_mutex_destroy(&call->mutex);
          free(call);
        }
      }
      uint32_t c2_mem_ffi_call_begin(void *endpoint, const char *route, const char *method,
          uint8_t stream, const uint64_t *timeout, void **out, void **error) {
        (void)endpoint; (void)route; (void)method; (void)timeout;
        Call *call = calloc(1, sizeof(Call));
        pthread_mutex_init(&call->mutex, NULL); pthread_cond_init(&call->released, NULL);
        call->references = 1; call->stream = stream;
        *out=call; *error=NULL; return 0;
      }
      typedef struct { Complete complete; void *user; Call *call; } Pending;
      static void *complete_next(void *raw) {
        Pending *pending = raw; Call *call = pending->call;
        pthread_mutex_lock(&call->mutex);
        while (call->stream && !call->closed) pthread_cond_wait(&call->released, &call->mutex);
        pthread_mutex_unlock(&call->mutex);
        Result *result = calloc(1,sizeof(Result)); result->buffer=(void *)4;
        pending->complete(pending->user,result); drop_call(call); free(pending); return NULL;
      }
      uint32_t c2_mem_ffi_call_next(Call *call, Complete complete, void *user) {
        pthread_mutex_lock(&call->mutex); call->references++; pthread_mutex_unlock(&call->mutex);
        Pending *pending=malloc(sizeof(Pending)); *pending=(Pending){complete,user,call};
        pthread_t thread; if (pthread_create(&thread,NULL,complete_next,pending)) abort();
        pthread_detach(thread); return 0;
      }
      uint32_t c2_mem_ffi_buffer_data(void *buffer, const unsigned char **out, size_t *len) {
        (void)buffer; *out=bytes; *len=sizeof(bytes); return 0;
      }
      void c2_mem_ffi_buffer_release(void *buffer) { (void)buffer; releases++; }
      void c2_mem_ffi_call_free(Call *call) {
        call_frees++;
        pthread_mutex_lock(&call->mutex); call->closed = 1;
        pthread_cond_broadcast(&call->released); pthread_mutex_unlock(&call->mutex);
        drop_call(call);
      }
      uint32_t c2_mem_ffi_call_check_deadline(void *call, void **error) {
        (void)call; *error=(void *)5; return 2;
      }
      uint32_t c2_mem_ffi_call_observe(void *call, const uint64_t *timeout, Complete complete, void *user) {
        (void)call; (void)timeout; Result *result=calloc(1,sizeof(Result));
        result->json="{\\"completed\\":true,\\"error\\":{\\"message\\":\\"ended\\",\\"code\\":400,\\"transportPhase\\":\\"dispatch_uncertain\\"},\\"completeItems\\":\\"3\\"}";
        complete(user,result); return 0;
      }
      const char *c2_mem_ffi_error_parts(void *error) {
        (void)error; return "{\\"message\\":\\"deadline\\",\\"code\\":401,\\"transportPhase\\":\\"pre_dispatch\\"}";
      }
      void c2_mem_ffi_error_free(void *error) { (void)error; }
      void c2_mem_ffi_result_free(Result *result) { free(result); }
      ${required.filter(name => !custom.has(name)).map(name => `void ${name}(void) {}`).join('\n')}
    `);
    const build = spawnSync(process.env.CC ?? 'cc',
      ['-std=c11', '-shared', '-fPIC', '-pthread', source, '-o', library], { encoding: 'utf8' });
    assert.equal(build.status, 0, build.stderr);
    const symbols = addon.load(library);
    const domain = symbols.createClientDomain(null, {});
    const endpoint = domain.createEndpoint('ipc://buffer-fixture', {});
    const call = endpoint.beginCall('route', 'method', false, null);
    assert.throws(() => call.checkDeadline(), error => {
      assert.equal(error.nativeDetails.code, 401);
      assert.equal(error.nativeDetails.transportPhase, 'pre_dispatch');
      return true;
    });
    const receipt = await call.observe(0);
    assert.equal(receipt.completed, true);
    assert.equal(receipt.completeItems, 3n);
    assert.ok(receipt.error instanceof Error);
    assert.equal(receipt.error.nativeDetails.transportPhase, 'dispatch_uncertain');
    const item = await call.next();
    assert.ok(item instanceof Uint8Array);
    assert.deepEqual([...item], [9, 8, 7]);
    const backing = item.buffer;
    const alias = new Uint8Array(backing);
    Object.defineProperty(item, 'buffer', { value: new ArrayBuffer(0) });
    item[0] = 4;
    assert.equal(domain.stats().first, 4, 'the view points at native bytes');
    assert.throws(() => item.release.call(domain), /Invalid native buffer receiver/);
    item.release();
    item.release();
    assert.equal(backing.byteLength, 0, 'release detaches the intrinsic backing despite a shadowed buffer property');
    assert.equal(item.byteLength, 0);
    assert.equal(alias.byteLength, 0);
    assert.equal(domain.stats().releases, 1);
    call.close();
    domain.close();
    const addonPath = resolve(import.meta.dirname, '../dist/native/c2_mem_ffi_node.node');
    const gcScript = `
      import assert from 'node:assert/strict';
      import { createRequire } from 'node:module';
      const addon = createRequire(import.meta.url)(process.argv[1]);
      const domain = addon.load(process.argv[2]).createClientDomain(null, {});
      const endpoint = domain.createEndpoint('ipc://buffer-fixture', {});
      const call = endpoint.beginCall('route', 'method', false, null);
      let item = await call.next();
      assert.equal(item[0], 9);
      item = null;
      for (let attempt = 0; attempt < 100 && domain.stats().releases === 0; attempt++) {
        global.gc(); await new Promise(resolve => setImmediate(resolve));
      }
      assert.equal(domain.stats().releases, 1, 'GC must release the abandoned Rust buffer once');
      call.close(); domain.close();
    `;
    const gc = spawnSync(process.execPath,
      ['--expose-gc', '--input-type=module', '-e', gcScript, addonPath, library], { encoding: 'utf8' });
    assert.equal(gc.status, 0, `${gc.signal ?? ''} ${gc.stdout}${gc.stderr}`);
    const releasedGcScript = `
      import assert from 'node:assert/strict';
      import { createRequire } from 'node:module';
      const addon = createRequire(import.meta.url)(process.argv[1]);
      const domain = addon.load(process.argv[2]).createClientDomain(null, {});
      const endpoint = domain.createEndpoint('ipc://released-buffer-fixture', {});
      const call = endpoint.beginCall('route', 'method', false, null);
      const item = await call.next();
      let backing = item.buffer;
      let alias = new Uint8Array(backing);
      item.release();
      assert.equal(backing.byteLength, 0);
      assert.equal(alias.byteLength, 0);
      backing = null;
      alias = null;
      for (let attempt = 0; attempt < 100; attempt++) {
        global.gc(); await new Promise(resolve => setImmediate(resolve));
      }
      item.release();
      assert.equal(domain.stats().releases, 1, 'release, GC and repeated release have one Rust owner');
      call.close(); domain.close();
    `;
    const releasedGc = spawnSync(process.execPath,
      ['--expose-gc', '--input-type=module', '-e', releasedGcScript, addonPath, library], { encoding: 'utf8' });
    assert.equal(releasedGc.status, 0, `${releasedGc.signal ?? ''} ${releasedGc.stdout}${releasedGc.stderr}`);
    const pendingScript = `
      import assert from 'node:assert/strict';
      import { createRequire } from 'node:module';
      const addon = createRequire(import.meta.url)(process.argv[1]);
      const domain = addon.load(process.argv[2]).createClientDomain(null, {});
      const endpoint = domain.createEndpoint('ipc://pending-fixture', {});
      function startNext() {
        const call = endpoint.beginCall('route', 'method', true, null);
        return call.next();
      }
      const pending = startNext();
      for (let attempt = 0; attempt < 100 && domain.stats().callFrees === 0; attempt++) {
        global.gc(); await new Promise(resolve => setImmediate(resolve));
      }
      assert.equal(domain.stats().callFrees, 1, 'pending next must not retain its JS call receiver');
      const item = await pending;
      assert.deepEqual([...item], [9, 8, 7]);
      item.release(); domain.close();
    `;
    const pendingGc = spawnSync(process.execPath,
      ['--expose-gc', '--input-type=module', '-e', pendingScript, addonPath, library], { encoding: 'utf8' });
    assert.equal(pendingGc.status, 0, `${pendingGc.signal ?? ''} ${pendingGc.stdout}${pendingGc.stderr}`);
    // The test-only addon makes backing finalization happen at detach, while
    // the Uint8Array wrapper remains live. This is an allowed N-API ordering.
    const monitoredSource = `
      #ifndef _WIN32
      #define _POSIX_C_SOURCE 200809L
      #endif
      #include <node_api.h>
      #include <stdbool.h>
      #include <stdlib.h>
      static napi_finalize watched_finalize;
      static void *watched_data, *watched_hint;
      static bool watched_ran, watched_freed;
      static void watched_free(void *ptr) {
        if (ptr == watched_hint) { watched_freed = true; return; }
        free(ptr);
      }
      static void watched_finalizer(napi_env env, void *data, void *hint) {
        if (!watched_ran) { watched_ran = true; watched_finalize(env, data, hint); }
      }
      static napi_status watched_create(napi_env env, void *data, size_t length,
          napi_finalize finalize, void *hint, napi_value *out) {
        watched_data = data; watched_hint = hint; watched_finalize = finalize;
        return napi_create_external_arraybuffer(env, data, length, watched_finalizer, hint, out);
      }
      static napi_status watched_detach(napi_env env, napi_value arraybuffer) {
        napi_status status = napi_detach_arraybuffer(env, arraybuffer);
        if (status == napi_ok && !watched_ran) watched_finalizer(env, watched_data, watched_hint);
        return status;
      }
      #define free watched_free
      #define napi_create_external_arraybuffer watched_create
      #define napi_detach_arraybuffer watched_detach
      ${addonSource.replace('NAPI_MODULE(c2_mem_ffi_node, Init)', `
        static napi_value watched_probe(napi_env env, napi_callback_info info) {
          (void)info; napi_value out; napi_get_boolean(env, watched_freed, &out); return out;
        }
        static napi_value watched_init(napi_env env, napi_value exports) {
          napi_value value = Init(env, exports), probe;
          napi_create_function(env, "ownerFreed", NAPI_AUTO_LENGTH, watched_probe, NULL, &probe);
          napi_set_named_property(env, value, "ownerFreed", probe);
          return value;
        }
        NAPI_MODULE(c2_mem_ffi_node, watched_init)
      `)}
    `;
    const monitoredFile = resolve(directory, 'monitored.c');
    const monitoredAddon = resolve(directory, 'monitored.node');
    writeFileSync(monitoredFile, monitoredSource);
    const monitoredArgs = [
      '-std=c11', '-Werror=incompatible-pointer-types', '-O2', '-fPIC', '-shared',
      `-I${resolve(process.execPath, '..', '..', 'include', 'node')}`,
      `-I${resolve(import.meta.dirname, '../../../include')}`,
      '-DNAPI_VERSION=10', monitoredFile, '-o', monitoredAddon,
    ];
    if (process.platform === 'darwin') monitoredArgs.splice(4, 0, '-undefined', 'dynamic_lookup');
    else monitoredArgs.push('-ldl');
    const monitoredBuild = spawnSync(process.env.CC ?? 'cc', monitoredArgs, { encoding: 'utf8' });
    assert.equal(monitoredBuild.status, 0, monitoredBuild.stderr);
    const monitoredScript = `
      import assert from 'node:assert/strict';
      import {createRequire} from 'node:module';
      const addon = createRequire(import.meta.url)(process.argv[1]);
      const domain = addon.load(process.argv[2]).createClientDomain(null, {});
      const endpoint = domain.createEndpoint('ipc://monitored-buffer', {});
      const call = endpoint.beginCall('route', 'method', false, null);
      const item = await call.next();
      const alias = new Uint8Array(item.buffer);
      item.release();
      assert.equal(alias.byteLength, 0);
      assert.equal(addon.ownerFreed(), false, 'the live view still owns its BufferOwner after backing finalization');
      global.gc(); await new Promise(resolve => setImmediate(resolve));
      item.release();
      assert.equal(domain.stats().releases, 1);
      call.close(); domain.close();
    `;
    const monitored = spawnSync(process.execPath,
      ['--expose-gc', '--input-type=module', '-e', monitoredScript, monitoredAddon, library], { encoding: 'utf8' });
    assert.equal(monitored.status, 0, `${monitored.signal ?? ''} ${monitored.stdout}${monitored.stderr}`);
    // Force the N-API failure before ArrayBuffer ownership is established.
    const failureSource = resolve(directory, 'arraybuffer-failure.c');
    const failureAddon = resolve(directory, 'arraybuffer-failure.node');
    writeFileSync(failureSource, `
      #ifndef _WIN32
      #define _POSIX_C_SOURCE 200809L
      #endif
      #include <node_api.h>
      static napi_status fail_external_arraybuffer(napi_env env, void *data, size_t length,
          napi_finalize finalize, void *hint, napi_value *result) {
        (void)data; (void)length; (void)finalize; (void)hint; (void)result;
        napi_throw_error(env, NULL, "Injected external ArrayBuffer creation failure.");
        return napi_pending_exception;
      }
      #define napi_create_external_arraybuffer fail_external_arraybuffer
      ${addonSource}
    `);
    const failureArgs = [
      '-std=c11', '-Werror=incompatible-pointer-types', '-O2', '-fPIC', '-shared',
      `-I${resolve(process.execPath, '..', '..', 'include', 'node')}`,
      `-I${resolve(import.meta.dirname, '../../../include')}`,
      '-DNAPI_VERSION=10', failureSource, '-o', failureAddon,
    ];
    if (process.platform === 'darwin') failureArgs.splice(4, 0, '-undefined', 'dynamic_lookup');
    else failureArgs.push('-ldl');
    const failureBuild = spawnSync(process.env.CC ?? 'cc', failureArgs, { encoding: 'utf8' });
    assert.equal(failureBuild.status, 0, failureBuild.stderr);
    const failureScript = `
      import assert from 'node:assert/strict';
      import { createRequire } from 'node:module';
      const addon = createRequire(import.meta.url)(process.argv[1]);
      const domain = addon.load(process.argv[2]).createClientDomain(null, {});
      const endpoint = domain.createEndpoint('ipc://arraybuffer-failure', {});
      const call = endpoint.beginCall('route', 'method', false, null);
      await assert.rejects(call.next(), /Injected external ArrayBuffer creation failure/);
      assert.equal(domain.stats().releases, 1, 'failed ArrayBuffer creation releases its native owner once');
      global.gc(); await new Promise(resolve => setImmediate(resolve));
      assert.equal(domain.stats().releases, 1, 'no finalizer owns the rejected ArrayBuffer');
      call.close(); domain.close();
    `;
    const failure = spawnSync(process.execPath,
      ['--expose-gc', '--input-type=module', '-e', failureScript, failureAddon, library], { encoding: 'utf8' });
    assert.equal(failure.status, 0, `${failure.signal ?? ''} ${failure.stdout}${failure.stderr}`);
  } finally {
    rmSync(directory, { recursive: true, force: true });
  }
});

test('ABI 8 native completion survives Worker termination after a pending call', { timeout: 30_000 }, async () => {
  const directory = mkdtempSync(resolve(tmpdir(), 'c2-abi8-late-callback-'));
  try {
    const addonSource = readFileSync(new URL('../native/node_c2_mem_ffi_loader.c', import.meta.url), 'utf8');
    const required = [...addonSource.matchAll(/LOAD_REQUIRED\(\w+, "([^"]+)"\)/g)].map(match => match[1]);
    const custom = new Set([
      'c2_mem_ffi_abi_version', 'c2_mem_ffi_client_domain_new', 'c2_mem_ffi_client_domain_free',
      'c2_mem_ffi_client_domain_stats', 'c2_mem_ffi_endpoint_context_new',
      'c2_mem_ffi_endpoint_context_free', 'c2_mem_ffi_call_begin', 'c2_mem_ffi_call_next',
      'c2_mem_ffi_call_free', 'c2_mem_ffi_result_free',
    ]);
    const source = resolve(directory, 'late.rs');
    const library = resolve(directory, process.platform === 'win32' ? 'late.dll' :
      process.platform === 'darwin' ? 'late.dylib' : 'late.so');
    writeFileSync(source, `
      use std::{ffi::{c_char, c_void}, sync::Mutex};
      #[repr(C)] pub struct Result { buffer: *mut c_void, error: *mut c_void, json: *const c_char }
      type Complete = extern "C" fn(*mut c_void, *mut Result);
      static PENDING: Mutex<Option<(usize, usize)>> = Mutex::new(None);
      static STATS: &[u8] = b"{\\"completed\\":1}\\0";
      fn result(json: *const c_char) -> *mut Result {
        Box::into_raw(Box::new(Result { buffer: std::ptr::null_mut(), error: std::ptr::null_mut(), json }))
      }
      #[unsafe(no_mangle)] pub extern "C" fn c2_mem_ffi_abi_version() -> u32 { 8 }
      #[unsafe(no_mangle)] pub unsafe extern "C" fn c2_mem_ffi_client_domain_new(
          _: *const c_void, _: *const c_char, out: *mut *mut c_void, error: *mut *mut c_void) -> u32 {
        unsafe { *out = 1usize as *mut c_void; *error = std::ptr::null_mut(); } 0
      }
      #[unsafe(no_mangle)] pub extern "C" fn c2_mem_ffi_client_domain_free(_: *mut c_void) {}
      #[unsafe(no_mangle)] pub unsafe extern "C" fn c2_mem_ffi_endpoint_context_new(
          _: *const c_void, _: *const c_char, _: *const c_char,
          out: *mut *mut c_void, error: *mut *mut c_void) -> u32 {
        unsafe { *out = 2usize as *mut c_void; *error = std::ptr::null_mut(); } 0
      }
      #[unsafe(no_mangle)] pub extern "C" fn c2_mem_ffi_endpoint_context_free(_: *mut c_void) {}
      #[unsafe(no_mangle)] pub unsafe extern "C" fn c2_mem_ffi_call_begin(
          _: *const c_void, _: *const c_char, _: *const c_char, _: u8, _: *const u64,
          out: *mut *mut c_void, error: *mut *mut c_void) -> u32 {
        unsafe { *out = 3usize as *mut c_void; *error = std::ptr::null_mut(); } 0
      }
      #[unsafe(no_mangle)] pub extern "C" fn c2_mem_ffi_call_free(_: *mut c_void) {}
      #[unsafe(no_mangle)] pub extern "C" fn c2_mem_ffi_call_next(
          _: *const c_void, complete: Complete, user: *mut c_void) -> u32 {
        *PENDING.lock().unwrap() = Some((complete as usize, user as usize)); 0
      }
      #[unsafe(no_mangle)] pub unsafe extern "C" fn c2_mem_ffi_client_domain_stats(
          _: *const c_void, out: *mut *mut Result) -> u32 {
        let (complete, user) = PENDING.lock().unwrap().take().expect("pending native completion");
        let callback: Complete = unsafe { std::mem::transmute(complete) };
        callback(user as *mut c_void, result(std::ptr::null()));
        unsafe { *out = result(STATS.as_ptr().cast()); } 0
      }
      #[unsafe(no_mangle)] pub unsafe extern "C" fn c2_mem_ffi_result_free(ptr: *mut Result) {
        if !ptr.is_null() { unsafe { drop(Box::from_raw(ptr)); } }
      }
      ${required.filter(name => !custom.has(name)).map(name => `#[unsafe(no_mangle)] pub extern "C" fn ${name}() {}`).join('\n')}
    `);
    const build = spawnSync(process.env.RUSTC ?? 'rustc',
      ['--edition=2024', '--crate-type=cdylib', source, '-o', library], { encoding: 'utf8' });
    assert.equal(build.status, 0, build.stderr);
    const addonPath = resolve(import.meta.dirname, '../dist/native/c2_mem_ffi_node.node');
    const workerSource = `
      const { parentPort, workerData } = require('node:worker_threads');
      const addon = require(workerData.addonPath);
      const domain = addon.load(workerData.library).createClientDomain(null, {});
      const endpoint = domain.createEndpoint('ipc://late-callback', {});
      const call = endpoint.beginCall('route', 'method', false, null);
      call.next();
      parentPort.postMessage('pending');
    `;
    const childScript = `
      import assert from 'node:assert/strict';
      import {createRequire} from 'node:module';
      import {Worker} from 'node:worker_threads';
      const require = createRequire(import.meta.url);
      const addon = require(process.argv[1]);
      const worker = new Worker(${JSON.stringify(workerSource)}, {eval: true, execArgv: [], workerData: {
        addonPath: process.argv[1], library: process.argv[2],
      }});
      try {
        const pending = new Promise((resolve, reject) => {
          worker.once('message', resolve);
          worker.once('error', reject);
          worker.once('exit', code => reject(new Error('Worker exited before pending message: ' + code)));
        });
        assert.equal(await pending, 'pending');
        assert.equal(await worker.terminate(), 1);
        const domain = addon.load(process.argv[2]).createClientDomain(null, {});
        try { assert.equal(domain.stats().completed, 1); }
        finally { domain.close(); }
      } finally { await worker.terminate(); }
    `;
    const completed = spawnSync(process.execPath,
      ['--input-type=module', '-e', childScript, addonPath, library], {encoding: 'utf8', timeout: 10_000});
    assert.equal(completed.status, 0, `${completed.signal ?? ''} ${completed.stdout}${completed.stderr}`);
  } finally {
    rmSync(directory, { recursive: true, force: true });
  }
});
