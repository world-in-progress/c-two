import { spawnSync } from 'node:child_process';
import { existsSync } from 'node:fs';
import { dirname, resolve } from 'node:path';
import { fileURLToPath } from 'node:url';

const coreRoot = resolve(dirname(fileURLToPath(import.meta.url)), '../../../../..');

export function resolveCargo() {
  if (process.env.CARGO) {
    return process.env.CARGO;
  }
  const candidates = [];
  const executable = process.platform === 'win32' ? 'cargo.exe' : 'cargo';
  if (process.env.CARGO_HOME) {
    candidates.push(resolve(process.env.CARGO_HOME, 'bin', executable));
  }
  const userDirectory = process.env.USERPROFILE ?? process.env.HOME;
  if (userDirectory) {
    candidates.push(resolve(userDirectory, '.cargo', 'bin', executable));
  }
  candidates.push('/opt/homebrew/bin/cargo', '/usr/local/bin/cargo');
  for (const candidate of candidates) {
    if (existsSync(candidate)) {
      return candidate;
    }
  }
  return 'cargo';
}

export function runCargo(args, options = {}) {
  const cargo = resolveCargo();
  const result = spawnSync(cargo, args, {
    encoding: 'utf8',
    ...options,
  });
  if (result.error) {
    throw result.error;
  }
  if (result.status !== 0) {
    process.stdout.write(result.stdout ?? '');
    process.stderr.write(result.stderr ?? '');
    throw new Error(`${cargo} ${args.join(' ')} failed with exit code ${result.status ?? 1}`);
  }
  return result;
}

// The tests' native IPC peer is a Core workspace member, so it builds from
// core/Cargo.lock as committed. Concurrent test files only wait on Cargo's
// build-directory lock; none of them writes a shared file.
export function buildProtocolPeer(options = {}) {
  runCargo(['build', '--offline', '--locked', '--manifest-path', resolve(coreRoot, 'Cargo.toml'),
    '-p', 'c2-node-protocol-server'], options);
  const target = process.env.CARGO_TARGET_DIR ?? resolve(coreRoot, 'target');
  return resolve(target, 'debug', `c2-node-protocol-server${process.platform === 'win32' ? '.exe' : ''}`);
}
