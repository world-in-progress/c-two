import assert from 'node:assert/strict';
import { readFile } from 'node:fs/promises';
import { createInterface } from 'node:readline';
import { pathToFileURL } from 'node:url';
import { createBundledC2MemFfiNodeRuntime } from '@c-two/c2-mem-ffi';

// This process keeps generated clients and their shared transports alive while
// the Python harness publishes/removes/replaces routes and restarts the owner.
const config = JSON.parse(await readFile(process.argv[2], 'utf8'));
const manager = await import(pathToFileURL(config.managerModule).href);
const builder = await import(pathToFileURL(config.builderModule).href);
const runtime = createBundledC2MemFfiNodeRuntime();
const transports = new Map();

function stateFor(id) {
  let state = transports.get(id);
  if (state !== undefined) return state;
  state = { connects: 0, businessWrites: 0, observations: [], loseReply: false };
  const transport = manager.createIpcEncodedTransport(config.address, {
    async connect(address) {
      const connection = await runtime.connect(address);
      state.connects += 1;
      let loseBusinessReply = false;
      return {
        async write(bytes) {
          const flags = new DataView(bytes.buffer, bytes.byteOffset, bytes.byteLength).getUint32(12, true);
          if ((flags & (1 << 7)) !== 0) {
            state.businessWrites += 1;
            loseBusinessReply = state.loseReply;
            state.loseReply = false;
          }
          await connection.write(bytes);
        },
        async readExactly(size) {
          const bytes = await connection.readExactly(size);
          // Read the real business response header first: the callback has
          // executed, but this client observes a lost reply and must not replay.
          if (loseBusinessReply) {
            loseBusinessReply = false;
            throw new Error('injected loss after actual business reply header');
          }
          return bytes;
        },
        async close() { await connection.close(); },
      };
    },
    observe(observation) { state.observations.push(observation); },
  });
  state.transport = transport;
  state.clients = {
    manager: new manager.ContractClient(transport, 'manager'),
    builder: new builder.ContractClient(transport, 'builder'),
  };
  transports.set(id, state);
  return state;
}

const reply = (value) => process.stdout.write(`${JSON.stringify(value)}\n`);
reply({ready: true});
try {
  for await (const line of createInterface({input: process.stdin, crlfDelay: Infinity})) {
    const command = JSON.parse(line);
    if (command.op === 'exit') {
      reply({ok: true});
      break;
    }
    const state = stateFor(command.transport ?? 'original');
    let error;
    try {
      if (command.op === 'close') {
        await state.transport.close();
        await state.transport.close();
      } else if (command.op === 'prepare') {
        const contract = command.route === 'manager' ? manager.CONTRACT : builder.CONTRACT;
        await state.transport.prepare(command.route, contract);
      } else {
        assert.equal(command.op, 'call');
        state.loseReply = command.uncertain === true;
        assert.equal(await state.clients[command.route].method_0_ping(), undefined);
      }
    } catch (caught) {
      error = {name: caught.name, message: caught.message};
    }
    reply({
      ok: error === undefined, error,
      connects: state.connects, businessWrites: state.businessWrites,
      observations: state.observations,
    });
  }
} finally {
  for (const state of transports.values()) {
    await state.transport.close();
    await state.transport.close();
  }
  await runtime.close();
}
