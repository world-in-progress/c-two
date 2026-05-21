import { spawnSync } from 'node:child_process';
import { copyFileSync, mkdirSync } from 'node:fs';
import { dirname, resolve } from 'node:path';
import { fileURLToPath } from 'node:url';

import { runCargo } from './cargo-tools.mjs';

const scriptDir = dirname(fileURLToPath(import.meta.url));
const packageRoot = resolve(scriptDir, '..');
const crateRoot = resolve(packageRoot, '..', '..');
const source = resolve(packageRoot, 'native', 'node_c2_mem_ffi_loader.c');
const distNative = resolve(packageRoot, 'dist', 'native');
const output = resolve(distNative, 'c2_mem_ffi_node.node');
const nodeInclude = resolve(process.execPath, '..', '..', 'include', 'node');
const c2MemFfiInclude = resolve(crateRoot, 'include');

if (process.platform === 'win32') {
  console.log('Skipping c2-mem-ffi Node addon build on Windows; POSIX SHM support is required.');
  process.exit(0);
}

mkdirSync(distNative, { recursive: true });

let metadata;
try {
  runCargo([
    'build',
    '--manifest-path',
    resolve(crateRoot, 'Cargo.toml'),
  ], {
    stdio: 'inherit',
  });
  metadata = runCargo([
    'metadata',
    '--manifest-path',
    resolve(crateRoot, 'Cargo.toml'),
    '--format-version=1',
    '--no-deps',
  ]);
} catch (error) {
  console.error(error.message);
  process.exit(1);
}

const targetDirectory = JSON.parse(metadata.stdout).target_directory;
const libraryName = process.platform === 'darwin'
  ? 'libc2_mem_ffi.dylib'
  : 'libc2_mem_ffi.so';
copyFileSync(resolve(targetDirectory, 'debug', libraryName), resolve(distNative, libraryName));

const cc = process.env.CC || 'cc';
const args = [
  '-std=c11',
  '-O2',
  '-fPIC',
  '-shared',
  `-I${nodeInclude}`,
  `-I${c2MemFfiInclude}`,
  '-DNAPI_VERSION=10',
  source,
  '-o',
  output,
];

if (process.platform === 'darwin') {
  args.splice(4, 0, '-undefined', 'dynamic_lookup');
} else {
  args.push('-ldl');
}

const result = spawnSync(cc, args, {
  cwd: packageRoot,
  stdio: 'inherit',
});

if (result.error) {
  console.error(result.error.message);
  process.exit(1);
}
process.exit(result.status ?? 1);
