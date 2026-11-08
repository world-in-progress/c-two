import assert from 'node:assert/strict';
import { readFile } from 'node:fs/promises';
import { createInterface } from 'node:readline';
import { pathToFileURL } from 'node:url';
import { captureLocalIpcEndpointContext, createNodeIpcConnector } from '@c-two/c2-mem-ffi';

const config = JSON.parse(await readFile(process.argv[2], 'utf8'));
if (config.portable) await (await import('fastdb4ts/payload')).initPayload();
const manager = await import(pathToFileURL(config.managerModule).href);
const builder = await import(pathToFileURL(config.builderModule).href);
const endpointContext = captureLocalIpcEndpointContext(config.ipcRoot === null ? {} : {ipcRoot: config.ipcRoot});
const connector = createNodeIpcConnector({endpointContext});
const preparations = [];
let deactivateNextCall = false;
const observations = [];
const http = [];
const failures = [];
const dispatches = [];
const streams = new Map();
const retainedItems = new Map();
let pause;
const deferred = () => {
  let resolve;
  const promise = new Promise(done => { resolve = done; });
  return {promise, resolve};
};
async function pauseAt(target) {
  if (pause?.target !== target || pause.used) return;
  const current = pause;
  current.used = true;
  current.reached.resolve();
  await current.release.promise;
}
const ipc = {
    async connect(address, timeout, options) {
      const endpoint = await connector(address, timeout, options);
      return {
        async prepare(route, contract, token) {
          try {
            const binding = await endpoint.prepare(route, contract, token);
            preparations.push({route, ...binding, connectionId: String(binding.connectionId)});
            return binding;
          } catch (error) {
            failures.push({route, error: describeError(error)});
            throw error;
          }
        },
        beginCall: (...args) => {
          if (deactivateNextCall) {
            deactivateNextCall = false;
            endpoint.deactivate();
          }
          const native = endpoint.beginCall(...args);
          return {
            chargeInput: (...values) => native.chargeInput(...values),
            async dispatch(bytes) {
              await pauseAt('dispatch');
              dispatches.push({route: args[0], method: args[1], stream: args[2]});
              return native.dispatch(bytes);
            },
            async next() {
              const value = await native.next();
              await pauseAt('response');
              return value;
            },
            cancel: () => native.cancel(),
            observe: (...values) => native.observe(...values),
            checkDeadline: () => native.checkDeadline(), close: () => native.close(),
          };
        },
        deactivate: () => endpoint.deactivate(),
        close: () => endpoint.close(),
      };
    },
  };
const options = {
  async fetch(url, init) {
    const record = {url, method: init.method};
    http.push(record);
    if (init.method === 'POST') await pauseAt('post');
    const response = await fetch(url, init);
    record.status = response.status;
    if (init.method === 'POST' && pause?.target === 'http-response' && !pause.used) {
      record.completeBodyBytes = (await response.clone().arrayBuffer()).byteLength;
      await pauseAt('http-response');
    }
    if (new URL(url).pathname.startsWith('/_resolve/')) await pauseAt('resolve');
    return response;
  },
  observe: observation => observations.push(observation),
};
const createTransport = () => config.mode === 'direct'
  ? manager.createIpcEncodedTransport(config.address, {...ipc, ...options})
  : manager.createRelayAwareHttpEncodedTransport(config.address, {
    ...options,
    ...(config.mode === 'http' ? {} : {
      headers: {'x-c2-local-namespace': endpointContext.namespaceId()}, ipc,
    }),
  });
