import { spawnSync } from 'node:child_process';
import { existsSync } from 'node:fs';
import { dirname, resolve } from 'node:path';

export function runCommand(command, args, options = {}) {
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

export function runTsc(args, options = {}) {
  const { packageRoot = process.cwd(), ...spawnOptions } = options;
  if (process.env.TSC_JS) {
    return runCommand(process.execPath, [process.env.TSC_JS, ...args], spawnOptions);
  }
  if (process.env.TSC) {
    return runCommand(process.env.TSC, args, spawnOptions);
  }
  for (const tscJs of candidateTscScripts(packageRoot)) {
    if (existsSync(tscJs)) {
      return runCommand(process.execPath, [tscJs, ...args], spawnOptions);
    }
  }
  return runCommand('tsc', args, spawnOptions);
}

function candidateTscScripts(packageRoot) {
  const candidates = [
    resolve(packageRoot, 'node_modules', 'typescript', 'bin', 'tsc'),
  ];
  if (process.env.npm_config_global_prefix) {
    candidates.push(resolve(process.env.npm_config_global_prefix, 'lib', 'node_modules', 'typescript', 'bin', 'tsc'));
  }
  if (process.env.npm_execpath) {
    candidates.push(resolve(dirname(process.env.npm_execpath), '..', '..', 'typescript', 'bin', 'tsc'));
  }
  return candidates;
}
