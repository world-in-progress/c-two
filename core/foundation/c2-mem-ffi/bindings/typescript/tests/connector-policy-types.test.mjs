import assert from 'node:assert/strict';
import {spawnSync} from 'node:child_process';
import {mkdtempSync, rmSync, writeFileSync} from 'node:fs';
import {tmpdir} from 'node:os';
import {join} from 'node:path';
import {fileURLToPath} from 'node:url';
import test from 'node:test';
import * as binding from '../dist/index.js';

test('connector is the single factory facade and domain limits are factory-only', () => {
  assert.equal(binding.createNodeIpcRuntime, undefined);
  const directory = mkdtempSync(join(tmpdir(), 'c2-factory-types-'));
  try {
    const bindings = fileURLToPath(new URL('../dist/index.js', import.meta.url));
    const transport = fileURLToPath(new URL('../../../../c2-codegen/assets/typescript_transport.ts', import.meta.url));
    const compiler = fileURLToPath(new URL('../node_modules/typescript/bin/tsc', import.meta.url));
    const check = join(directory, 'check.ts');
    writeFileSync(check, `
      import {createNodeIpcConnector, type NativeEndpointOptions, type NativeClientDomainOptions} from ${JSON.stringify(bindings)};
      import type {C2IpcTransportOptions, C2IpcNativeEndpointOptions} from ${JSON.stringify(transport)};
      const limits: NativeClientDomainOptions = {memoryBudgetBytes: 1024n, fileSpillBudgetBytes: 0, maxOutstandingCalls: 64n};
      const connect = createNodeIpcConnector(limits);
      void connect('ipc://first', 100, {connectTimeoutMs: 50}).then(endpoint => {
        endpoint.beginCall('route', 'step', false, null, 'op-1');
        // @ts-expect-error a server stream cannot receive an operation key
        endpoint.beginCall('route', 'watch', true, null, 'op-1');
        // @ts-expect-error a subscription cannot receive an operation key
        endpoint.openSubscription('route', 'topic', {callId: 'op-1'});
      });
      // @ts-expect-error capacity belongs to the factory
      void connect('ipc://second', 100, {memoryBudgetBytes: 1024});
      // @ts-expect-error capacity belongs to the factory
      const endpoint: NativeEndpointOptions = {fileSpillBudgetBytes: 64};
      // @ts-expect-error generated connectors do not accept capacity per connection
      const generatedEndpoint: C2IpcNativeEndpointOptions = {maxOutstandingCalls: 1024};
      // @ts-expect-error generated transports do not own factory capacity
      const generatedTransport: C2IpcTransportOptions = {connect, memoryBudgetBytes: 64};
      // @ts-expect-error retired option
      createNodeIpcConnector({requestShmThreshold: 1024});
      // @ts-expect-error retired option
      createNodeIpcConnector({requestChunkSize: 64});
      // @ts-expect-error retired native factory option
      const threshold: NativeClientDomainOptions = {requestShmThreshold: 1024};
      // @ts-expect-error retired native factory option
      const chunk: NativeClientDomainOptions = {requestChunkSize: 64};
      // @ts-expect-error the duplicate factory facade has been removed
      import type {C2MemFfiNodeRuntime} from ${JSON.stringify(bindings)};
    `);
    const result = spawnSync(process.execPath, [compiler, '--noEmit', '--strict', '--target', 'ES2022',
      '--module', 'ES2022', '--moduleResolution', 'Bundler', '--lib', 'ES2022,DOM', '--typeRoots',
      join(directory, 'node_modules', '@types'), check], {encoding: 'utf8'});
    assert.equal(result.status, 0, result.stdout + result.stderr);
  } finally { rmSync(directory, {recursive: true, force: true}); }
});
