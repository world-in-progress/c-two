import assert from 'node:assert/strict';
import { spawnSync } from 'node:child_process';
import { existsSync, readdirSync, writeFileSync } from 'node:fs';
import { delimiter, dirname, join, resolve } from 'node:path';
import { fileURLToPath } from 'node:url';
import { runCargo } from '../../c2-mem-ffi/bindings/typescript/scripts/cargo-tools.mjs';

const testsDir = dirname(fileURLToPath(import.meta.url));
const coreRoot = resolve(testsDir, '../../..');
const repoRoot = resolve(coreRoot, '..');

function findFastDbLibraryDir(root) {
  if (!existsSync(root)) return undefined;
  const targets = process.platform === 'win32'
    ? new Set(['fastdb.dll'])
    : process.platform === 'darwin'
      ? new Set(['libfastdb.dylib'])
      : new Set(['libfastdb.so']);
  const stack = [root];
  while (stack.length > 0) {
    const current = stack.pop();
    let entries;
    try {
      entries = readdirSync(current, { withFileTypes: true });
    } catch {
      continue;
    }
    for (const entry of entries) {
      if (entry.isFile() && targets.has(entry.name)) {
        return current;
      }
      if (entry.isDirectory()) {
        stack.push(join(current, entry.name));
      }
    }
  }
  return undefined;
}

function buildFastDbEnvironment() {
  const env = { ...process.env };
  let libDir = env.FASTDB_PAYLOAD_SYSTEM_LIB_DIR;
  if (!env.FASTDB_PAYLOAD_LINK_MODE && !env.FASTDB_CORE_SDK_DIR && !libDir) {
    libDir =
      findFastDbLibraryDir(resolve(repoRoot, '.venv')) ??
      findFastDbLibraryDir(resolve(repoRoot, '..', 'fastdb', 'build'));
    if (libDir !== undefined) {
      env.FASTDB_PAYLOAD_LINK_MODE = 'system';
      env.FASTDB_PAYLOAD_SYSTEM_LIB_DIR = libDir;
    }
  }
  if (libDir !== undefined) {
    if (process.platform === 'darwin') {
      env.DYLD_LIBRARY_PATH = env.DYLD_LIBRARY_PATH ? `${libDir}${delimiter}${env.DYLD_LIBRARY_PATH}` : libDir;
    } else if (process.platform === 'win32') {
      env.PATH = env.PATH ? `${libDir}${delimiter}${env.PATH}` : libDir;
    } else {
      env.LD_LIBRARY_PATH = env.LD_LIBRARY_PATH ? `${libDir}${delimiter}${env.LD_LIBRARY_PATH}` : libDir;
    }
  }
  return env;
}

let cachedRenderedTransport;

export function renderTypescriptTransportSource() {
  if (cachedRenderedTransport !== undefined) {
    return cachedRenderedTransport;
  }
  const env = buildFastDbEnvironment();
  const target = resolve(env.CARGO_TARGET_DIR ?? resolve(coreRoot, 'target'));
  runCargo(
    [
      'build',
      '--offline',
      '--locked',
      '--manifest-path',
      resolve(coreRoot, 'Cargo.toml'),
      '--target-dir',
      target,
      '-p',
      'c2-codegen',
      '--example',
      'emit_typescript_transport',
    ],
    { env },
  );
  const exe = resolve(
    target,
    'debug',
    'examples',
    `emit_typescript_transport${process.platform === 'win32' ? '.exe' : ''}`,
  );
  const emitted = spawnSync(exe, [], { encoding: 'utf8', env });
  assert.equal(emitted.status, 0, emitted.stderr ?? '');
  assert.equal(emitted.stdout.includes('__C2_GENERATED_ERROR_CODE_NAMES__'), false);
  assert.equal(emitted.stdout.includes('719: "RouteConflict"'), true);
  cachedRenderedTransport = emitted.stdout;
  return cachedRenderedTransport;
}

export function writeRenderedTypescriptTransport(directory) {
  const destination = join(directory, 'typescript_transport.ts');
  writeFileSync(destination, renderTypescriptTransportSource(), 'utf8');
  return destination;
}
