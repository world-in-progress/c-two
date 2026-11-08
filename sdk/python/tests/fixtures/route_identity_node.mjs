// Drive the generated transport against real c3 relays from a Python test.
import {readFileSync} from 'node:fs';
import {pathToFileURL} from 'node:url';
import {createInterface} from 'node:readline';

const transport = await import(pathToFileURL(process.argv[2]));
const config = JSON.parse(readFileSync(process.argv[3], 'utf8'));
const oldA = transport.createRelayAwareHttpEncodedTransport(config.relayA);
const oldB = transport.createRelayAwareHttpEncodedTransport(config.relayB);
let freshA;
let freshB;

async function call(client, base64) {
  const response = await client.call(
    config.name, config.contract, 'call',
    () => new Uint8Array(Buffer.from(base64, 'base64')),
  );
  return Buffer.from(response).toString('base64');
}

function failure(error) {
  return {
    type: error.constructor.name,
    code: error.code ?? null,
    canonicalName: error.canonicalName ?? null,
    details: error.details ?? {},
    body: error.body ?? null,
    transportPhase: error.transportPhase ?? null,
    retrySafe: error.retrySafe ?? null,
    routeConflictInstance: error instanceof transport.C2RouteConflictError,
    coreInstance: error instanceof transport.C2CoreError,
  };
}

async function attempt(client, base64) {
  try { return {value: await call(client, base64)}; }
  catch (error) { return {error: failure(error)}; }
}

for await (const line of createInterface({input: process.stdin, crlfDelay: Infinity})) {
  const command = JSON.parse(line);
  let result;
  if (command.step === 'before') {
    result = {
      a: await attempt(oldA, command.a),
      b: await attempt(oldB, command.b),
    };
  } else if (command.step === 'conflict') {
    freshA = transport.createRelayAwareHttpEncodedTransport(config.relayA);
    freshB = transport.createRelayAwareHttpEncodedTransport(config.relayB);
    result = {
      newA: await attempt(freshA, command.a),
      newB: await attempt(freshB, command.b),
      oldA: await attempt(oldA, command.a),
      oldB: await attempt(oldB, command.b),
    };
  } else if (command.step === 'follow') {
    result = {
      refused: await attempt(oldA, command.a),
      followed: await attempt(oldA, command.b),
      oldB: await attempt(oldB, command.b),
    };
  } else if (command.step === 'close') {
    await oldA.close();
    await oldB.close();
    await freshA?.close();
    await freshB?.close();
    result = {closed: true};
  } else {
    throw new Error(`unknown step ${command.step}`);
  }
  process.stdout.write(JSON.stringify(result) + '\n');
  if (command.step === 'close') {
    process.stdout.write('', () => process.exit(0));
    break;
  }
}