const transports = new Map();
function stateFor(name = 'main') {
  if (!transports.has(name)) {
    const transport = createTransport();
    transports.set(name, {transport, clients: {
      manager: new manager.ContractClient(transport, 'manager'),
      builder: new builder.ContractClient(transport, 'builder'),
    }});
  }
  return transports.get(name);
}
const clientFor = (route, name) => {
  const state = stateFor(name);
  return state.clients[route] ??= new manager.ContractClient(state.transport, route);
};
const reply = value => process.stdout.write(`${JSON.stringify(value)}\n`);
const pending = new Map();
function describeError(error) {
  return {
    name: error.name, message: error.message, code: error.code,
    transportPhase: error.transportPhase, retrySafe: error.retrySafe,
    nativeDetails: error.nativeDetails, details: error.details,
    status: error.status, body: error.body,
    ...(error.cause instanceof Error ? {cause: describeError(error.cause)} : {}),
  };
}
const evidence = () => ({
  preparations, failures, observations, http, dispatches,
  physicalConnections: new Set(preparations.map(item => item.connectionId)).size,
});
function decodeEvidence(payload, release = true) {
  const sequence = payload.entryView(0);
  const item = sequence.at(0n);
  const access = item.acquire();
  try { return JSON.parse(access.str()); }
  finally {
    access.dispose(); item.dispose(); sequence.dispose();
    if (release) payload.dispose();
  }
}
async function outcome(operation) {
  try { return {ok: true, ...await operation()}; }
  catch (error) { return {ok: false, error: describeError(error)}; }
}
async function call(route, name) {
  try {
    const payload = await clientFor(route, name).method_0_ping();
    if (!config.portable) {
      assert.equal(payload, undefined);
      return {ok: true};
    }
    return {ok: true, value: decodeEvidence(payload)};
  } catch (error) {
    return {ok: false, error: describeError(error)};
  }
}
reply({ready: true});
try {
  for await (const line of createInterface({input: process.stdin, crlfDelay: Infinity})) {
    const command = JSON.parse(line);
    if (command.op === 'exit') {
      pause?.release.resolve();
      for (const stream of streams.values()) await stream.close(null);
      for (const item of retainedItems.values()) item.dispose();
      await Promise.all([...transports.values()].map(state => state.transport.close()));
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
    if (command.op === 'snapshot') {
      reply({ok: true, stats: connector.stats(), ...evidence()});
      continue;
    }
    if (command.op === 'start-call') {
      assert(!pending.has(command.id));
      pending.set(command.id, call(command.route, command.client));
      reply({ok: true});
      continue;
    }
    if (command.op === 'finish-call') {
      assert(pending.has(command.id));
      const outcome = await pending.get(command.id);
      pending.delete(command.id);
      reply({...outcome, ...evidence()});
      continue;
    }
    if (command.op === 'prepare') {
      assert.equal(config.mode, 'direct');
      let outcome;
      try {
        await stateFor(command.client).transport.prepare(command.route, manager.CONTRACT);
        outcome = {ok: true};
      } catch (error) {
        outcome = {ok: false, error: describeError(error)};
      }
      reply({...outcome, ...evidence()});
      continue;
    }
    if (command.op === 'pause') {
      pause = {target: command.target, used: false, reached: deferred(), release: deferred()};
      reply({ok: true});
      continue;
    }
    if (command.op === 'wait-paused') {
      await pause.reached.promise;
      reply({ok: true, ...evidence()});
      continue;
    }
    if (command.op === 'resume') {
      pause.release.resolve();
      reply({ok: true});
      continue;
    }
    if (command.op === 'wait-call') {
      reply({...await pending.get(command.id), ...evidence()});
      continue;
    }
    if (command.op === 'call-many') {
      const ready = deferred();
      const calls = Array.from({length: command.count}, async () => {
        await ready.promise;
        return call(command.route, command.client);
      });
      ready.resolve();
      const outcomes = await Promise.all(calls);
      reply({ok: outcomes.every(value => value.ok), outcomes, ...evidence()});
      continue;
    }
    if (command.op === 'stream-open') {
      streams.set(command.id, clientFor(command.route, command.client).method_1_items());
      reply({ok: true});
      continue;
    }
    if (command.op === 'stream-next' || command.op === 'start-next') {
      const next = outcome(async () => {
        const item = await streams.get(command.id).next();
        if (!item.done && command.retain) retainedItems.set(command.id, item.value);
        return {done: item.done, ...(item.done ? {} : {value: decodeEvidence(item.value, !command.retain)})};
      });
      if (command.op === 'start-next') {
        pending.set(command.id, next);
        reply({ok: true});
      } else reply({...await next, ...evidence()});
      continue;
    }
    if (command.op === 'retained-item') {
      reply({ok: true, value: decodeEvidence(retainedItems.get(command.id), false)});
      continue;
    }
    if (command.op === 'release-item') {
      retainedItems.get(command.id).dispose();
      retainedItems.delete(command.id);
      reply({ok: true});
      continue;
    }
    if (command.op === 'stream-close') {
      const receipt = await streams.get(command.id).close(null);
      streams.delete(command.id);
      reply({ok: true, receipt: {
        completed: receipt.completed,
        completeItems: receipt.completeItems?.toString() ?? null,
        error: receipt.error === null ? null : describeError(receipt.error),
      }, stats: connector.stats(), ...evidence()});
      continue;
    }
    assert.equal(command.op, 'call');
    reply({...await call(command.route, command.client), ...evidence()});
  }
} finally {
  pause?.release.resolve();
  await Promise.all([...transports.values()].map(state => state.transport.close()));
  connector.close();
  endpointContext.close();
}
