import assert from 'node:assert/strict';
import { spawnSync } from 'node:child_process';
import { existsSync, readdirSync, writeFileSync } from 'node:fs';
import { delimiter, dirname, join, resolve } from 'node:path';
import { fileURLToPath } from 'node:url';
import { runCargo } from '../../c2-mem-ffi/bindings/typescript/scripts/cargo-tools.mjs';

const testsDir = dirname(fileURLToPath(import.meta.url));
const coreRoot = resolve(testsDir, '../../..');
const repoRoot = resolve(coreRoot, '..');

function findFastDbLibraryLocations(root) {
  if (!existsSync(root)) return undefined;
  if (process.platform === 'win32') {
    let dllDir;
    let libDir;
    const stack = [root];
    while (stack.length > 0 && (dllDir === undefined || libDir === undefined)) {
      const current = stack.pop();
      let entries;
      try {
        entries = readdirSync(current, { withFileTypes: true });
      } catch {
        continue;
      }
      const files = new Set();
      for (const entry of entries) {
        if (entry.isFile()) {
          files.add(entry.name.toLowerCase());
        } else if (entry.isDirectory()) {
          stack.push(join(current, entry.name));
        }
      }
      if (files.has('fastdb.dll') && files.has('fastdb.lib')) {
        return { linkDir: current, runtimeDirs: [current] };
      }
      if (dllDir === undefined && files.has('fastdb.dll')) {
        dllDir = current;
      }
      if (libDir === undefined && files.has('fastdb.lib')) {
        libDir = current;
      }
    }
    if (libDir !== undefined && dllDir !== undefined) {
      return {
        linkDir: libDir,
        runtimeDirs: libDir === dllDir ? [libDir] : [dllDir, libDir],
      };
    }
    return undefined;
  }
  const targetName = process.platform === 'darwin' ? 'libfastdb.dylib' : 'libfastdb.so';
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
      if (entry.isFile() && entry.name === targetName) {
        return { linkDir: current, runtimeDirs: [current] };
      }
      if (entry.isDirectory()) {
        stack.push(join(current, entry.name));
      }
    }
  }
  return undefined;
}

export function prependEnvPaths(env, key, dirs) {
  if (dirs.length === 0) return;
  const existing = env[key] ?? '';
  const parts = existing ? existing.split(delimiter).filter(Boolean) : [];
  const desired = [];
  const seen = new Set();
  for (const dir of dirs) {
    if (!seen.has(dir)) {
      seen.add(dir);
      desired.push(dir);
    }
  }
  const remaining = parts.filter(part => !seen.has(part));
  env[key] = [...desired, ...remaining].join(delimiter);
}

function buildFastDbEnvironment() {
  const env = { ...process.env };
  let runtimeDirs = [];
  if (env.FASTDB_PAYLOAD_SYSTEM_LIB_DIR) {
    const dir = resolve(env.FASTDB_PAYLOAD_SYSTEM_LIB_DIR);
    env.FASTDB_PAYLOAD_SYSTEM_LIB_DIR = dir;
    runtimeDirs = [dir];
  } else if (!env.FASTDB_CORE_SDK_DIR) {
    const discovered =
      findFastDbLibraryLocations(resolve(repoRoot, '.venv')) ??
      findFastDbLibraryLocations(resolve(repoRoot, '..', 'fastdb', 'build'));
    if (discovered !== undefined) {
      env.FASTDB_PAYLOAD_LINK_MODE = 'system';
      env.FASTDB_PAYLOAD_SYSTEM_LIB_DIR = discovered.linkDir;
      runtimeDirs = discovered.runtimeDirs;
    }
  }
  if (process.platform === 'darwin') {
    prependEnvPaths(env, 'DYLD_LIBRARY_PATH', runtimeDirs);
  } else if (process.platform === 'win32') {
    const pathKey = Object.keys(env).find(k => k.toUpperCase() === 'PATH') ?? 'PATH';
    prependEnvPaths(env, pathKey, runtimeDirs);
  } else {
    prependEnvPaths(env, 'LD_LIBRARY_PATH', runtimeDirs);
  }
  return env;
}

function extractBuiltExampleExecutable(cargoStdout) {
  let executablePath;
  for (const rawLine of cargoStdout.split(/\r?\n/)) {
    const line = rawLine.trim();
    if (!line.startsWith('{')) continue;
    let msg;
    try {
      msg = JSON.parse(line);
    } catch {
      continue;
    }
    if (
      msg?.reason === 'compiler-artifact' &&
      msg?.target?.name === 'emit_typescript_transport' &&
      typeof msg.executable === 'string' &&
      msg.executable.length > 0
    ) {
      executablePath = msg.executable;
    }
  }
  assert.ok(executablePath, 'Cargo JSON output did not report executable path for emit_typescript_transport');
  return executablePath;
}

let cachedRenderedTransport;

export function renderTypescriptTransportSource() {
  if (cachedRenderedTransport !== undefined) {
    return cachedRenderedTransport;
  }
  const env = buildFastDbEnvironment();
  const target = resolve(env.CARGO_TARGET_DIR ?? resolve(coreRoot, 'target'));
  const buildResult = runCargo(
    [
      'build',
      '--offline',
      '--locked',
      '--message-format=json',
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
  const exe = extractBuiltExampleExecutable(buildResult.stdout);
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
