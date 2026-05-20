import { dirname, resolve } from 'node:path';
import { fileURLToPath } from 'node:url';

import { runCommand } from './tsc-tools.mjs';

const scriptDir = dirname(fileURLToPath(import.meta.url));
const packageRoot = resolve(scriptDir, '..');

try {
  runCommand(process.execPath, [resolve(scriptDir, 'build-runtime.mjs')], { cwd: packageRoot });
  runCommand(process.execPath, ['--test', resolve(packageRoot, 'tests', 'c2-mem-ffi-node-loader.test.mjs')], {
    cwd: packageRoot,
    stdio: 'inherit',
  });
} catch (error) {
  console.error(error.message);
  process.exit(1);
}
