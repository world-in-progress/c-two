import assert from 'node:assert/strict';
import { createHash } from 'node:crypto';
import { readFile } from 'node:fs/promises';
import { pathToFileURL } from 'node:url';
import { createNodeIpcConnector } from '@c-two/c2-mem-ffi';
import { Payload, initPayload } from 'fastdb4ts/payload';

const config = JSON.parse(await readFile(process.argv[2], 'utf8'));
await initPayload();
const generated = await import(pathToFileURL(config.contractModule).href);
const payloadModule = await import(pathToFileURL(config.payloadModule).href);
const encodedInput = new Uint8Array(await readFile(config.inputPath));
const source = Payload.openCopy(payloadModule.compileSpec(), encodedInput);
const expected = createHash('sha256').update(encodedInput).digest('hex');
const connector = createNodeIpcConnector();
const transport = generated.createIpcEncodedTransport(config.address, {
  connect: connector,
  callTimeoutMs: 30_000,
});
let stream;
try {
  await transport.prepare(config.route, generated.CONTRACT);
  const client = new generated.ContractClient(transport, config.route);
  await client.method_0_ping();
  stream = config.hold ? client.hold_method_1_echo(source) : client.method_1_echo(source);
  assert.equal(stream[Symbol.asyncIterator](), stream);
  let count = 0;
  let failure = null;
  try {
  for await (const item of stream) {
    const payload = config.hold ? item.value : item;
    const actual = createHash('sha256').update(payload.binaryBytes()).digest('hex');
    assert.equal(actual, expected);
    if (config.hold) {
      item.release();
      assert.throws(() => item.value, /released/);
    } else {
      payload.dispose();
    }
    count += 1;
  }
  }
  catch (error) { failure = error; }
  const receipt = await stream.close(null);
  assert.equal(receipt.completed, true);
  const expectedCount = config.count ?? 2;
  assert.equal(count, expectedCount);
  assert.equal(receipt.completeItems, BigInt(expectedCount));
  if (config.fail) {
    assert.ok(failure instanceof Error);
    assert.ok(receipt.error instanceof Error);
    assert.equal(receipt.error.code, failure.code);
    assert.match(receipt.error.message, /node-python-stream-error/);
    assert.equal(receipt.error.transportPhase, null);
  } else {
    assert.equal(failure, null);
    assert.equal(receipt.error, null);
  }
  console.log(JSON.stringify({count, completeItems: receipt.completeItems.toString(), held: config.hold, failed: config.fail ?? false}));
} finally {
  if (stream !== undefined) await stream.close(0);
  source.dispose();
  await transport.close();
  connector.close();
}
