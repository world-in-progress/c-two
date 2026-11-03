import { runNpm } from './npm-tools.mjs';
import { runCargo } from './cargo-tools.mjs';
import { createRequire } from 'node:module';
import { spawnSync } from 'node:child_process';
import { copyFileSync, existsSync, mkdirSync, mkdtempSync, readFileSync, rmSync, writeFileSync } from 'node:fs';
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
  // The consumer installs only published tarballs. Build the test-owned native
  // peer in the source checkout; no Rust build or sibling link enters the package.
  const peerRoot = resolve(packageRoot, 'tests/protocol-server');
  const target = process.env.CARGO_TARGET_DIR ?? resolve(packageRoot, '../../../../target');
  try {
    copyFileSync(resolve(packageRoot, '../../../../Cargo.lock'), resolve(peerRoot, 'Cargo.lock'));
    runCargo(['build', '--offline', '--manifest-path', resolve(peerRoot, 'Cargo.toml'), '--target-dir', target]);
  } finally { rmSync(resolve(peerRoot, 'Cargo.lock'), { force: true }); }
  const peer = resolve(target, 'debug', `c2-node-protocol-server${process.platform === 'win32' ? '.exe' : ''}`);
  const require = createRequire(resolve(consumerRoot, 'package.json'));
  const ts = require('typescript');
  const source = readFileSync(resolve(packageRoot, '../../../c2-codegen/assets/typescript_transport.ts'), 'utf8');
  writeFileSync(resolve(consumerRoot, 'transport.ts'), source);
  writeFileSync(resolve(consumerRoot, 'transport.mjs'), ts.transpileModule(source, {
    compilerOptions: { target: ts.ScriptTarget.ES2022, module: ts.ModuleKind.ES2022 },
  }).outputText);
  writeFileSync(resolve(consumerRoot, 'smoke.mjs'), `
import assert from 'node:assert/strict';
import { spawn } from 'node:child_process';
import { mkdirSync } from 'node:fs';
import { resolve } from 'node:path';
import { createInterface } from 'node:readline';
import {
  C2_MEM_FFI_ABI_VERSION, createNodeIpcConnect, loadBundledC2MemFfiNodeNativeSymbols,
} from '@c-two/c2-mem-ffi';
import { createIpcEncodedTransport } from './transport.mjs';
const { symbols } = loadBundledC2MemFfiNodeNativeSymbols();
assert.equal(C2_MEM_FFI_ABI_VERSION, 7);
assert.equal(symbols.c2_mem_ffi_abi_version(), 7);
const address = 'ipc://packed-node-' + process.pid;
const root = resolve('ipc'); mkdirSync(root, {mode: 0o700});
const child = spawn(process.argv[2], [process.platform === 'win32' ? '-' : root, address], {stdio: ['pipe', 'pipe', 'pipe']});
let stderr = ''; child.stderr.on('data', b => {stderr += b;});
const closed = new Promise(resolve => child.once('exit', code => resolve(code)));
const lines = createInterface({input: child.stdout})[Symbol.asyncIterator]();
const connect = createNodeIpcConnect(process.platform === 'win32' ? {} : {ipcRoot: root});
let connection;
const transport = createIpcEncodedTransport(address, {connect: async address => (connection = await connect(address))});
try {
  assert.equal(JSON.parse((await lines.next()).value).kind, 'ready');
  const contract = {schema: 'c-two.contract.v2', namespace: 'test.p8a', name: 'Echo', version: '0.1.0', descriptorSha256: 'd'.repeat(64), abiHash: 'a'.repeat(64), signatureHash: 'b'.repeat(64)};
  for (const payload of [new Uint8Array([1, 3, 5, 7]), new Uint8Array(8192).fill(9)]) {
    assert.deepEqual(await transport.call('echo', contract, 'echo', payload), payload);
  }
  const memory = connection.memory;
  const request = memory.prepareRequest(new Uint8Array([2, 4, 6, 8]));
  assert.equal(typeof request.block.loanId, 'bigint'); assert.ok(request.block.loanId > 0n);
  memory.discardRequest(request);
  await transport.close();
} finally {
  connection?.close(); connect.close(); child.stdin.end('quit\\n');
  const timer = setTimeout(() => child.kill(), 12000);
  const code = await closed; clearTimeout(timer); assert.equal(code, 0, stderr);
}
`);
  writeFileSync(resolve(consumerRoot, 'smoke-types.mts'), `
import {
  C2_MEM_FFI_ABI_VERSION,
  type C2MemFfiMemoryContext, type C2MemFfiPreparedRequest, type C2MemFfiBlock,
  type C2MemFfiNodeNativeSymbols, createNodeIpcConnect, loadBundledC2MemFfiNodeNativeSymbols,
} from '@c-two/c2-mem-ffi';
import type { C2IpcConnect } from './transport.js';
const abiVersion: 7 = C2_MEM_FFI_ABI_VERSION;
const symbols: C2MemFfiNodeNativeSymbols = loadBundledC2MemFfiNodeNativeSymbols();
const connect = createNodeIpcConnect();
const genericConnect: C2IpcConnect = connect;
void genericConnect;
async function smokeTypes(): Promise<C2MemFfiBlock | undefined> {
  const connection = await connect('ipc://typecheck');
  const memory: C2MemFfiMemoryContext | undefined = connection.memory;
  if (memory === undefined) return undefined;
  const request: C2MemFfiPreparedRequest = memory.prepareRequest(new Uint8Array([abiVersion]));
  const block: C2MemFfiBlock = request.block;
  const id: bigint = block.loanId;
  memory.discardRequest(request);
  await connection.close();
  void id;
  return block;
}
void symbols; void smokeTypes;
`);
  writeFileSync(resolve(consumerRoot, 'tsconfig.json'), `
{
  "compilerOptions": {
    "lib": ["ES2022", "DOM"],
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
  run(process.execPath, ['smoke.mjs', peer], {
    cwd: consumerRoot,
  });
  console.log('npm packed ABI 7 runtime and TypeScript tarballs install, typecheck, and exercise verified IPC plus opaque memory owners from a clean consumer project.');
} finally {
  rmSync(tempRoot, { recursive: true, force: true });
}
