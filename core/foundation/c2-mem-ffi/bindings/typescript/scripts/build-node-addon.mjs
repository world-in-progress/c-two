import { spawnSync } from 'node:child_process';
import { copyFileSync, mkdirSync } from 'node:fs';
import { dirname, resolve } from 'node:path';
import { fileURLToPath } from 'node:url';
import { createRequire } from 'node:module';

import { runCargo } from './cargo-tools.mjs';
import { nativeLibraryName } from './platform.mjs';

const scriptDir = dirname(fileURLToPath(import.meta.url));
const packageRoot = resolve(scriptDir, '..');
const crateRoot = resolve(packageRoot, '..', '..');
const source = resolve(packageRoot, 'native', 'node_c2_mem_ffi_loader.c');
const distNative = resolve(packageRoot, 'dist', 'native');
const output = resolve(distNative, 'c2_mem_ffi_node.node');
const nodeInclude = resolve(process.execPath, '..', '..', 'include', 'node');
const c2MemFfiInclude = resolve(crateRoot, 'include');

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
const libraryName = nativeLibraryName();
copyFileSync(resolve(targetDirectory, 'debug', libraryName), resolve(distNative, libraryName));

if (process.platform === 'win32') {
  // node-gyp resolves the matching Node headers/import library and MSVC tools.
  const require = createRequire(import.meta.url);
  const nodeGyp = require.resolve('node-gyp/bin/node-gyp.js');
  const result = spawnSync(process.execPath, [nodeGyp, 'rebuild', '--release'], {
    cwd: packageRoot,
    stdio: 'inherit',
  });
  if (result.error || result.status !== 0) {
    console.error(result.error?.message ?? `node-gyp failed with exit ${result.status}`);
    process.exit(result.status ?? 1);
  }
  copyFileSync(resolve(packageRoot, 'build', 'Release', 'c2_mem_ffi_node.node'), output);
  process.exit(0);
}

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
