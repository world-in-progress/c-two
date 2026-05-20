import { spawnSync } from 'node:child_process';
import { existsSync } from 'node:fs';
import { resolve } from 'node:path';

export function resolveCargo() {
  if (process.env.CARGO) {
    return process.env.CARGO;
  }
  const candidates = [];
  if (process.env.CARGO_HOME) {
    candidates.push(resolve(process.env.CARGO_HOME, 'bin', 'cargo'));
  }
  if (process.env.HOME) {
    candidates.push(resolve(process.env.HOME, '.cargo', 'bin', 'cargo'));
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
