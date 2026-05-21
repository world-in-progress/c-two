import { dirname, resolve } from 'node:path';
import { fileURLToPath } from 'node:url';

import { runCommand, runTsc } from './tsc-tools.mjs';

const scriptDir = dirname(fileURLToPath(import.meta.url));
const packageRoot = resolve(scriptDir, '..');

runTsc(['-p', 'tsconfig.json'], { cwd: packageRoot, packageRoot });
runCommand(process.execPath, [resolve(scriptDir, 'build-node-addon.mjs')], { cwd: packageRoot });
