import { dirname, resolve } from 'node:path';
import { fileURLToPath } from 'node:url';

import { runCargo } from './cargo-tools.mjs';

const scriptDir = dirname(fileURLToPath(import.meta.url));
const packageRoot = resolve(scriptDir, '..');
const crateRoot = resolve(packageRoot, '..', '..');

try {
  runCargo([
    'build',
    '--manifest-path',
    resolve(crateRoot, 'Cargo.toml'),
  ], {
    cwd: packageRoot,
    stdio: 'inherit',
  });
} catch (error) {
  console.error(error.message);
  process.exit(1);
}
