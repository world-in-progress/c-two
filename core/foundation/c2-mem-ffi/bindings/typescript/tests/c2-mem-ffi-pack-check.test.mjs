import assert from 'node:assert/strict';
import { spawnSync } from 'node:child_process';
import { mkdirSync, rmSync, writeFileSync } from 'node:fs';
import { dirname, resolve } from 'node:path';
import { fileURLToPath } from 'node:url';
import test from 'node:test';

const testDir = dirname(fileURLToPath(import.meta.url));
const packageRoot = resolve(testDir, '..');
const checkPackScript = resolve(packageRoot, 'scripts', 'check-pack.mjs');

test('c2-mem-ffi pack checker rejects unexpected runtime files', () => {
  const extraFile = resolve(packageRoot, 'dist', 'native', 'unexpected-debug.map');
  mkdirSync(dirname(extraFile), { recursive: true });
  writeFileSync(extraFile, 'debug artifact');

  try {
    const result = spawnSync(process.execPath, [checkPackScript], {
      cwd: packageRoot,
      encoding: 'utf8',
    });

    assert.notEqual(result.status, 0);
    assert.match(result.stderr, /unexpected runtime files/);
    assert.match(result.stderr, /dist\/native\/unexpected-debug\.map/);
  } finally {
    rmSync(extraFile, { force: true });
  }
});
