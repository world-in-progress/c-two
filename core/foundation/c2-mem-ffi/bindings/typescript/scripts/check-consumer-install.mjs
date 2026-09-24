import { runNpm } from './npm-tools.mjs';
import { spawnSync } from 'node:child_process';
import { existsSync, mkdirSync, mkdtempSync, readFileSync, rmSync, writeFileSync } from 'node:fs';
import { tmpdir } from 'node:os';
import { dirname, isAbsolute, resolve } from 'node:path';
import { fileURLToPath } from 'node:url';

const scriptDir = dirname(fileURLToPath(import.meta.url));
const packageRoot = resolve(scriptDir, '..');
const tempRoot = mkdtempSync(resolve(tmpdir(), 'c2-mem-ffi-pack-'));

function run(command, args, options = {}) {
  const result = spawnSync(command, args, {
    encoding: 'utf8',
    ...options,
  });
  if (result.error) {
    throw result.error;
  }
  if (result.status !== 0) {
    process.stdout.write(result.stdout ?? '');
    process.stderr.write(result.stderr ?? '');
    throw new Error(`${command} ${args.join(' ')} failed with exit code ${result.status ?? 1}`);
  }
  return result;
}


try {
  let tarball = process.env.C2_MEM_FFI_PACKAGE_TARBALL;
  if (tarball === undefined) {
    const pack = runNpm(['pack', packageRoot, '--json', '--ignore-scripts'], {
      cwd: tempRoot,
    });
    let entries;
    try {
      entries = JSON.parse(pack.stdout);
    } catch (error) {
      process.stdout.write(pack.stdout);
      throw new Error(`Failed to parse npm pack JSON: ${String(error)}`);
    }
    const filename = entries?.[0]?.filename;
    if (typeof filename !== 'string' || filename.length === 0) {
      throw new Error('npm pack JSON did not contain a tarball filename.');
    }
    tarball = isAbsolute(filename) ? filename : resolve(tempRoot, filename);
  } else {
    tarball = resolve(tarball);
  }
  if (!existsSync(tarball)) {
    throw new Error(`C-Two package tarball does not exist: ${tarball}`);
  }

  let typescriptTarball = process.env.C2_TYPESCRIPT_PACKAGE_TARBALL;
  if (typescriptTarball === undefined) {
    const pack = runNpm([
      'pack',
      'typescript@5.9.3',
      '--json',
      '--pack-destination',
      tempRoot,
    ], {
      cwd: tempRoot,
    });
    let entries;
    try {
      entries = JSON.parse(pack.stdout);
    } catch (error) {
      process.stdout.write(pack.stdout);
      throw new Error(`Failed to parse TypeScript pack JSON: ${String(error)}`);
    }
    const filename = entries?.[0]?.filename;
    if (typeof filename !== 'string' || filename.length === 0) {
      throw new Error('TypeScript npm pack JSON did not contain a tarball filename.');
    }
    typescriptTarball = isAbsolute(filename) ? filename : resolve(tempRoot, filename);
  } else {
    typescriptTarball = resolve(typescriptTarball);
  }
  if (!existsSync(typescriptTarball)) {
    throw new Error(`TypeScript package tarball does not exist: ${typescriptTarball}`);
  }

  const consumerRoot = resolve(tempRoot, 'consumer');
  mkdirSync(consumerRoot);
  writeFileSync(resolve(consumerRoot, 'package.json'), JSON.stringify({
    name: 'c-two-c2-mem-ffi-package-consumer',
    private: true,
    type: 'module',
  }));
  runNpm(['install', tarball, typescriptTarball, '--ignore-scripts', '--no-audit', '--no-fund'], {
    cwd: consumerRoot,
  });
  const lock = JSON.parse(readFileSync(resolve(consumerRoot, 'package-lock.json'), 'utf8'));
  for (const [path, metadata] of Object.entries(lock.packages ?? {})) {
    if (metadata?.link === true) {
      throw new Error(`clean consumer contains a sibling link dependency at ${path}`);
    }
  }
  writeFileSync(resolve(consumerRoot, 'smoke.mjs'), `
import assert from 'node:assert/strict';

import {
  C2_MEM_FFI_ABI_VERSION,
  createC2MemFfiRequestPoolFromSymbols,
  createC2MemFfiResponsePoolFromSymbols,
  createNodeIpcConnect,
  loadBundledC2MemFfiNodeNativeSymbols,
} from '@c-two/c2-mem-ffi';

const { requestSymbols, responseSymbols, symbols } = loadBundledC2MemFfiNodeNativeSymbols();

assert.equal(C2_MEM_FFI_ABI_VERSION, 2);
assert.equal(symbols.c2_mem_ffi_abi_version(), C2_MEM_FFI_ABI_VERSION);
assert.equal(typeof createNodeIpcConnect, 'function');

const prefixSeed = process.pid.toString(36);
const requestPool = await createC2MemFfiRequestPoolFromSymbols(requestSymbols, {
  prefix: \`/cc2p\${prefixSeed}a\`,
  segmentSize: 65536,
  maxSegments: 1,
  minBlockSize: 4096,
});
try {
  const payload = new Uint8Array([1, 3, 5, 7]);
  const block = await requestPool.write(payload);
  assert.equal(block.dedicated, false);
  assert.equal(block.byteLength, payload.byteLength);
  await requestPool.release(block);
} finally {
  await requestPool.close?.();
}

const handle = (await symbols.c2_mem_ffi_request_pool_new(\`/cc2p\${prefixSeed}b\`, 65536, 1, 4096)).value;
try {
  const payload = new Uint8Array([2, 4, 6, 8]);
  const block = (await symbols.c2_mem_ffi_request_pool_write(handle, payload)).value;
  const destination = new Uint8Array(payload.byteLength);
  const readResult = await symbols.c2_mem_ffi_request_pool_read_local(handle, block, destination);
  assert.equal(readResult.status, 0);
  assert.equal(readResult.value, payload.byteLength);
  assert.deepEqual(Array.from(destination), Array.from(payload));
  await symbols.c2_mem_ffi_request_pool_release(handle, block);
} finally {
  await symbols.c2_mem_ffi_request_pool_destroy(handle);
}
try {
  await symbols.c2_mem_ffi_request_pool_prefix(handle);
  throw new Error('destroyed native request pool handle remained usable');
} catch (error) {
  if (!String(error).includes('closed')) {
    throw error;
  }
}
try {
  await symbols.c2_mem_ffi_request_pool_destroy(handle);
  throw new Error('destroyed native request pool handle could be destroyed twice');
} catch (error) {
  if (!String(error).includes('closed')) {
    throw error;
  }
}

const responseHandle = (await symbols.c2_mem_ffi_response_pool_new(\`/cc2p\${prefixSeed}d\`, 65536, 1, 4096)).value;
await symbols.c2_mem_ffi_response_pool_destroy(responseHandle);
try {
  await symbols.c2_mem_ffi_response_pool_read(
    responseHandle,
    { segmentIndex: 0, generation: 7, offset: 0, byteLength: 1, dedicated: false },
    new Uint8Array(1),
  );
  throw new Error('destroyed native response pool handle remained usable');
} catch (error) {
  if (!String(error).includes('closed')) {
    throw error;
  }
}
try {
  await symbols.c2_mem_ffi_response_pool_destroy(responseHandle);
  throw new Error('destroyed native response pool handle could be destroyed twice');
} catch (error) {
  if (!String(error).includes('closed')) {
    throw error;
  }
}

const fakeServerPool = await createC2MemFfiRequestPoolFromSymbols(requestSymbols, {
  prefix: \`/cc2p\${prefixSeed}c\`,
  segmentSize: 65536,
  maxSegments: 1,
  minBlockSize: 4096,
});
const responsePool = await createC2MemFfiResponsePoolFromSymbols(responseSymbols, {
  prefix: fakeServerPool.prefix,
  segmentSize: 65536,
  maxSegments: 1,
  minBlockSize: 4096,
});
try {
  for (const payload of [new Uint8Array([9, 7, 5, 3]), new Uint8Array(128 * 1024 + 3).fill(9)]) {
    const block = await fakeServerPool.write(payload);
    assert.equal(block.dedicated, payload.byteLength > 65536);
    assert.equal(block.generation === 0, block.dedicated);
    const destination = new Uint8Array(payload.byteLength);
    await responsePool.read(block, destination);
    assert.deepEqual(destination, payload);
    await responsePool.release(block);
    await fakeServerPool.forgetConsumed(block);
  }
} finally {
  await responsePool.close?.();
  await fakeServerPool.close?.();
}
`);
  writeFileSync(resolve(consumerRoot, 'smoke-types.mts'), `
import {
  C2_MEM_FFI_ABI_VERSION,
  type C2MemFfiNativeRequestBackend,
  type C2MemFfiNativeResponseBackend,
  type C2MemFfiNodeNativeSymbols,
  type C2MemFfiPoolConfig,
  type C2MemFfiRequestBlock,
  type C2MemFfiResponseBlock,
  createC2MemFfiNativeRequestBackend,
  createC2MemFfiNativeResponseBackend,
  createC2MemFfiRequestPoolFromSymbols,
  createC2MemFfiResponsePoolFromSymbols,
  createNodeIpcConnect,
  loadBundledC2MemFfiNodeNativeSymbols,
} from '@c-two/c2-mem-ffi';

const abiVersion: 2 = C2_MEM_FFI_ABI_VERSION;
const symbols: C2MemFfiNodeNativeSymbols = loadBundledC2MemFfiNodeNativeSymbols();
const connect = createNodeIpcConnect();
const poolConfig: C2MemFfiPoolConfig = {
  prefix: '/cc2ptype',
  segmentSize: 65536,
  maxSegments: 1,
  minBlockSize: 4096,
};

async function smokeTypes(): Promise<C2MemFfiResponseBlock> {
  const requestPool = await createC2MemFfiRequestPoolFromSymbols(symbols.requestSymbols, poolConfig);
  const responsePool = await createC2MemFfiResponsePoolFromSymbols(symbols.responseSymbols, poolConfig);
  const requestBackend: C2MemFfiNativeRequestBackend = createC2MemFfiNativeRequestBackend(requestPool);
  const responseBackend: C2MemFfiNativeResponseBackend = createC2MemFfiNativeResponseBackend(responsePool);
  const requestBlock: C2MemFfiRequestBlock = await requestBackend.writeRequest(new Uint8Array([abiVersion]));
  await requestBackend.releaseRequest(requestBlock);
  await responseBackend.close();
  await requestBackend.close();
  return {
    ...requestBlock,
    prefix: poolConfig.prefix,
    segments: requestPool.segments,
  };
}

void abiVersion;
void connect;
void smokeTypes;
`);
  writeFileSync(resolve(consumerRoot, 'tsconfig.json'), `
{
  "compilerOptions": {
    "lib": ["ES2022"],
    "module": "NodeNext",
    "moduleResolution": "NodeNext",
    "noEmit": true,
    "exactOptionalPropertyTypes": true,
    "skipLibCheck": false,
    "strict": true,
    "target": "ES2022",
    "types": []
  },
  "include": ["smoke-types.mts"]
}
`);
  run(process.execPath, [
    resolve(consumerRoot, 'node_modules', 'typescript', 'bin', 'tsc'),
    '-p',
    'tsconfig.json',
  ], {
    cwd: consumerRoot,
  });
  run(process.execPath, ['smoke.mjs'], {
    cwd: consumerRoot,
  });
  console.log('npm packed runtime and TypeScript tarballs install, typecheck, and exercise bundled native request/response pool and request/response handle lifecycle paths from a clean consumer project.');
} finally {
  rmSync(tempRoot, { recursive: true, force: true });
}
