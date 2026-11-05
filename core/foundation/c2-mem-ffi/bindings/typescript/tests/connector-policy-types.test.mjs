import assert from 'node:assert/strict';
import {spawnSync} from 'node:child_process';
import {mkdtempSync, rmSync, writeFileSync} from 'node:fs';
import {tmpdir} from 'node:os';
import {join} from 'node:path';
import {fileURLToPath} from 'node:url';
import test from 'node:test';
import * as binding from '../dist/index.js';

test('connector is the single factory facade and transport policy is factory-only', () => {
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
      const policy: NativeClientDomainOptions = {requestShmThreshold: 1024, requestChunkSize: 64};
      const connect = createNodeIpcConnector(policy);
      void connect('ipc://first', 100, {connectTimeoutMs: 50});
      // @ts-expect-error policy belongs to the factory
      void connect('ipc://second', 100, {requestShmThreshold: 1024});
      // @ts-expect-error policy belongs to the factory
      const endpoint: NativeEndpointOptions = {requestChunkSize: 64};
      // @ts-expect-error generated connectors do not accept factory policy per connection
      const generatedEndpoint: C2IpcNativeEndpointOptions = {requestShmThreshold: 1024};
      // @ts-expect-error generated transports do not own factory policy
      const generatedTransport: C2IpcTransportOptions = {connect, requestChunkSize: 64};
      // @ts-expect-error the duplicate factory facade has been removed
      import type {C2MemFfiNodeRuntime} from ${JSON.stringify(bindings)};
    `);
    const result = spawnSync(process.execPath, [compiler, '--noEmit', '--strict', '--target', 'ES2022',
      '--module', 'ES2022', '--moduleResolution', 'Bundler', '--lib', 'ES2022,DOM', '--typeRoots',
      join(directory, 'node_modules', '@types'), check], {encoding: 'utf8'});
    assert.equal(result.status, 0, result.stdout + result.stderr);
  } finally { rmSync(directory, {recursive: true, force: true}); }
});
