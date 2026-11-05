import assert from 'node:assert/strict';
import { readFile } from 'node:fs/promises';
import { createInterface } from 'node:readline';
import { pathToFileURL } from 'node:url';
import { createNodeIpcConnector } from '@c-two/c2-mem-ffi';

// Keep generated clients and native endpoint contexts alive as routes change.
const config = JSON.parse(await readFile(process.argv[2], 'utf8'));
const manager = await import(pathToFileURL(config.managerModule).href);
const builder = await import(pathToFileURL(config.builderModule).href);
const connector = createNodeIpcConnector(config.ipcRoot ? {ipcRoot: config.ipcRoot} : {});
const transports = new Map();

function stateFor(id, command) {
  let state = transports.get(id);
  if (state !== undefined) return state;
  state = { connects: 0, observations: [] };
  const transport = manager.createIpcEncodedTransport(config.address, {
    connectTimeoutMs: command.connectTimeoutMs,
    async connect(address, timeoutMs, nativeOptions) {
      state.connects += 1;
      return connector(address, timeoutMs, nativeOptions);
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

const reply = value => process.stdout.write(`${JSON.stringify(value)}\n`);
reply({ready: true});
try {
  for await (const line of createInterface({input: process.stdin, crlfDelay: Infinity})) {
    const command = JSON.parse(line);
    if (command.op === 'exit') { reply({ok: true}); break; }
    const state = stateFor(command.transport ?? 'original', command);
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
        assert.equal(await state.clients[command.route].method_0_ping(), undefined);
      }
    } catch (caught) {
      error = {name: caught.name, message: caught.message, code: caught.code, nativeDetails: caught.nativeDetails};
    }
    reply({ok: error === undefined, error, connects: state.connects, observations: state.observations});
  }
} finally {
  for (const state of transports.values()) await state.transport.close();
  connector.close();
}
