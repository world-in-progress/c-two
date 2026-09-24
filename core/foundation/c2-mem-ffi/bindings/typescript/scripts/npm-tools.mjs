import { existsSync } from 'node:fs';
import { dirname, resolve } from 'node:path';

import { runCommand } from './tsc-tools.mjs';

export function runNpm(args, options = {}) {
  const candidates = [
    process.env.npm_execpath,
    resolve(dirname(process.execPath), 'node_modules', 'npm', 'bin', 'npm-cli.js'),
    resolve(dirname(process.execPath), '..', 'lib', 'node_modules', 'npm', 'bin', 'npm-cli.js'),
  ];
  for (const script of candidates) {
    if (script !== undefined && existsSync(script)) {
      return runCommand(process.execPath, [script, ...args], options);
    }
  }
  if (process.platform === 'win32') {
    throw new Error('npm-cli.js is required from the Node.js installation.');
  }
  return runCommand('npm', args, options);
}
