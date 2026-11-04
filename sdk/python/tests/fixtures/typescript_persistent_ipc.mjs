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
    pauseFirstChunk: undefined, lostReplyRequestId: undefined,
    failResponseCopy: false, responseCopyLength: undefined, copyFailureInjected: false,
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
      let closed = false;
      return {
        get deactivated() { return connection.deactivated; },
        deactivation: connection.deactivation,
        async write(bytes) {
          const header = new DataView(bytes.buffer, bytes.byteOffset, bytes.byteLength);
          const flags = header.getUint32(12, true);
          if (closed) state.writesAfterClose += 1;
          if ((flags & (1 << 12)) !== 0 && bytes[16] === 0x01
              && ((flags & (1 << 9)) === 0 || (flags & (1 << 13)) !== 0)) {
            state.businessWrites += 1;
            if ((flags & (1 << 9)) !== 0) {
              state.chunkWrites += 1;
              state.chunkRequests += 1;
            }
            if (state.loseReply) state.lostReplyRequestId = header.getBigUint64(4, true);
            state.loseReply = false;
          } else if ((flags & (1 << 12)) !== 0 && bytes[16] === 0x01 && (flags & (1 << 9)) !== 0) {
            state.chunkWrites += 1;
          }
          state.activeWrites += 1;
          try {
            await connection.write(bytes);
            if ((flags & (1 << 12)) !== 0 && (flags & (1 << 9)) !== 0
                && (flags & (1 << 13)) !== 0 && state.pauseFirstChunk !== undefined) {
              state.pauseFirstChunk.written();
              await state.pauseFirstChunk.resume;
              state.pauseFirstChunk = undefined;
            }
            if ((flags & (1 << 12)) !== 0 && (flags & (1 << 9)) !== 0) state.completedChunkWrites += 1;
          } finally {
            state.activeWrites -= 1;
          }
        },
        async readExactly(size) {
          const bytes = await connection.readExactly(size);
          if (state.responseCopyLength !== undefined && size === state.responseCopyLength) {
            state.responseCopyLength = undefined;
            state.copyFailureInjected = true;
            throw Object.assign(new Error('injected local response copy allocation failure'), {
              c2LocalAllocationFailure: true,
            });
          }
          if (size === 16) {
            const header = new DataView(bytes.buffer, bytes.byteOffset, bytes.byteLength);
            const flags = header.getUint32(12, true);
            const requestId = header.getBigUint64(4, true);
            if (flags === ((1 << 12) | (1 << 1)) && requestId === state.lostReplyRequestId) {
              state.lostReplyRequestId = undefined;
              throw new Error('injected loss after actual business reply header');
            }
            if (state.failResponseCopy && flags === ((1 << 12) | (1 << 1)) && requestId > 0n) {
              state.failResponseCopy = false;
              state.responseCopyLength = header.getUint32(0, true) - 12;
            }
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
          // Python input decoding runs after the resource execution boundary.
          assert.equal(caught.name, 'C2CrmMethodError');
        }
        await closing;
        assert.equal(state.writesAfterClose, 0);
      } else if (command.op === 'copyFail') {
        state.failResponseCopy = true;
        await assert.rejects(state.clients.manager.method_0_ping(), /local response payload allocation failed/);
        assert.equal(state.copyFailureInjected, true, 'a complete IPC 16 CALL_END body was consumed');
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
