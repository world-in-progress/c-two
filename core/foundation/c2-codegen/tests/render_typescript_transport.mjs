import assert from 'node:assert/strict';
import { readFileSync, writeFileSync } from 'node:fs';
import { join } from 'node:path';

const PLACEHOLDER = '/* __C2_GENERATED_ERROR_CODE_NAMES__ */ Object.freeze({})';

export function renderTypescriptTransportSource() {
  const template = readFileSync(new URL('../assets/typescript_transport.ts', import.meta.url), 'utf8');
  const rustRegistry = readFileSync(new URL('../../c2-error/src/lib.rs', import.meta.url), 'utf8');
  const macroMatch = /^define_error_codes!\s*\{([\s\S]*?)\n\}/m.exec(rustRegistry);
  assert.ok(macroMatch, 'define_error_codes! block missing in c2-error/src/lib.rs');
  const entries = [];
  const linePattern = /^\s*([A-Za-z0-9_]+)\s*=\s*([0-9]+)\b/gm;
  let match;
  while ((match = linePattern.exec(macroMatch[1])) !== null) {
    entries.push(`  ${Number(match[2])}: ${JSON.stringify(match[1])},`);
  }
  assert.equal(entries.length, 25, `expected 25 canonical error codes, got ${entries.length}`);
  assert.ok(template.includes(PLACEHOLDER), 'typescript_transport.ts missing error-code placeholder');
  const table = `Object.freeze({\n${entries.join('\n')}\n})`;
  const rendered = template.replace(PLACEHOLDER, table);
  assert.equal(rendered.includes('__C2_GENERATED_ERROR_CODE_NAMES__'), false);
  return rendered;
}

export function writeRenderedTypescriptTransport(directory) {
  const destination = join(directory, 'typescript_transport.ts');
  writeFileSync(destination, renderTypescriptTransportSource(), 'utf8');
  return destination;
}
