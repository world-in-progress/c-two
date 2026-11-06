import assert from 'node:assert/strict';
import { createHash } from 'node:crypto';
import { readFile } from 'node:fs/promises';
import { createInterface } from 'node:readline';
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
const connector = createNodeIpcConnector(config.connectorOptions ?? {});
let endpoint;
let connections = 0;
const transport = generated.createIpcEncodedTransport(config.address, {
  connect: async (...args) => {
    connections += 1;
    endpoint = await connector(...args);
    return endpoint;
  },
  callTimeoutMs: 30_000,
});
let stream;
try {
  await transport.prepare(config.route, generated.CONTRACT);
  const client = new generated.ContractClient(transport, config.route);
  if (config.scenario === 'interleave' || config.scenario === 'unary') {
    await concurrentClient(client);
  } else {
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
      const expectedError = config.host === 'rust'
        ? (config.ending === 'step' ? /stream matrix step failed/ : /stream matrix cleanup failed/)
        : /node-python-stream-error/;
      assert.match(receipt.error.message, expectedError);
      assert.equal(receipt.error.transportPhase, null);
    } else {
      assert.equal(failure, null);
      assert.equal(receipt.error, null);
    }
    const result = {count, completeItems: receipt.completeItems.toString(), held: config.hold, failed: config.fail ?? false};
    if (config.collectStats) {
      // The same-connection round trip follows queued RELEASE records.
      await client.method_0_ping();
      result.stats = connector.stats();
    }
    console.log(JSON.stringify(result));
  }
} finally {
  if (stream !== undefined) await stream.close(0);
  source.dispose();
  await transport.close();
  connector.close();
}

async function concurrentClient(client) {
  const binding = await endpoint.prepare(config.route, generated.CONTRACT);
  const streams = [];
  let secondSource;
  const consume = (item, digest) => {
    assert.equal(createHash('sha256').update(item.binaryBytes()).digest('hex'), digest);
    item.dispose();
  };
  try {
    if (config.scenario === 'interleave') {
      const bytes = new Uint8Array(await readFile(config.secondInputPath));
      secondSource = Payload.openCopy(payloadModule.compileSpec(), bytes);
      const digests = [expected, createHash('sha256').update(bytes).digest('hex')];
      streams.push(client.method_1_echo(source), client.method_1_echo(secondSource));
      const first = await Promise.all(streams.map(stream => stream.next()));
      for (let index = 0; index < first.length; index++) {
        assert.equal(first[index].done, false);
        consume(first[index].value, digests[index]);
      }
      await client.method_0_ping();
      console.log(JSON.stringify({kind: 'unary_interleaved'}));
      await Promise.all(streams.map(async (stream, index) => {
        let count = 1;
        for await (const item of stream) { consume(item, digests[index]); count += 1; }
        assert.equal(count, index + 3);
        assert.deepEqual(await stream.close(null), {completed: true, error: null, completeItems: BigInt(count)});
      }));
    } else {
      let firstSettled = false;
      // Do not await the first call; the test controller confirms its resource barrier before the second.
      const first = client.method_0_ping().then(() => { firstSettled = true; });
      const control = createInterface({input: process.stdin});
      try {
        assert.equal((await control[Symbol.asyncIterator]().next()).value, 'first_unary_blocked');
      } finally { control.close(); }
      const second = client.method_0_ping();
      await second;
      assert.equal(firstSettled, false, 'the second unary completed while the first was blocked');
      console.log(JSON.stringify({kind: 'second_unary_completed'}));
      await first;
    }
    const after = await endpoint.prepare(config.route, generated.CONTRACT);
    assert.equal(after.connectionId, binding.connectionId);
    assert.equal(connections, 1);
    assert.equal(connector.stats().used_operations, '0');
    console.log(JSON.stringify({scenario: config.scenario, connections, connectionId: String(binding.connectionId)}));
  } finally {
    for (const stream of streams) await stream.close(0);
    if (secondSource !== undefined) secondSource.dispose();
  }
}
