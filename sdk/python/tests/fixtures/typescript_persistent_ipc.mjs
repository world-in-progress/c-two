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
const runtime = createBundledC2MemFfiNodeRuntime({ipcRoot: config.ipcRoot ?? undefined});
const transports = new Map();

function stateFor(id, command) {
  let state = transports.get(id);
  if (state !== undefined) return state;
  state = {
    connects: 0, businessWrites: 0, observations: [], loseReply: false,
    chunkWrites: 0, completedChunkWrites: 0, chunkRequests: 0, activeWrites: 0, writesAfterClose: 0, closes: 0,
    pauseFirstChunk: undefined,
  };
  const transport = manager.createIpcEncodedTransport(config.address, {
    requestChunkSize: 4096,
    connectTimeoutMs: command.connectTimeoutMs,
    async connect(address, timeoutMs) {
      if (command.connectDelayMs !== undefined) {
        await new Promise(resolve => setTimeout(resolve, command.connectDelayMs));
      }
      const connection = await runtime.connect(address, timeoutMs);
      state.connects += 1;
      let loseBusinessReply = false;
      let closed = false;
      return {
        get deactivated() { return connection.deactivated; },
        deactivation: connection.deactivation,
        async write(bytes) {
          const header = new DataView(bytes.buffer, bytes.byteOffset, bytes.byteLength);
          const flags = header.getUint32(12, true);
          if (closed) state.writesAfterClose += 1;
          if ((flags & (1 << 7)) !== 0) {
            state.businessWrites += 1;
            if ((flags & (1 << 9)) !== 0) {
              state.chunkWrites += 1;
              if (header.getUint16(16, true) === 0) state.chunkRequests += 1;
            }
            loseBusinessReply = state.loseReply;
            state.loseReply = false;
          }
          state.activeWrites += 1;
          try {
            await connection.write(bytes);
            if ((flags & (1 << 7)) !== 0 && (flags & (1 << 9)) !== 0
                && header.getUint16(16, true) === 0 && state.pauseFirstChunk !== undefined) {
              state.pauseFirstChunk.written();
              await state.pauseFirstChunk.resume;
              state.pauseFirstChunk = undefined;
            }
            if ((flags & (1 << 7)) !== 0 && (flags & (1 << 9)) !== 0) state.completedChunkWrites += 1;
          } finally {
            state.activeWrites -= 1;
          }
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
        async close() {
          if (!closed) {
            closed = true;
            state.closes += 1;
          }
          await connection.close();
        },
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
    const state = stateFor(command.transport ?? 'original', command);
    let error;
    try {
      if (command.op === 'close') {
        await state.transport.close();
        await state.transport.close();
      } else if (command.op === 'wait') {
        await new Promise(resolve => setTimeout(resolve, command.ms));
      } else if (command.op === 'prepare') {
        const contract = command.route === 'manager' ? manager.CONTRACT : builder.CONTRACT;
        await state.transport.prepare(command.route, contract);
      } else if (command.op === 'large') {
        // Reach native chunk admission with the existing no-input method;
        // this payload must be refused before Python dispatch/deserialization.
        await state.transport.call('manager', manager.CONTRACT, 'ping', new Uint8Array(64 * 1024 * 1024));
      } else if (command.op === 'closeDuringUpload') {
        let written;
        let resume;
        const firstChunk = new Promise(resolve => { written = resolve; });
        const continueUpload = new Promise(resolve => { resume = resolve; });
        state.pauseFirstChunk = {written, resume: continueUpload};
        const dispatched = state.transport.call('manager', manager.CONTRACT, 'ping', new Uint8Array(8192));
        await firstChunk;
        const closing = state.transport.close();
        const rejected = state.transport.call('manager', manager.CONTRACT, 'ping', new Uint8Array());
        try {
          await assert.rejects(rejected, /deactivated/);
        } finally {
          resume();
        }
        try {
          await dispatched;
        } catch (caught) {
          // A complete service refusal still belongs to the dispatched call.
          assert.equal(caught.name, 'C2CrmMethodError');
        }
        await closing;
        assert.equal(state.writesAfterClose, 0);
      } else if (command.op === 'copyFail') {
        const original = globalThis.Uint8Array;
        let injected = false;
        globalThis.Uint8Array = new Proxy(original, {
          construct(target, argumentsList, newTarget) {
            if (!injected && state.businessWrites > 0 && Number.isInteger(argumentsList[0])
                && argumentsList[0] > 0 && argumentsList[0] !== 16
                && new Error().stack?.includes('NodeIpcConnection.take')) {
              injected = true;
              throw new Error('injected Node local response copy allocation failure');
            }
            return Reflect.construct(target, argumentsList, newTarget);
          },
        });
        try {
          await assert.rejects(state.clients.manager.method_0_ping(), /local response payload allocation failed/);
          assert.equal(injected, true, 'Node take() local allocation failure was not reached');
        } finally {
          globalThis.Uint8Array = original;
        }
      } else {
        assert.equal(command.op, 'call');
        state.loseReply = command.uncertain === true;
        assert.equal(await state.clients[command.route].method_0_ping(), undefined);
      }
    } catch (caught) {
      error = {
        name: caught.name, message: caught.message,
        payload: caught.payload instanceof Uint8Array ? new TextDecoder().decode(caught.payload) : undefined,
      };
    }
    reply({
      ok: error === undefined, error,
      connects: state.connects, businessWrites: state.businessWrites,
      chunkWrites: state.chunkWrites, completedChunkWrites: state.completedChunkWrites, chunkRequests: state.chunkRequests,
      activeWrites: state.activeWrites, writesAfterClose: state.writesAfterClose, closes: state.closes,
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
