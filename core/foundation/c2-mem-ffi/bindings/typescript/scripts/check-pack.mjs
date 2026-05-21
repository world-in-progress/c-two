import { spawnSync } from 'node:child_process';

function run(command, args, options = {}) {
  const result = spawnSync(command, args, {
    encoding: 'utf8',
    ...options,
  });
  if (result.error) {
    console.error(result.error.message);
    process.exit(1);
  }
  if (result.status !== 0) {
    process.stdout.write(result.stdout ?? '');
    process.stderr.write(result.stderr ?? '');
    process.exit(result.status ?? 1);
  }
  return result;
}

function runNpm(args, options = {}) {
  if (process.env.npm_execpath) {
    return run(process.execPath, [process.env.npm_execpath, ...args], options);
  }
  return run('npm', args, options);
}

const result = runNpm(['pack', '--dry-run', '--json', '--ignore-scripts']);

let entries;
try {
  entries = JSON.parse(result.stdout);
} catch (error) {
  console.error(`Failed to parse npm pack JSON: ${String(error)}`);
  process.stdout.write(result.stdout);
  process.exit(1);
}

const entry = entries?.[0];
if (typeof entry !== 'object' || entry === null || !Array.isArray(entry.files)) {
  console.error('npm pack JSON did not contain a file list.');
  process.exit(1);
}

const files = new Set(entry.files.map((file) => file.path));
const required = [
  'README.md',
  'package.json',
  'dist/index.js',
  'dist/index.d.ts',
  'dist/native/c2_mem_ffi_node.node',
  process.platform === 'darwin'
    ? 'dist/native/libc2_mem_ffi.dylib'
    : 'dist/native/libc2_mem_ffi.so',
];

const missing = required.filter((path) => !files.has(path));
if (missing.length > 0) {
  console.error(`npm pack is missing required runtime files: ${missing.join(', ')}`);
  process.exit(1);
}

const requiredSet = new Set(required);
const unexpected = [...files].filter((path) => !requiredSet.has(path));
if (unexpected.length > 0) {
  console.error(`npm pack includes unexpected runtime files: ${unexpected.join(', ')}`);
  process.exit(1);
}

console.log(`npm pack dry-run contains ${files.size} runtime files.`);
