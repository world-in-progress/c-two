import assert from 'node:assert/strict';
import { readFile } from 'node:fs/promises';
import { createInterface } from 'node:readline';
import { pathToFileURL } from 'node:url';
import { captureLocalIpcEndpointContext, createNodeIpcConnector } from '@c-two/c2-mem-ffi';

const config = JSON.parse(await readFile(process.argv[2], 'utf8'));
const manager = await import(pathToFileURL(config.managerModule).href);
const builder = await import(pathToFileURL(config.builderModule).href);
const endpointContext = captureLocalIpcEndpointContext(config.ipcRoot === null ? {} : {ipcRoot: config.ipcRoot});
const connector = createNodeIpcConnector({endpointContext});
const preparations = [];
let deactivateNextCall = false;
const observations = [];
const http = [];
const transport = manager.createRelayAwareHttpEncodedTransport(config.address, {
  headers: {'x-c2-local-namespace': endpointContext.namespaceId()},
  ipc: {
    async connect(address, timeout, options) {
      const endpoint = await connector(address, timeout, options);
      return {
        async prepare(route, contract, token) {
          const binding = await endpoint.prepare(route, contract, token);
          preparations.push({route, ...binding, connectionId: String(binding.connectionId)});
          return binding;
        },
        beginCall: (...args) => {
          if (deactivateNextCall) {
            deactivateNextCall = false;
            endpoint.deactivate();
          }
          return endpoint.beginCall(...args);
        },
        deactivate: () => endpoint.deactivate(),
        close: () => endpoint.close(),
      };
    },
  },
  async fetch(url, init) {
    http.push({url, method: init.method});
    return fetch(url, init);
  },
  observe: observation => observations.push(observation),
});
const clients = {
  manager: new manager.ContractClient(transport, 'manager'),
  builder: new builder.ContractClient(transport, 'builder'),
};
const reply = value => process.stdout.write(`${JSON.stringify(value)}\n`);
reply({ready: true});
try {
  for await (const line of createInterface({input: process.stdin, crlfDelay: Infinity})) {
    const command = JSON.parse(line);
    if (command.op === 'exit') {
      await transport.close();
      const stats = connector.stats();
      assert.equal(stats.used_operations, '0');
      assert.equal(stats.payload_buffer?.used_bytes ?? '0', '0');
      connector.close();
      reply({ok: true, stats});
      break;
    }
    if (command.op === 'deactivate-next-call') {
      deactivateNextCall = true;
      reply({ok: true});
      continue;
    }
    let error;
    try {
      assert.equal(command.op, 'call');
      assert.equal(await clients[command.route].method_0_ping(), undefined);
    } catch (caught) {
      error = {
        name: caught.name, message: caught.message, code: caught.code,
        transportPhase: caught.transportPhase, retrySafe: caught.retrySafe,
        nativeDetails: caught.nativeDetails,
      };
    }
    reply({ok: error === undefined, error, preparations, observations, http});
  }
} finally {
  await transport.close();
  connector.close();
  endpointContext.close();
}
