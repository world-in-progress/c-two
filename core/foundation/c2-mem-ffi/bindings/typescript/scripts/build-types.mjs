import { dirname, resolve } from 'node:path';
import { fileURLToPath } from 'node:url';

import { runTsc } from './tsc-tools.mjs';

const scriptDir = dirname(fileURLToPath(import.meta.url));
const packageRoot = resolve(scriptDir, '..');
const args = ['-p', 'tsconfig.json'];

if (process.argv.includes('--no-emit')) {
  args.push('--noEmit');
}

try {
  runTsc(args, { cwd: packageRoot, packageRoot });
} catch (error) {
  console.error(error.message);
  process.exit(1);
}
