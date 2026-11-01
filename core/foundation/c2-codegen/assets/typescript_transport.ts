export interface C2ContractIdentity {
  readonly schema: "c-two.contract.v2";
  readonly namespace: string;
  readonly name: string;
  readonly version: string;
  readonly descriptorSha256: string;
  readonly abiHash: string;
  readonly signatureHash: string;
}

export interface C2RouteContractIdentity extends C2ContractIdentity {
  readonly abiHash: string;
  readonly signatureHash: string;
}

export interface C2HeldResult<T> {
  readonly value: T;
  readonly buffer: C2ResponsePayload;
  release(): void;
}

export type C2ByteArray = Uint8Array & { readonly buffer: ArrayBufferLike };

export type C2ObservedPath =
  | "DirectIpc"
  | "ExplicitRelay"
  | "RelayAwareLocalIpc"
  | "RelayAwareHttp";

export interface C2RouteToken {
  readonly routeUid: string;
}

export interface C2ServerIdentity {
  readonly serverId: string;
  readonly serverInstanceId: string;
}

export interface C2TransportObservation extends C2RouteToken {
  readonly path: C2ObservedPath;
  readonly routeName: string;
  readonly requests: number;
  readonly responses: number;
  readonly relayUrl?: string;
  readonly ipcAddress?: string | undefined;
  readonly serverId?: string | undefined;
  readonly serverInstanceId?: string | undefined;
}

export type C2TransportObserver = (observation: C2TransportObservation) => void;

export interface C2EncodedClientTransport {
  call(routeName: string, contract: C2ContractIdentity, method: string, payload: C2ByteArray): Promise<C2ResponsePayload>;
}

export interface C2HttpRelayEncodedTransport<Payload extends C2ResponsePayload = C2ByteArray> extends C2EncodedClientTransport {
  call(routeName: string, contract: C2ContractIdentity, method: string, payload: C2ByteArray): Promise<Payload>;
}

export interface C2RelayAwareHttpEncodedTransport<Payload extends C2ResponsePayload = C2ByteArray> extends C2EncodedClientTransport {
  call(routeName: string, contract: C2ContractIdentity, method: string, payload: C2ByteArray): Promise<Payload>;
  close(): Promise<void>;
}

export interface C2IpcConnection {
  readonly memory?: C2IpcMemoryContext | undefined;
  write(data: C2ByteArray): void | Promise<void>;
  readExactly(byteLength: number): C2ByteArray | Promise<C2ByteArray>;
  close(): void | Promise<void>;
}

export type C2IpcConnect = (address: string) => C2IpcConnection | Promise<C2IpcConnection>;

export interface C2IpcShmBlock {
  readonly segmentIndex: number;
  readonly generation: number;
  readonly offset: number;
  readonly byteLength: number;
  readonly loanId: bigint;
  readonly dedicated: boolean;
}
export interface C2IpcPreparedRequest { readonly block: C2IpcShmBlock; }
export interface C2IpcMemoryWrite {
  readonly prefix: C2ByteArray;
  start(): void;
  complete(success: boolean): void;
  discard(): void;
}
/** One native ConnectionCore, bound to this connector's verified OS stream. */
export interface C2IpcMemoryContext {
  readonly prefix: string;
  finishHandshake(prefix: string | null): void;
  prepareRequest(payload: C2ByteArray): C2IpcPreparedRequest;
  discardRequest(request: C2IpcPreparedRequest): void;
  admitResponse(block: C2IpcShmBlock): unknown;
  consumeResponse(response: unknown, destination: C2ByteArray): void;
  discardResponse(response: unknown): void;
  beginWrite(kind: 0 | 1 | 2, request?: C2IpcPreparedRequest): C2IpcMemoryWrite | undefined;
  receiveRelease(frame: C2ByteArray): void;
  deactivate(): void;
  tryClose(): boolean;
}

export interface C2IpcTransportOptions<Payload extends C2ResponsePayload = C2ByteArray> {
  readonly connect: C2IpcConnect;
  readonly responsePayloadAllocator?: C2ResponsePayloadAllocator<Payload>;
  readonly requestShmThreshold?: number;
  readonly requestChunkSize?: number;
  readonly expectedServerIdentity?: C2ServerIdentity;
  readonly expectedRouteToken?: C2RouteToken;
  readonly observationPath?: "DirectIpc" | "RelayAwareLocalIpc";
  readonly observe?: C2TransportObserver | undefined;
}

export interface C2IpcEncodedTransport<Payload extends C2ResponsePayload = C2ByteArray> extends C2EncodedClientTransport {
  prepare(routeName: string, contract: C2ContractIdentity): Promise<void>;
  call(routeName: string, contract: C2ContractIdentity, method: string, payload: C2ByteArray): Promise<Payload>;
  close(): Promise<void>;
}

export interface C2ProviderOwnedResponsePayload {
  readonly byteLength: number;
}

export type C2ResponsePayload = C2ByteArray | ArrayBufferLike | C2ProviderOwnedResponsePayload;

export interface C2AllocatedResponsePayload<Payload extends C2ResponsePayload = C2ResponsePayload> {
  readonly payload: Payload;
  readonly view: C2ByteArray;
  release?(): void;
}

export type C2ResponsePayloadAllocator<Payload extends C2ResponsePayload = C2ResponsePayload> = (byteLength: number) => C2AllocatedResponsePayload<Payload>;

export type C2ResponsePayloadUnknownLengthStrategy = "reject" | "buffer";

export interface C2FetchHeaders {
  get(name: string): string | null;
}

export interface C2ReadableStreamReader {
  read(): Promise<{ done: boolean; value?: C2ByteArray }>;
  releaseLock?(): void;
}

export interface C2ReadableStream {
  getReader(): C2ReadableStreamReader;
}

export interface C2FetchResponse {
  readonly status: number;
  readonly headers?: C2FetchHeaders;
  readonly body?: C2ReadableStream | null;
  arrayBuffer(): Promise<ArrayBufferLike>;
  text(): Promise<string>;
}

export interface C2AbortSignal {
  readonly aborted: boolean;
  addEventListener(type: "abort", listener: () => void, options?: { readonly once?: boolean }): void;
}

export type C2Fetch = (
  input: string,
  init: {
    readonly method: "GET" | "POST";
    readonly headers: Readonly<Record<string, string>>;
    readonly body?: C2ByteArray;
    readonly signal?: C2AbortSignal | undefined;
  },
) => Promise<C2FetchResponse>;

export interface C2HttpRelayTransportOptions<Payload extends C2ResponsePayload = C2ByteArray> {
  readonly fetch?: C2Fetch;
  readonly headers?: Readonly<Record<string, string>>;
  readonly callTimeoutMs?: number;
  readonly resolveTimeoutMs?: number;
  readonly responsePayloadAllocator?: C2ResponsePayloadAllocator<Payload>;
  readonly responsePayloadUnknownLengthStrategy?: C2ResponsePayloadUnknownLengthStrategy;
  readonly responsePayloadUnknownLengthMaxBytes?: number;
  readonly observe?: C2TransportObserver | undefined;
}

export interface C2RelayAwareHttpTransportOptions<Payload extends C2ResponsePayload = C2ByteArray> extends C2HttpRelayTransportOptions<Payload> {
  readonly maxAttempts?: number;
  readonly routeCacheTtlMs?: number;
  readonly ipc?: C2IpcTransportOptions<Payload>;
}

interface C2RelayRouteInfo {
  readonly name: string;
  readonly relayUrl: string;
  readonly routeUid: string;
  readonly ipcAddress?: string | undefined;
  readonly serverId?: string | undefined;
  readonly serverInstanceId?: string | undefined;
  readonly crmNs: string;
  readonly crmName: string;
  readonly crmVer: string;
  readonly abiHash: string;
  readonly signatureHash: string;
  readonly maxPayloadSize: number;
}

interface C2RelayRouteCacheEntry {
  readonly routeName: string;
  readonly routes: readonly C2RelayRouteInfo[];
  readonly expiresAtMs: number;
}

interface C2IpcMethodInfo {
  readonly name: string;
  readonly index: number;
}

interface C2IpcRouteInfo {
  readonly name: string;
  readonly routeUid: string;
  readonly crmNs: string;
  readonly crmName: string;
  readonly crmVer: string;
  readonly abiHash: string;
  readonly signatureHash: string;
  readonly maxPayloadSize: number;
  readonly methods: readonly C2IpcMethodInfo[];
}

interface C2IpcHandshake {
  readonly shmPrefix: string;
  readonly capabilityFlags: number;
  readonly serverIdentity: C2ServerIdentity;
}

interface C2IpcOpenConnection {
  readonly connection: C2IpcConnection;
  readonly handshake: C2IpcHandshake;
  writeChain: Promise<void>;
  closing: boolean;
  discarded?: boolean;
  reading: Promise<void> | undefined;
}

interface C2IpcFrame {
  readonly requestId: bigint;
  readonly flags: number;
  readonly payload: Uint8Array;
}

interface C2IpcReplyChunk {
  readonly totalSize: number;
  readonly totalChunks: number;
  readonly chunkIndex: number;
  readonly data: Uint8Array;
}

type C2IpcCallResponse =
  | { readonly kind: "inline"; readonly payload: Uint8Array }
  | { readonly kind: "shm"; readonly block: C2IpcShmBlock; readonly response: unknown };

type C2IpcReplyControl =
  | { readonly kind: "success"; readonly payload: Uint8Array }
  | { readonly kind: "error"; readonly payload: Uint8Array }
  | { readonly kind: "route-not-found"; readonly routeName: string };

export class C2HttpRelayError extends Error {
  constructor(readonly status: number, readonly body: string) {
    super(`C-Two HTTP relay call failed with status ${status}: ${body}`);
    this.name = "C2HttpRelayError";
    Object.setPrototypeOf(this, C2HttpRelayError.prototype);
  }
}

export class C2HttpTransportError extends Error {
  constructor(message: string) {
    super(`C-Two HTTP transport error: ${message}`);
    this.name = "C2HttpTransportError";
    Object.setPrototypeOf(this, C2HttpTransportError.prototype);
  }
}

export class C2CrmMethodError extends Error {
  constructor(readonly payload: Uint8Array) {
    super("CRM method error");
    this.name = "C2CrmMethodError";
    Object.setPrototypeOf(this, C2CrmMethodError.prototype);
  }
}

export class C2IpcTransportError extends Error {
  readonly category: "protocol_version_mismatch" | null;
  readonly localProtocolVersion: number | null;
  readonly peerProtocolVersion: number | null;

  constructor(message: string, protocolVersionMismatch?: {
    readonly localProtocolVersion: number;
    readonly peerProtocolVersion: number;
  }) {
    super(`C-Two IPC transport error: ${message}`);
    this.name = "C2IpcTransportError";
    Object.setPrototypeOf(this, C2IpcTransportError.prototype);
    if (protocolVersionMismatch !== undefined) {
      const { localProtocolVersion, peerProtocolVersion } = protocolVersionMismatch;
      this.category = "protocol_version_mismatch";
      this.localProtocolVersion = localProtocolVersion;
      this.peerProtocolVersion = peerProtocolVersion;
    } else {
      this.category = null;
      this.localProtocolVersion = null;
      this.peerProtocolVersion = null;
    }
  }
}

export class C2IpcRouteNotFoundError extends Error {
  constructor(readonly routeName: string) {
    super(`C-Two IPC route not found: ${routeName}`);
    this.name = "C2IpcRouteNotFoundError";
    Object.setPrototypeOf(this, C2IpcRouteNotFoundError.prototype);
  }
}

export function createIpcEncodedTransport<Payload extends C2ResponsePayload = C2ByteArray>(address: string, options: C2IpcTransportOptions<Payload>): C2IpcEncodedTransport<Payload> {
  validateIpcAddress(address);
  const connect = normalizeIpcConnect(options, "C-Two IPC transport options");
  const responsePayloadAllocator = normalizeResponsePayloadAllocator(options.responsePayloadAllocator, "C-Two IPC responsePayloadAllocator");
  const requestShmThreshold = normalizeIpcRequestShmThreshold(options.requestShmThreshold, "C-Two IPC requestShmThreshold");
  const requestChunkSize = normalizeIpcRequestChunkSize(options.requestChunkSize, "C-Two IPC requestChunkSize");
  const expectedServerIdentity = normalizeExpectedServerIdentity(options.expectedServerIdentity, "C-Two IPC expectedServerIdentity");
  const expectedRouteToken = normalizeExpectedRouteToken(options.expectedRouteToken, "C-Two IPC expectedRouteToken");
  const observationPath = normalizeIpcObservationPath(options.observationPath);
  const observe = normalizeTransportObserver(options.observe, "C-Two IPC observe");
  let connectionPromise: Promise<C2IpcOpenConnection> | undefined;
  // Bindings survive stream loss and explicit close. A new connection may
  // restore this transport, but cannot redirect its already acquired routes.
  let boundServerIdentity: C2ServerIdentity | undefined;
  const boundRoutes = new Map<string, C2IpcRouteInfo>();
  let nextRequestId = 1n;
  let callChain: Promise<void> = Promise.resolve();
  let requests = 0;
  let responses = 0;

  const openConnection = (): Promise<C2IpcOpenConnection> => {
    if (connectionPromise === undefined) {
      connectionPromise = connectAndHandshakeIpc(address, connect).catch((error) => {
        connectionPromise = undefined;
        throw error;
      });
    }
    return connectionPromise;
  };

  const allocateRequestId = (): bigint => {
    const requestId = nextRequestId;
    if (requestId > 0xffffffffffffffffn) {
      void connectionPromise?.then(open => { open.connection.memory?.deactivate(); return discardConnection(open); });
      throw new C2IpcTransportError("C-Two IPC request ID space exhausted.");
    }
    nextRequestId += 1n;
    return requestId;
  };

  const discardConnection = async (open: C2IpcOpenConnection): Promise<void> => {
    connectionPromise = undefined;
    if (open.discarded) return;
    open.discarded = true;
    try {
      open.connection.memory?.deactivate();
      await open.connection.close();
    } catch (cleanupError) {
      // Preserve the primary stream failure, including uncertain writes.
      console.warn("Warning: IPC connection cleanup failed:", cleanupError);
    }
  };

  const flushReleases = async (open: C2IpcOpenConnection): Promise<void> => {
    try { await writeIpcFrame(open); }
    catch (error) { await discardConnection(open); throw error; }
  };

  const control = async (
    open: C2IpcOpenConnection,
    tag: number,
    request: unknown,
    responseTag: number,
  ): Promise<C2IpcCatalogReply> => {
    const requestId = allocateRequestId();
    try {
      if (open.closing) throw new C2IpcTransportError("C-Two IPC connection is closing.");
      // New route queries require business admission, even with CTRL on wire.
      await writeIpcFrame(open, encodeIpcFrame(requestId, C2_IPC_FLAG_CTRL,
        concatUint8Arrays([new Uint8Array([tag]), C2_TEXT_ENCODER.encode(JSON.stringify(request))])));
      const frame = await readIpcResponseFrame(open);
      if (frame.requestId !== requestId || frame.flags !== (C2_IPC_FLAG_RESPONSE | C2_IPC_FLAG_CTRL)) {
        throw new C2IpcTransportError("C-Two IPC route control reply has invalid request id or flags.");
      }
      return decodeIpcCatalogReply(frame.payload, responseTag);
    } catch (error) {
      await discardConnection(open);
      if (error instanceof C2IpcTransportError) {
        throw error;
      }
      throw new C2IpcTransportError(`C-Two IPC route control failed: ${String(error)}`);
    }
  };

  const prepareRoute = async (
    routeName: string,
    contract: C2ContractIdentity,
  ): Promise<{ readonly open: C2IpcOpenConnection; readonly route: C2IpcRouteInfo }> => {
    requireRouteNamePathValue(routeName);
    const routeContract = requireRouteContractIdentity(contract);
    const boundRoute = boundRoutes.get(routeName);
    if (boundRoute !== undefined) {
      requireMatchingIpcRoute(boundRoute, routeName, routeContract);
    }
    const open = await openConnection();
    requireExpectedServerIdentity(open.handshake.serverIdentity, expectedServerIdentity);
    requireExpectedServerIdentity(open.handshake.serverIdentity, boundServerIdentity);
    const expected = {
      route_name: routeName,
      crm_ns: routeContract.namespace,
      crm_name: routeContract.name,
      crm_ver: routeContract.version,
      abi_hash: routeContract.abiHash,
      signature_hash: routeContract.signatureHash,
    };
    const token = boundRoute ?? expectedRouteToken;
    const request = {
      expected,
      observed_route_uid: token?.routeUid ?? null,
    };
    // The handshake route list is a snapshot, never an acquire authority.
    const reply = await control(open, C2_IPC_ROUTE_LOOKUP, request, C2_IPC_ROUTE_LOOKUP_ACK);
    if (reply.kind === "nack") {
      throw new C2IpcTransportError(`C-Two IPC route catalog NACK ${reply.code}: ${reply.message}`);
    }
    if (reply.routeName !== routeName) {
      await discardConnection(open);
      throw new C2IpcTransportError("C-Two IPC route lookup reply names a different route.");
    }
    if (reply.status === "not_found") {
      throw new C2IpcRouteNotFoundError(routeName);
    }
    if (reply.status === "removed" || reply.status === "closed") {
      throw new C2IpcTransportError(`C-Two IPC route ${routeName} is ${reply.status}.`);
    }
    if (reply.status === "contract_mismatch") {
      throw new C2IpcTransportError(`C-Two IPC route ${routeName} does not match the expected CRM contract.`);
    }
    if (!("current" in reply)) {
      throw new C2IpcTransportError("C-Two IPC route lookup is missing its current record.");
    }
    const current = reply.current;
    requireExpectedServerIdentity(current.serverIdentity, open.handshake.serverIdentity);
    const route = current.route;
    requireMatchingIpcRoute(route, routeName, routeContract);
    requireExpectedRouteToken(route, expectedRouteToken);
    requireExpectedRouteToken(route, boundRoute);
    if (current.state !== "ready") {
      throw new C2IpcTransportError(`C-Two IPC route ${routeName} is ${current.state}.`);
    }
    if (boundServerIdentity === undefined) {
      boundServerIdentity = open.handshake.serverIdentity;
    }
    if (boundRoute === undefined) {
      boundRoutes.set(routeName, route);
    }
    return { open, route };
  };

  const enqueue = <T>(run: () => Promise<T>): Promise<T> => {
    const result = callChain.then(run, run);
    callChain = result.then(() => undefined, () => undefined);
    return result;
  };

  return {
    async prepare(routeName: string, contract: C2ContractIdentity): Promise<void> {
      await enqueue(async () => { await prepareRoute(routeName, contract); });
    },

    async call(routeName: string, contract: C2ContractIdentity, method: string, payload: Uint8Array): Promise<Payload> {
      const run = async (): Promise<Payload> => {
        requireMethodPathValue(method);
        const requestPayload = requireUint8Array(payload, "payload");
        const { open, route } = await prepareRoute(routeName, contract);
        if (requestPayload.byteLength > route.maxPayloadSize) {
          throw new C2IpcTransportError(`C-Two IPC route ${routeName} payload ${requestPayload.byteLength} exceeds max_payload_size ${route.maxPayloadSize}.`);
        }
        const methodInfo = findIpcMethod(route, method);
        const requestId = allocateRequestId();
        let responseError: unknown;
        let responseValue: Payload | undefined;
        // The server's handshake ACK gates every outgoing shared-memory
        // request. Without CAP_SHM_REQUEST the call automatically stays on the
        // existing owned inline/chunked paths, never touching the writer.
        const useShm = open.connection.memory !== undefined
          && (open.handshake.capabilityFlags & C2_IPC_CAP_SHM_REQUEST) !== 0
          && requestPayload.byteLength > requestShmThreshold
          && requestPayload.byteLength <= 0xffffffff;
        const chunked = !useShm && requestPayload.byteLength > requestChunkSize;
        const receive = async (): Promise<C2IpcCallResponse> => {
          let frame: C2IpcFrame;
          try {
            try {
              frame = await readIpcResponseFrame(open);
            } catch (error) {
              throw new C2IpcTransportError(`C-Two IPC call response read failed: ${String(error)}`);
            }
            if (frame.requestId !== requestId) {
              throw new C2IpcTransportError(`C-Two IPC response request id ${frame.requestId} did not match request id ${requestId}.`);
            }
            if ((frame.flags & C2_IPC_FLAG_RESPONSE) === 0 || (frame.flags & C2_IPC_FLAG_REPLY_V2) === 0) {
              throw new C2IpcTransportError("C-Two IPC response is not a v2 reply frame.");
            }
          } catch (error) {
            if (!chunked) await discardConnection(open);
            throw error;
          }
          let buddy: C2IpcShmBlock | undefined;
          if ((frame.flags & C2_IPC_FLAG_BUDDY) !== 0) {
            try {
              if ((open.handshake.capabilityFlags & C2_IPC_CAP_SHM_RESPONSE) === 0) {
                throw new C2IpcTransportError("C-Two IPC server sent an SHM reply without the acknowledged SHM response capability.");
              }
              if (open.connection.memory === undefined) throw new C2IpcTransportError("C-Two IPC SHM response has no native memory context.");
              if ((frame.flags & C2_IPC_FLAG_CHUNKED) !== 0) {
                throw new C2IpcTransportError("C-Two IPC chunked SHM responses are not supported by this generated transport yet.");
              }
              buddy = decodeIpcBuddyPayload(frame.payload);
            } catch (error) {
              await discardConnection(open);
              throw error;
            }
          } else if ((frame.flags & C2_IPC_FLAG_CHUNKED) !== 0) {
            try {
              return { kind: "inline", payload: await readIpcChunkedSuccessPayload(open, frame, requestId) };
            } catch (error) {
              if (!chunked) await discardConnection(open);
              if (error instanceof C2IpcTransportError) throw error;
              throw new C2IpcTransportError(`C-Two IPC chunked response read failed: ${String(error)}`);
            }
          }
          const memory = open.connection.memory;
          let response: unknown;
          let admitted = false;
          let delivered = false;
          try {
            if (buddy !== undefined && memory !== undefined) {
              try { response = memory.admitResponse(buddy); }
              catch (error) { await discardConnection(open); throw error; }
              admitted = true;
            }
            const reply = decodeIpcReplyControl(frame.payload.subarray(buddy === undefined ? 0 : C2_IPC_BUDDY_PAYLOAD_BYTES));
            if (reply.kind === "error") throw new C2CrmMethodError(reply.payload);
            if (reply.kind === "route-not-found") throw new C2IpcRouteNotFoundError(reply.routeName);
            if (buddy !== undefined) {
              if (reply.payload.byteLength !== 0 || memory === undefined) throw new C2IpcTransportError("Invalid SHM response control or native memory context.");
              delivered = true;
              return { kind: "shm", block: buddy, response };
            }
            return { kind: "inline", payload: reply.payload };
          } finally {
            if (admitted && !delivered) {
              memory!.discardResponse(response);
              await flushReleases(open);
            }
          }
        };
        const observeResponse = (value: Payload): Payload => {
          responses += 1;
          return observeTransportResponse(value, observe, {
            path: observationPath,
            routeName,
            routeUid: route.routeUid,
            ipcAddress: address,
            serverId: open.handshake.serverIdentity.serverId,
            serverInstanceId: open.handshake.serverIdentity.serverInstanceId,
            requests,
            responses,
          });
        };
        try {
          let callControl: Uint8Array;
          const totalChunks = chunked ? Math.ceil(requestPayload.byteLength / requestChunkSize) : 0;
          try {
            callControl = encodeIpcCallControl(route, methodInfo.index);
            if (chunked) {
              if (totalChunks <= 0 || totalChunks > 0xffff) {
                throw new C2IpcTransportError(`C-Two IPC request chunk count ${totalChunks} is outside the u16 chunk header range.`);
              }
            }
          } catch (error) {
            await discardConnection(open);
            if (error instanceof C2IpcTransportError) throw error;
            throw new C2IpcTransportError(`C-Two IPC call write failed: ${String(error)}`);
          }
          const writing = (async (): Promise<void> => {
            try {
              if (useShm) {
                await writeIpcBuddyCallRequestFrame(open, requestId, callControl, requestPayload);
              } else if (chunked) {
                for (let index = 0; index < totalChunks; index += 1) {
                  // close/discard clears this before closing the stream;
                  // callChain prevents a replacement until this exchange ends.
                  if (connectionPromise === undefined) {
                    throw new C2IpcTransportError("C-Two IPC connection is closed.");
                  }
                  const start = index * requestChunkSize;
                  const chunk = requestPayload.subarray(start, Math.min(start + requestChunkSize, requestPayload.byteLength));
                  let flags = C2_IPC_FLAG_CALL_V2 | C2_IPC_FLAG_CHUNKED;
                  if (index === totalChunks - 1) flags |= C2_IPC_FLAG_CHUNK_LAST;
                  const body = index === 0
                    ? concatUint8Arrays([encodeIpcRequestChunkHeader(index, totalChunks), callControl, chunk])
                    : concatUint8Arrays([encodeIpcRequestChunkHeader(index, totalChunks), chunk]);
                  await writeIpcFrame(open, encodeIpcFrame(requestId, flags, body));
                }
              } else {
                await writeIpcFrame(open, encodeIpcFrame(requestId, C2_IPC_FLAG_CALL_V2, concatUint8Arrays([callControl, requestPayload])));
              }
              requests += 1;
            } catch (error) {
              if (error instanceof C2IpcTransportError) throw error;
              throw new C2IpcTransportError(`C-Two IPC call write failed: ${String(error)}`);
            }
          })();
          let reply: C2IpcCallResponse;
          if (chunked) {
            const receiving = receive();
            try {
              [, reply] = await Promise.all([writing, receiving]);
            } catch (error) {
              // Preserve the first failure before close wakes the other side.
              await discardConnection(open);
              const [, received] = await Promise.allSettled([writing, receiving]);
              if (received.status === "fulfilled" && received.value.kind === "shm") {
                open.connection.memory!.discardResponse(received.value.response);
                try { await writeIpcFrame(open); } catch { /* preserve the original failed exchange */ }
              }
              await discardConnection(open);
              throw error;
            }
          } else {
            try {
              await writing;
            } catch (error) {
              await discardConnection(open);
              throw error;
            }
            reply = await receive();
          }
          if (reply.kind === "shm") {
            responseValue = await readIpcShmSuccessPayload(reply.response, reply.block.byteLength, responsePayloadAllocator, open.connection.memory!, () => flushReleases(open));
            return observeResponse(responseValue);
          }
          try {
            responseValue = copyResponsePayloadToAllocator(reply.payload, responsePayloadAllocator);
            return observeResponse(responseValue);
          } catch (error) {
            throw new C2IpcTransportError(`C-Two IPC responsePayloadAllocator failed: ${String(error)}`);
          }
        } catch (error) {
          responseError = error;
          if (chunked || error instanceof C2IpcTransportError) await discardConnection(open);
        }
        if (responseError !== undefined) {
          throw responseError;
        }
        return responseValue as Payload;
      };
      return await enqueue(run);
    },

    async close(): Promise<void> {
      const current = connectionPromise;
      connectionPromise = undefined;
      if (current === undefined) {
        return;
      }
      const open = await current.catch(() => undefined);
      if (open !== undefined) {
        let closeAttempted = false;
        try {
          if (open.connection.memory !== undefined) {
            open.closing = true;
            open.connection.memory.deactivate();
            await open.writeChain;
            await writeIpcFrame(open);
            if (!open.connection.memory.tryClose()) {
              await writeIpcFrame(open, encodeIpcFrame(0n, C2_IPC_FLAG_SIGNAL, new Uint8Array([0x08])), undefined, true);
              if (open.reading !== undefined) await open.reading;
              else {
                try { await readIpcResponseFrame(open); } catch (error) {
                  if (!open.connection.memory.tryClose()) throw error;
                }
              }
            }
          }
          closeAttempted = true;
          await open.connection.close();
        } catch (error) {
          if (!closeAttempted) {
            try { await open.connection.close(); } catch { /* preserve the settlement failure */ }
          }
          const closeError = new C2IpcTransportError(`C-Two IPC close failed: ${String(error)}`);
          closeError.cause = error;
          throw closeError;
        }
      }
    },
  };
}

export function createHttpRelayEncodedTransport<Payload extends C2ResponsePayload = C2ByteArray>(baseUrl: string, options: C2HttpRelayTransportOptions<Payload> = {}): C2HttpRelayEncodedTransport<Payload> {
  const normalizedOptions = normalizeHttpRelayTransportOptions(options, "C-Two HTTP relay transport options");
  const normalizedBaseUrl = normalizeRelayBaseUrl(baseUrl, "C-Two HTTP relay base URL");
  const fetchImpl = normalizeFetch(normalizedOptions.fetch, "C-Two HTTP relay fetch") ?? globalFetch();
  const headers = normalizeHttpHeaders(normalizedOptions.headers, "C-Two HTTP relay headers");
  const callTimeoutMs = normalizeTimeoutMs(normalizedOptions.callTimeoutMs, "C-Two HTTP relay callTimeoutMs", 300_000);
  const resolveTimeoutMs = normalizeTimeoutMs(normalizedOptions.resolveTimeoutMs, "C-Two HTTP relay resolveTimeoutMs", 5_000);
  const responsePayloadAllocator = normalizeResponsePayloadAllocator(normalizedOptions.responsePayloadAllocator, "C-Two HTTP relay responsePayloadAllocator");
  const responsePayloadUnknownLengthStrategy = normalizeResponsePayloadUnknownLengthStrategy(
    normalizedOptions.responsePayloadUnknownLengthStrategy,
    "C-Two HTTP relay responsePayloadUnknownLengthStrategy",
  );
  const responsePayloadUnknownLengthMaxBytes = normalizeUnknownLengthBufferMaxBytes(
    normalizedOptions.responsePayloadUnknownLengthMaxBytes,
    "C-Two HTTP relay responsePayloadUnknownLengthMaxBytes",
  );
  const observe = normalizeTransportObserver(normalizedOptions.observe, "C-Two HTTP relay observe");
  let requests = 0;
  let responses = 0;
  return {
    async call(routeName: string, contract: C2ContractIdentity, method: string, payload: Uint8Array): Promise<Payload> {
      requireRouteNamePathValue(routeName);
      requireMethodPathValue(method);
      const requestPayload = requireUint8Array(payload, "payload");
      const routeContract = requireRouteContractIdentity(contract);
      const routes = await resolveRelayRoutes(
        fetchImpl,
        normalizedBaseUrl,
        routeName,
        routeContract,
        headers,
        resolveTimeoutMs,
      );
      if (routes.length === 0) {
        throw resourceNotFoundError(routeName);
      }
      let route: C2RelayRouteInfo | undefined;
      let lastPreparationError: unknown;
      for (const candidate of routes) {
        if (requestPayload.byteLength > candidate.maxPayloadSize) {
          lastPreparationError = payloadTooLargeError(
            routeName,
            requestPayload.byteLength,
            candidate.maxPayloadSize,
          );
          continue;
        }
        try {
          await probeResolvedHttpRoute(
            fetchImpl,
            candidate.relayUrl,
            routeName,
            routeContract,
            candidate,
            headers,
            resolveTimeoutMs,
          );
          route = candidate;
          break;
        } catch (error) {
          lastPreparationError = error;
        }
      }
      if (route === undefined) {
        if (lastPreparationError instanceof Error) {
          throw lastPreparationError;
        }
        throw resourceNotFoundError(routeName);
      }
      requests += 1;
      const response = await callResolvedHttpRoute(
        fetchImpl,
        route.relayUrl,
        routeName,
        routeContract,
        route,
        method,
        requestPayload,
        headers,
        callTimeoutMs,
        responsePayloadAllocator,
        responsePayloadUnknownLengthStrategy,
        responsePayloadUnknownLengthMaxBytes,
      );
      responses += 1;
      return observeTransportResponse(response, observe, {
        path: "ExplicitRelay",
        routeName,
        routeUid: route.routeUid,
        relayUrl: route.relayUrl,
        requests,
        responses,
      });
    },
  };
}

export function createRelayAwareHttpEncodedTransport<Payload extends C2ResponsePayload = C2ByteArray>(anchorUrl: string, options: C2RelayAwareHttpTransportOptions<Payload> = {}): C2RelayAwareHttpEncodedTransport<Payload> {
  const normalizedOptions = normalizeHttpRelayTransportOptions(options, "C-Two relay-aware HTTP transport options");
  const normalizedAnchorUrl = normalizeRelayBaseUrl(anchorUrl, "C-Two relay anchor URL");
  const fetchImpl = normalizeFetch(normalizedOptions.fetch, "C-Two relay-aware HTTP fetch") ?? globalFetch();
  const headers = normalizeHttpHeaders(normalizedOptions.headers, "C-Two HTTP relay headers");
  const maxAttempts = normalizeRelayMaxAttempts(normalizedOptions.maxAttempts);
  const routeCacheTtlMs = normalizeRelayRouteCacheTtlMs(normalizedOptions.routeCacheTtlMs);
  const callTimeoutMs = normalizeTimeoutMs(normalizedOptions.callTimeoutMs, "C-Two HTTP relay callTimeoutMs", 300_000);
  const resolveTimeoutMs = normalizeTimeoutMs(normalizedOptions.resolveTimeoutMs, "C-Two relay resolveTimeoutMs", 5_000);
  const responsePayloadAllocator = normalizeResponsePayloadAllocator(normalizedOptions.responsePayloadAllocator, "C-Two relay-aware HTTP responsePayloadAllocator");
  const responsePayloadUnknownLengthStrategy = normalizeResponsePayloadUnknownLengthStrategy(
    normalizedOptions.responsePayloadUnknownLengthStrategy,
    "C-Two relay-aware HTTP responsePayloadUnknownLengthStrategy",
  );
  const responsePayloadUnknownLengthMaxBytes = normalizeUnknownLengthBufferMaxBytes(
    normalizedOptions.responsePayloadUnknownLengthMaxBytes,
    "C-Two relay-aware HTTP responsePayloadUnknownLengthMaxBytes",
  );
  const observe = normalizeTransportObserver(normalizedOptions.observe, "C-Two relay-aware HTTP observe");
  const ipcOptions = normalizeRelayAwareIpcOptions(normalizedOptions.ipc);
  const routeCache = new Map<string, C2RelayRouteCacheEntry>();
  const currentRelayByCacheKey = new Map<string, string>();
  let requests = 0;
  let responses = 0;
  return {
    async call(routeName: string, contract: C2ContractIdentity, method: string, payload: Uint8Array): Promise<Payload> {
      requireRouteNamePathValue(routeName);
      requireMethodPathValue(method);
      const requestPayload = requireUint8Array(payload, "payload");
      const routeContract = requireRouteContractIdentity(contract);
      const cacheKey = relayRouteCacheKey(routeName, routeContract);
      let lastError: unknown;
      const excludedRoutes = new Set<string>();
      const excludedLocalRouteTokens = new Set<string>();
      for (let attempt = 0; attempt < maxAttempts; attempt += 1) {
        let routes: readonly C2RelayRouteInfo[];
        try {
          routes = await resolveRelayRoutesCached(
            fetchImpl,
            normalizedAnchorUrl,
            routeName,
            routeContract,
            headers,
            routeCache,
            routeCacheTtlMs,
            resolveTimeoutMs,
            attempt > 0,
          );
        } catch (error) {
          lastError = error;
          if (isRetryableResolveError(error) && attempt + 1 < maxAttempts) {
            continue;
          }
          throw error;
        }
        if (routes.length === 0) {
          throw resourceNotFoundError(routeName);
        }
        const localRoute = selectLocalIpcRoute(
          routes,
          normalizedAnchorUrl,
          ipcOptions,
          excludedLocalRouteTokens,
        );
        if (localRoute !== undefined && ipcOptions !== undefined) {
          const localTokenKey = routeTokenKey(localRoute);
          const localTransport = createIpcEncodedTransport<Payload>(
            localRoute.ipcAddress as string,
            {
              ...ipcOptions,
              expectedServerIdentity: {
                serverId: localRoute.serverId as string,
                serverInstanceId: localRoute.serverInstanceId as string,
              },
              expectedRouteToken: {
                routeUid: localRoute.routeUid,
              },
              observationPath: "RelayAwareLocalIpc",
              observe,
            },
          );
          try {
            await localTransport.prepare(routeName, routeContract);
          } catch (error) {
            try {
              await localTransport.close();
            } catch (closeError) {
              const primaryError = error instanceof Error ? error : new C2IpcTransportError(
                `C-Two relay-aware local IPC preparation failed: ${String(error)}`,
              );
              primaryError.cause = closeError;
              throw primaryError;
            }
            if (error instanceof C2IpcTransportError && error.category === "protocol_version_mismatch") {
              throw error;
            }
            lastError = error;
            excludedLocalRouteTokens.add(localTokenKey);
            invalidateRelayRouteCache(routeCache, cacheKey);
            const distinctRoutes = routes.filter(
              (route) => routeTokenKey(route) !== localTokenKey,
            );
            if (distinctRoutes.length === 0) {
              throw samePathFallbackDeniedError(routeName, localRoute);
            }
            continue;
          }
          return await callPreparedLocalIpcAndClose(
            localTransport,
            routeName,
            routeContract,
            method,
            requestPayload,
          );
        }
        const httpRoutes = routes.filter(
          (route) => !excludedLocalRouteTokens.has(routeTokenKey(route)),
        );
        if (httpRoutes.length === 0 && excludedLocalRouteTokens.size > 0) {
          throw samePathFallbackDeniedError(routeName, routes[0]);
        }
        const orderedRoutes = orderRelayRoutes(httpRoutes, currentRelayByCacheKey.get(cacheKey), excludedRoutes);
        if (orderedRoutes.length === 0) {
          if (lastError instanceof Error) {
            throw lastError;
          }
          throw resourceNotFoundError(routeName);
        }
        let staleRoute = false;
        for (const route of orderedRoutes) {
          const relayUrl = normalizeRelayBaseUrl(route.relayUrl, "C-Two resolved relay URL");
          if (requestPayload.byteLength > route.maxPayloadSize) {
            lastError = payloadTooLargeError(routeName, requestPayload.byteLength, route.maxPayloadSize);
            continue;
          }
          try {
            await probeResolvedHttpRoute(
              fetchImpl,
              relayUrl,
              routeName,
              routeContract,
              route,
              headers,
              resolveTimeoutMs,
            );
            requests += 1;
            const result = await callResolvedHttpRoute(
              fetchImpl,
              relayUrl,
              routeName,
              routeContract,
              route,
              method,
              requestPayload,
              headers,
              callTimeoutMs,
              responsePayloadAllocator,
              responsePayloadUnknownLengthStrategy,
              responsePayloadUnknownLengthMaxBytes,
            );
            responses += 1;
            currentRelayByCacheKey.set(cacheKey, relayUrl);
            return observeTransportResponse(result, observe, {
              path: "RelayAwareHttp",
              routeName,
              routeUid: route.routeUid,
              relayUrl,
              requests,
              responses,
            });
          } catch (error) {
            lastError = error;
            if (!isRetryableRouteError(error)) {
              throw error;
            }
            invalidateRelayRouteCache(routeCache, cacheKey);
            currentRelayByCacheKey.delete(cacheKey);
            excludedRoutes.add(relayUrl);
            staleRoute = true;
            break;
          }
        }
        if (lastError instanceof C2HttpRelayError && lastError.status === 413) {
          throw lastError;
        }
        if (staleRoute) {
          continue;
        }
      }
      if (lastError instanceof Error) {
        throw lastError;
      }
      throw resourceNotFoundError(routeName);
    },

    async close(): Promise<void> {
      routeCache.clear();
      currentRelayByCacheKey.clear();
    },
  };
}

const C2_IPC_HANDSHAKE_VERSION = 15;
const C2_IPC_FRAME_BODY_HEADER_BYTES = 12;
const C2_IPC_FRAME_HEADER_BYTES = 16;
const C2_IPC_BUDDY_PAYLOAD_BYTES = 23;
const C2_IPC_BUDDY_FLAG_DEDICATED = 1 << 0;
const C2_IPC_REPLY_CHUNK_META_BYTES = 16;
const C2_IPC_REQUEST_CHUNK_HEADER_BYTES = 4;
const C2_IPC_DEFAULT_REQUEST_SHM_THRESHOLD = 4096;
const C2_IPC_DEFAULT_REQUEST_CHUNK_SIZE = 131_072;
const C2_IPC_CAP_SHM_REQUEST = 1 << 3;
const C2_IPC_CAP_SHM_RESPONSE = 1 << 4;
const C2_IPC_FLAG_SIGNAL = 1 << 11;
const C2_IPC_FLAG_RESPONSE = 1 << 1;
const C2_IPC_FLAG_HANDSHAKE = 1 << 2;
const C2_IPC_FLAG_CTRL = 1 << 4;
const C2_IPC_FLAG_BUDDY = 1 << 6;
const C2_IPC_FLAG_CALL_V2 = 1 << 7;
const C2_IPC_FLAG_REPLY_V2 = 1 << 8;
const C2_IPC_FLAG_CHUNKED = 1 << 9;
const C2_IPC_FLAG_CHUNK_LAST = 1 << 10;
// Canonical c2-wire route catalog MsgType discriminants.
const C2_IPC_RELEASE = 0x17;
const C2_IPC_ROUTE_LOOKUP = 0x10;
const C2_IPC_ROUTE_LOOKUP_ACK = 0x11;
const C2_IPC_ROUTE_NACK = 0x15;
const C2_IPC_REPLY_STATUS_SUCCESS = 0x00;
const C2_IPC_REPLY_STATUS_ERROR = 0x01;
const C2_IPC_REPLY_STATUS_ROUTE_NOT_FOUND = 0x02;
const C2_TEXT_ENCODER = new TextEncoder();
const C2_TEXT_DECODER = new TextDecoder("utf-8", { fatal: true });

function requireUint8Array(value: unknown, label: string): Uint8Array {
  if (!(value instanceof Uint8Array)) {
    throw new Error(`${label} must be a Uint8Array.`);
  }
  return value;
}

function requireResponsePayload(value: unknown, label: string): C2ResponsePayload {
  if (value instanceof Uint8Array || isArrayBufferLike(value)) {
    return value;
  }
  if (isProviderOwnedResponsePayload(value)) {
    return value;
  }
  throw new Error(`${label} must be a Uint8Array, ArrayBuffer, or provider-owned response payload.`);
}

function isArrayBufferLike(value: unknown): value is ArrayBufferLike {
  return value instanceof ArrayBuffer || (typeof SharedArrayBuffer !== "undefined" && value instanceof SharedArrayBuffer);
}

function normalizeIpcConnect<Payload extends C2ResponsePayload>(value: C2IpcTransportOptions<Payload>, label: string): C2IpcConnect {
  if (typeof value !== "object" || value === null) {
    throw new C2IpcTransportError(`${label} must be an object.`);
  }
  if (typeof value.connect !== "function") {
    throw new C2IpcTransportError(`${label} connect must be a function.`);
  }
  return value.connect;
}

function normalizeIpcRequestShmThreshold(value: number | undefined, label: string): number {
  if (value === undefined) {
    return C2_IPC_DEFAULT_REQUEST_SHM_THRESHOLD;
  }
  if (!Number.isSafeInteger(value) || value < 0 || value > C2_MAX_RESPONSE_PAYLOAD_BYTES) {
    throw new C2IpcTransportError(`${label} must be a non-negative safe integer no greater than ${C2_MAX_RESPONSE_PAYLOAD_BYTES}.`);
  }
  return value;
}

function normalizeIpcRequestChunkSize(value: number | undefined, label: string): number {
  if (value === undefined) {
    return C2_IPC_DEFAULT_REQUEST_CHUNK_SIZE;
  }
  const maxRequestChunkSize = C2_MAX_RESPONSE_PAYLOAD_BYTES - C2_IPC_REQUEST_CHUNK_HEADER_BYTES - C2_MAX_WIRE_TEXT_BYTES - 2;
  if (!Number.isSafeInteger(value) || value <= 0 || value > maxRequestChunkSize) {
    throw new C2IpcTransportError(`${label} must be a positive safe integer no greater than ${maxRequestChunkSize}.`);
  }
  return value;
}

function requireIpcConnection(value: unknown): C2IpcConnection {
  if (typeof value !== "object" || value === null) {
    throw new C2IpcTransportError("C-Two IPC connect must return a connection object.");
  }
  const candidate = value as Partial<C2IpcConnection>;
  if (typeof candidate.write !== "function") {
    throw new C2IpcTransportError("C-Two IPC connection write must be a function.");
  }
  if (typeof candidate.readExactly !== "function") {
    throw new C2IpcTransportError("C-Two IPC connection readExactly must be a function.");
  }
  if (typeof candidate.close !== "function") {
    throw new C2IpcTransportError("C-Two IPC connection close must be a function.");
  }
  return candidate as C2IpcConnection;
}

async function connectAndHandshakeIpc(
  address: string,
  connect: C2IpcConnect,
): Promise<C2IpcOpenConnection> {
  let connection: C2IpcConnection;
  try {
    connection = requireIpcConnection(await connect(address));
  } catch (error) {
    if (error instanceof C2IpcTransportError) {
      throw error;
    }
    throw new C2IpcTransportError(`C-Two IPC connect failed: ${String(error)}`);
  }
  try {
    // Preserve local metadata validation before any bytes reach the peer.
    const metadata = encodeIpcFrame(0n, C2_IPC_FLAG_HANDSHAKE, encodeClientIpcHandshake(connection.memory));
    await connection.write(encodeClientIpcHello());
    await acceptServerIpcVersion(connection);
    await connection.write(metadata);
    const frame = await readIpcFrame(connection);
    if (frame.requestId !== 0n || frame.flags !== (C2_IPC_FLAG_RESPONSE | C2_IPC_FLAG_HANDSHAKE)) {
      throw new C2IpcTransportError("C-Two IPC server metadata response has invalid request id or flags.");
    }
    const handshake = decodeServerIpcHandshake(frame.payload);
    connection.memory?.finishHandshake((handshake.capabilityFlags & C2_IPC_CAP_SHM_RESPONSE) !== 0 ? handshake.shmPrefix : null);
    return { connection, handshake, writeChain: Promise.resolve(), closing: false, reading: undefined };
  } catch (error) {
    const primaryError = error instanceof C2IpcTransportError ? error : new C2IpcTransportError(
      `C-Two IPC handshake failed: ${String(error)}`,
    );
    try {
      await connection.close();
    } catch (closeError) {
      primaryError.cause = closeError;
    }
    throw primaryError;
  }
}

function encodeClientIpcHello(): Uint8Array {
  // This permanent 25-byte envelope is independent of business frame codecs.
  const record = new Uint8Array(25);
  const view = new DataView(record.buffer);
  view.setUint32(0, 21, true);
  view.setBigUint64(4, 0n, true);
  view.setUint32(12, 0x04, true);
  record.set([0x43, 0x32, 0x49, 0x50, 1], 16); // C2IP, Hello
  view.setUint32(21, C2_IPC_HANDSHAKE_VERSION, true);
  return record;
}

async function acceptServerIpcVersion(connection: C2IpcConnection): Promise<void> {
  // Never let an unnegotiated length select a read or payload allocation.
  const header = requireUint8Array(await connection.readExactly(16), "C-Two IPC version preamble header");
  if (header.byteLength !== 16) {
    throw new C2IpcTransportError("C-Two IPC version preamble header is truncated.");
  }
  const envelope = new DataView(header.buffer, header.byteOffset, header.byteLength);
  if (envelope.getUint32(0, true) !== 21 || envelope.getBigUint64(4, true) !== 0n || envelope.getUint32(12, true) !== 0x06) {
    throw new C2IpcTransportError("C-Two IPC version preamble has invalid length, request id or flags.");
  }
  const body = requireUint8Array(await connection.readExactly(9), "C-Two IPC version preamble body");
  if (body.byteLength !== 9) {
    throw new C2IpcTransportError("C-Two IPC version preamble body is truncated.");
  }
  if (body[0] !== 0x43 || body[1] !== 0x32 || body[2] !== 0x49 || body[3] !== 0x50 || (body[4] !== 2 && body[4] !== 3)) {
    throw new C2IpcTransportError("C-Two IPC version preamble has invalid magic or response kind.");
  }
  const peerProtocolVersion = readU32LE(body, 5);
  if (peerProtocolVersion === 0) {
    throw new C2IpcTransportError("C-Two IPC version preamble version must be positive.");
  }
  if (body[4] === 2) {
    if (peerProtocolVersion !== C2_IPC_HANDSHAKE_VERSION) {
      throw new C2IpcTransportError("C-Two IPC Accepted preamble has a different protocol version.");
    }
    return;
  }
  if (peerProtocolVersion === C2_IPC_HANDSHAKE_VERSION) {
    throw new C2IpcTransportError("C-Two IPC VersionMismatch preamble has the local protocol version.");
  }
  throw new C2IpcTransportError(
    `C-Two IPC protocol version mismatch: local=${C2_IPC_HANDSHAKE_VERSION}, peer=${peerProtocolVersion}.`,
    { localProtocolVersion: C2_IPC_HANDSHAKE_VERSION, peerProtocolVersion },
  );
}

function validateIpcAddress(address: string): void {
  if (typeof address !== "string" || !address.startsWith("ipc://")) {
    throw new C2IpcTransportError("C-Two IPC address must be an ipc:// address.");
  }
  const region = address.slice("ipc://".length);
  validateIpcRegionId(region);
}

function validateIpcRegionId(region: string): void {
  if (region.length === 0) {
    throw new C2IpcTransportError("C-Two IPC region id cannot be empty.");
  }
  if (region.trim() !== region) {
    throw new C2IpcTransportError("C-Two IPC region id cannot contain leading or trailing whitespace.");
  }
  if (region === "." || region === "..") {
    throw new C2IpcTransportError("C-Two IPC region id cannot be a relative path component.");
  }
  if (region.includes("/") || region.includes("\\")) {
    throw new C2IpcTransportError("C-Two IPC region id cannot contain path separators.");
  }
  if (/[\u0000-\u001F\u007F-\u009F]/.test(region)) {
    throw new C2IpcTransportError("C-Two IPC region id cannot contain control characters.");
  }
}

function encodeClientIpcHandshake(memory: C2IpcMemoryContext | undefined): Uint8Array {
  const capabilities = memory === undefined ? 0 : C2_IPC_CAP_SHM_REQUEST | C2_IPC_CAP_SHM_RESPONSE;
  return concatUint8Arrays([encodeIpcHandshakeText(memory?.prefix ?? "", "client SHM prefix"), writeU16LE(capabilities)]);
}

function encodeIpcHandshakeText(value: string, field: string): Uint8Array {
  requireIpcHandshakeText(value, field);
  const bytes = utf8Encode(value);
  return concatUint8Arrays([new Uint8Array([bytes.byteLength]), bytes]);
}

function requireIpcHandshakeText(value: string, field: string): void {
  if (utf8ByteLength(value) > C2_MAX_WIRE_TEXT_BYTES) {
    throw new C2IpcTransportError(`C-Two IPC ${field} cannot exceed ${C2_MAX_WIRE_TEXT_BYTES} bytes.`);
  }
  if (/[\u0000-\u001F\u007F-\u009F]/.test(value)) {
    throw new C2IpcTransportError(`C-Two IPC ${field} cannot contain control characters.`);
  }
}

function encodeIpcCallControl(route: C2IpcRouteInfo, methodIndex: number): Uint8Array {
  const routeBytes = utf8Encode(route.name);
  if (routeBytes.byteLength === 0 || routeBytes.byteLength > C2_MAX_WIRE_TEXT_BYTES) {
    throw new C2IpcTransportError(`C-Two IPC route_name is too long: ${routeBytes.byteLength} bytes.`);
  }
  const routeUidBytes = encodeIpcHandshakeText(route.routeUid, "route_uid");
  if (routeUidBytes.byteLength === 1) {
    throw new C2IpcTransportError("C-Two IPC route_uid must not be empty.");
  }
  const crmNsBytes = encodeIpcHandshakeText(route.crmNs, "crm_ns");
  const crmNameBytes = encodeIpcHandshakeText(route.crmName, "crm_name");
  const crmVerBytes = encodeIpcHandshakeText(route.crmVer, "crm_ver");
  const abiHashBytes = encodeIpcHandshakeText(route.abiHash, "abi_hash");
  const signatureHashBytes = encodeIpcHandshakeText(route.signatureHash, "signature_hash");
  if (!Number.isSafeInteger(methodIndex) || methodIndex < 0 || methodIndex > 0xffff) {
    throw new C2IpcTransportError(`C-Two IPC method index ${methodIndex} is invalid.`);
  }
  return concatUint8Arrays([
    new Uint8Array([routeBytes.byteLength]),
    routeBytes,
    routeUidBytes,
    crmNsBytes,
    crmNameBytes,
    crmVerBytes,
    abiHashBytes,
    signatureHashBytes,
    writeU16LE(methodIndex),
  ]);
}

async function writeIpcBuddyCallRequestFrame(open: C2IpcOpenConnection, requestId: bigint, control: C2ByteArray, payload: C2ByteArray): Promise<void> {
  const memory = open.connection.memory!;
  const request = memory.prepareRequest(payload);
  let handedOff = false;
  try {
    const frame = encodeIpcFrame(requestId, C2_IPC_FLAG_CALL_V2 | C2_IPC_FLAG_BUDDY,
      concatUint8Arrays([encodeIpcBuddyPayload(request.block), control]));
    handedOff = true;
    await writeIpcFrame(open, frame, request);
  } finally {
    if (!handedOff) memory.discardRequest(request);
  }
}

function requireIpcLoanId(value: unknown): asserts value is bigint {
  if (typeof value !== "bigint" || value <= 0n || value > 0xffffffffffffffffn) {
    throw new C2IpcTransportError("C-Two IPC loanId must be a nonzero u64 bigint.");
  }
}
function encodeIpcBuddyPayload(block: C2IpcShmBlock): Uint8Array {
  requireIpcLoanId(block.loanId);
  return concatUint8Arrays([
    writeU16LE(block.segmentIndex), writeU32LE(block.generation), writeU32LE(block.offset), writeU32LE(block.byteLength),
    new Uint8Array([block.dedicated ? C2_IPC_BUDDY_FLAG_DEDICATED : 0]), writeU64LEBigInt(block.loanId, "loan_id"),
  ]);
}

/** Serializes all post-handshake writes, including immediate response releases. */
async function writeIpcFrame(open: C2IpcOpenConnection, frame?: C2ByteArray, request?: C2IpcPreparedRequest, control = false): Promise<void> {
  const write = open.writeChain.then(async () => {
    const memory = open.connection.memory;
    let batch: C2IpcMemoryWrite | undefined;
    let started = false;
    try {
      batch = memory?.beginWrite(frame === undefined ? 2 : control ? 1 : 0, request);
      if (frame === undefined && batch === undefined) return;
      const bytes = batch === undefined ? frame! : concatUint8Arrays([batch.prefix, frame ?? new Uint8Array()]);
      batch?.start();
      started = true;
      try { await open.connection.write(bytes); }
      catch (error) { batch?.complete(false); throw error; }
      batch?.complete(true);
    } finally {
      if (!started) {
        batch?.discard();
        if (batch === undefined && request !== undefined) memory?.discardRequest(request);
      }
    }
  });
  open.writeChain = write.then(() => undefined, () => undefined);
  return await write;
}

async function readIpcResponseFrame(open: C2IpcOpenConnection): Promise<C2IpcFrame> {
  let finish!: () => void;
  open.reading = new Promise<void>(resolve => { finish = resolve; });
  try {
    for (;;) {
      const memory = open.connection.memory;
      if (open.closing && memory?.tryClose()) {
        await open.connection.close();
        throw new C2IpcTransportError("C-Two IPC connection is closed.");
      }
      const frame = await readIpcFrame(open.connection);
      if (frame.payload[0] === C2_IPC_RELEASE && (frame.flags & C2_IPC_FLAG_CTRL) !== 0) {
        if (memory === undefined) throw new C2IpcTransportError("Release on a connection without native memory.");
        memory.receiveRelease(encodeIpcFrame(frame.requestId, frame.flags, frame.payload));
        continue;
      }
      if (!open.closing) return frame;
      if ((frame.flags & C2_IPC_FLAG_BUDDY) !== 0) {
        if (memory === undefined || (open.handshake.capabilityFlags & C2_IPC_CAP_SHM_RESPONSE) === 0) throw new C2IpcTransportError("Unacknowledged SHM while closing.");
        const response = memory.admitResponse(decodeIpcBuddyPayload(frame.payload));
        memory.discardResponse(response);
        await writeIpcFrame(open);
      }
    }
  } finally { open.reading = undefined; finish(); }
}

function encodeIpcRequestChunkHeader(chunkIndex: number, totalChunks: number): Uint8Array {
  if (!Number.isSafeInteger(chunkIndex) || chunkIndex < 0 || chunkIndex > 0xffff) {
    throw new C2IpcTransportError(`C-Two IPC request chunk index ${chunkIndex} is outside the u16 chunk header range.`);
  }
  if (!Number.isSafeInteger(totalChunks) || totalChunks <= 0 || totalChunks > 0xffff) {
    throw new C2IpcTransportError(`C-Two IPC request chunk count ${totalChunks} is outside the u16 chunk header range.`);
  }
  const header = concatUint8Arrays([writeU16LE(chunkIndex), writeU16LE(totalChunks)]);
  if (header.byteLength !== C2_IPC_REQUEST_CHUNK_HEADER_BYTES) {
    throw new C2IpcTransportError("C-Two IPC request chunk header encoder produced an invalid length.");
  }
  return header;
}

function encodeIpcFrame(requestId: bigint, flags: number, payload: Uint8Array): Uint8Array {
  if (payload.byteLength > 0xffffffff - C2_IPC_FRAME_BODY_HEADER_BYTES) {
    throw new C2IpcTransportError(`C-Two IPC frame payload ${payload.byteLength} exceeds u32 frame length.`);
  }
  const frame = new Uint8Array(C2_IPC_FRAME_HEADER_BYTES + payload.byteLength);
  const view = new DataView(frame.buffer, frame.byteOffset, frame.byteLength);
  view.setUint32(0, C2_IPC_FRAME_BODY_HEADER_BYTES + payload.byteLength, true);
  view.setBigUint64(4, requestId, true);
  view.setUint32(12, flags, true);
  frame.set(payload, C2_IPC_FRAME_HEADER_BYTES);
  return frame;
}

async function readIpcFrame(connection: C2IpcConnection): Promise<C2IpcFrame> {
  const header = requireUint8Array(await connection.readExactly(C2_IPC_FRAME_HEADER_BYTES), "C-Two IPC frame header");
  if (header.byteLength !== C2_IPC_FRAME_HEADER_BYTES) {
    throw new C2IpcTransportError(`C-Two IPC frame header read returned ${header.byteLength} bytes.`);
  }
  const view = new DataView(header.buffer, header.byteOffset, header.byteLength);
  const totalLen = view.getUint32(0, true);
  if (totalLen < C2_IPC_FRAME_BODY_HEADER_BYTES) {
    throw new C2IpcTransportError(`C-Two IPC frame total_len ${totalLen} is smaller than the fixed body header.`);
  }
  const payloadLen = totalLen - C2_IPC_FRAME_BODY_HEADER_BYTES;
  if (payloadLen > C2_MAX_RESPONSE_PAYLOAD_BYTES) {
    throw new C2IpcTransportError(`C-Two IPC frame payload length ${payloadLen} must be no greater than ${C2_MAX_RESPONSE_PAYLOAD_BYTES}.`);
  }
  const payload = payloadLen === 0
    ? new Uint8Array()
    : requireUint8Array(await connection.readExactly(payloadLen), "C-Two IPC frame payload");
  if (payload.byteLength !== payloadLen) {
    throw new C2IpcTransportError(`C-Two IPC frame payload read returned ${payload.byteLength} bytes, expected ${payloadLen}.`);
  }
  return {
    requestId: view.getBigUint64(4, true),
    flags: view.getUint32(12, true),
    payload,
  };
}

function decodeServerIpcHandshake(payload: Uint8Array): C2IpcHandshake {
  let offset = 0;
  const readU8 = (field: string): number => {
    if (offset + 1 > payload.byteLength) {
      throw new C2IpcTransportError(`C-Two IPC handshake truncated at ${field}.`);
    }
    return payload[offset++];
  };
  const readU16 = (field: string): number => {
    if (offset + 2 > payload.byteLength) {
      throw new C2IpcTransportError(`C-Two IPC handshake truncated at ${field}.`);
    }
    const value = readU16LE(payload, offset);
    offset += 2;
    return value;
  };
  const readU64 = (field: string): number => {
    if (offset + 8 > payload.byteLength) {
      throw new C2IpcTransportError(`C-Two IPC handshake truncated at ${field}.`);
    }
    const value = readU64LENumber(payload, offset, field);
    offset += 8;
    return value;
  };
  const readText = (field: string): string => {
    const length = readU8(`${field} length`);
    if (offset + length > payload.byteLength) {
      throw new C2IpcTransportError(`C-Two IPC handshake truncated at ${field}.`);
    }
    const value = utf8Decode(payload.slice(offset, offset + length), field);
    offset += length;
    return value;
  };

  const shmPrefix = readText("prefix");
  const capabilityFlags = readU16("capability flags");
  if ((capabilityFlags & ~(C2_IPC_CAP_SHM_REQUEST | C2_IPC_CAP_SHM_RESPONSE)) !== 0) throw new C2IpcTransportError("Unknown IPC capability bits.");
  const serverId = readText("server_id");
  const serverInstanceId = readText("server_instance_id");
  if (serverId.length === 0 || serverInstanceId.length === 0) {
    throw new C2IpcTransportError("C-Two IPC handshake is missing server identity.");
  }
  const routeCount = readU16("route count");
  for (let routeIndex = 0; routeIndex < routeCount; routeIndex += 1) {
    const name = readText(`route ${routeIndex} name`);
    const routeUid = readText(`route ${routeIndex} route_uid`);
    const crmNs = readText(`route ${routeIndex} crm_ns`);
    const crmName = readText(`route ${routeIndex} crm_name`);
    const crmVer = readText(`route ${routeIndex} crm_ver`);
    const abiHash = readText(`route ${routeIndex} abi_hash`);
    const signatureHash = readText(`route ${routeIndex} signature_hash`);
    const maxPayloadSize = readU64(`route ${routeIndex} max_payload_size`);
    const methodCount = readU16(`route ${routeIndex} method count`);
    requireRouteNamePathValue(name);
    requireIpcHandshakeText(routeUid, `route ${routeIndex} route_uid`);
    if (routeUid.length === 0) {
      throw new C2IpcTransportError(`C-Two IPC route ${routeIndex} route_uid must not be empty.`);
    }
    requireRouteContractTextField(crmNs, "namespace");
    requireRouteContractTextField(crmName, "name");
    requireRouteContractTextField(crmVer, "version");
    if (!isLowerHexHash(abiHash) || !isLowerHexHash(signatureHash)) {
      throw new C2IpcTransportError(`C-Two IPC route ${routeIndex} is missing contract fingerprints.`);
    }
    if (maxPayloadSize <= 0) {
      throw new C2IpcTransportError(`C-Two IPC route ${routeIndex} max_payload_size must be positive.`);
    }
    for (let methodIndex = 0; methodIndex < methodCount; methodIndex += 1) {
      const methodName = readText(`route ${routeIndex} method ${methodIndex} name`);
      readU16(`route ${routeIndex} method ${methodIndex} index`);
      requireMethodPathValue(methodName);
    }
  }
  if (offset !== payload.byteLength) {
    throw new C2IpcTransportError("C-Two IPC handshake contains trailing bytes.");
  }
  return {
    shmPrefix,
    capabilityFlags,
    serverIdentity: { serverId, serverInstanceId },
  };
}

interface C2IpcCatalogRecord {
  readonly route: C2IpcRouteInfo;
  readonly serverIdentity: C2ServerIdentity;
  readonly state: string;
}

type C2IpcCatalogReply =
  | { readonly kind: "nack"; readonly code: number; readonly message: string }
  | { readonly kind: "lookup"; readonly status: "ready" | "stale" | "contract_mismatch"; readonly routeName: string; readonly current: C2IpcCatalogRecord }
  | { readonly kind: "lookup"; readonly status: "not_found" | "removed" | "closed"; readonly routeName: string };

// The canonical codec is a one-byte MsgType followed by serde JSON, carried
// in an inline FLAG_CTRL response (c2-wire::route_catalog_control). Validate
// untrusted records before they can become client bindings. JSON u64 values
// outside JavaScript's exact integer range are rejected instead of rounded.
function ipcCatalogObject(value: unknown): Record<string, unknown> {
  if (typeof value !== "object" || value === null || Array.isArray(value)) {
    throw new C2IpcTransportError("C-Two IPC route catalog expected a JSON object.");
  }
  return value as Record<string, unknown>;
}

function ipcCatalogText(value: unknown, field: string): string {
  if (typeof value !== "string" || value.length === 0) {
    throw new C2IpcTransportError(`C-Two IPC route catalog missing ${field}.`);
  }
  return value;
}

function ipcCatalogInteger(value: unknown, field: string, minimum = 0): number {
  if (typeof value !== "number" || !Number.isSafeInteger(value) || value < minimum) {
    throw new C2IpcTransportError(`C-Two IPC route catalog invalid integer ${field}.`);
  }
  return value;
}

function ipcCatalogUid(value: unknown): string {
  const uid = ipcCatalogText(value, "route_uid");
  if (utf8ByteLength(uid) > C2_MAX_WIRE_TEXT_BYTES || /[\x00-\x20/\\]/.test(uid)) {
    throw new C2IpcTransportError("C-Two IPC route catalog invalid route_uid.");
  }
  return uid;
}

function ipcCatalogFields(item: Record<string, unknown>, fields: readonly string[]): void {
  if (Object.keys(item).some(key => !fields.includes(key))) throw new C2IpcTransportError("C-Two IPC route catalog unknown fields.");
}

function ipcCatalogReason(value: unknown): void {
  if (typeof value !== "string" || ![
    "register_committed", "explicit_unregister", "shutdown",
  ].includes(value)) {
    throw new C2IpcTransportError("C-Two IPC route catalog invalid state reason.");
  }
}

function decodeIpcCatalogRecord(value: unknown): C2IpcCatalogRecord {
  const item = ipcCatalogObject(value);
  const name = ipcCatalogText(item.route_name, "route_name");
  requireRouteNamePathValue(name);
  const routeUid = ipcCatalogUid(item.route_uid);
  const contract = ipcCatalogObject(item.contract);
  ipcCatalogFields(contract, ["route_name", "crm_ns", "crm_name", "crm_ver", "abi_hash", "signature_hash"]);
  if (contract.route_name !== name) {
    throw new C2IpcTransportError("C-Two IPC route catalog contract route_name mismatch.");
  }
  const identity: C2RouteContractIdentity = {
    schema: "c-two.contract.v2", descriptorSha256: "",
    namespace: ipcCatalogText(contract.crm_ns, "crm_ns"),
    name: ipcCatalogText(contract.crm_name, "crm_name"),
    version: ipcCatalogText(contract.crm_ver, "crm_ver"),
    abiHash: ipcCatalogText(contract.abi_hash, "abi_hash"),
    signatureHash: ipcCatalogText(contract.signature_hash, "signature_hash"),
  };
  requireRouteContractIdentity(identity);
  const serverIdentity = {
    serverId: ipcCatalogText(item.owner_server_id, "owner_server_id"),
    serverInstanceId: ipcCatalogText(item.owner_server_instance_id, "owner_server_instance_id"),
  };
  validateIpcRegionId(serverIdentity.serverId);
  validateIpcRegionId(serverIdentity.serverInstanceId);
  ipcCatalogFields(item, ["route_name", "route_uid", "owner_server_id", "owner_server_instance_id", "contract", "methods", "max_payload_size", "state", "state_reason"]);
  ipcCatalogReason(item.state_reason);
  const state = ipcCatalogText(item.state, "state");
  if (!["ready", "closed"].includes(state)) {
    throw new C2IpcTransportError("C-Two IPC route catalog invalid state.");
  }
  if (!Array.isArray(item.methods) || item.methods.length > 256) {
    throw new C2IpcTransportError("C-Two IPC route catalog invalid methods.");
  }
  const names = new Set<string>();
  const indexes = new Set<number>();
  const methods = item.methods.map((value): C2IpcMethodInfo => {
    const method = ipcCatalogObject(value);
    ipcCatalogFields(method, ["name", "index"]);
    const name = ipcCatalogText(method.name, "method name");
    requireMethodPathValue(name);
    const index = ipcCatalogInteger(method.index, "method index");
    if (index > 0xffff || names.has(name) || indexes.has(index)) {
      throw new C2IpcTransportError("C-Two IPC route catalog invalid or duplicate method.");
    }
    names.add(name);
    indexes.add(index);
    return { name, index };
  });
  return {
    route: {
      name, routeUid,
      crmNs: identity.namespace, crmName: identity.name, crmVer: identity.version,
      abiHash: identity.abiHash, signatureHash: identity.signatureHash,
      maxPayloadSize: ipcCatalogInteger(item.max_payload_size, "max_payload_size", 1), methods,
    },
    serverIdentity,
    state,
  };
}

function decodeIpcCatalogReply(payload: Uint8Array, responseTag: number): C2IpcCatalogReply {
  if (payload.byteLength < 2 || (payload[0] !== responseTag && payload[0] !== C2_IPC_ROUTE_NACK)) {
    throw new C2IpcTransportError("C-Two IPC route catalog unexpected response tag.");
  }
  const item = ipcCatalogObject(JSON.parse(utf8Decode(payload.subarray(1), "route catalog")));
  if (payload[0] === C2_IPC_ROUTE_NACK) {
    ipcCatalogFields(item, ["error"]);
    const error = ipcCatalogObject(item.error);
    const code = ipcCatalogInteger(error.code, "error code");
    ipcCatalogText(error.name, "error name");
    if (error.version !== 1 || typeof error.message !== "string") {
      throw new C2IpcTransportError("C-Two IPC route catalog invalid NACK error envelope.");
    }
    const details = ipcCatalogObject(error.details);
    if (Object.values(details).some((value) => typeof value !== "string")) {
      throw new C2IpcTransportError("C-Two IPC route catalog invalid NACK error details.");
    }
    return { kind: "nack", code, message: error.message };
  }
  const status = item.status;
  if (status === "ready" || status === "stale" || status === "contract_mismatch") {
    ipcCatalogFields(item, ["status", "current"]);
    const current = decodeIpcCatalogRecord(item.current);
    return { kind: "lookup", status, routeName: current.route.name, current };
  }
  if (status === "not_found" || status === "removed" || status === "closed") {
    ipcCatalogFields(item, status === "not_found" ? ["status", "route_name"] : status === "removed" ? ["status", "route_name", "route_uid"] : ["status", "route_name", "route_uid", "reason"]);
    const routeName = ipcCatalogText(item.route_name, "route_name");
    requireRouteNamePathValue(routeName);
    if (status === "closed") {
      ipcCatalogUid(item.route_uid);
      ipcCatalogReason(item.reason);
    } else if (status === "removed" && item.route_uid !== null && item.route_uid !== undefined) {
      ipcCatalogUid(item.route_uid);
    }
    return { kind: "lookup", status, routeName };
  }
  throw new C2IpcTransportError("C-Two IPC route catalog invalid lookup status.");
}

function requireMatchingIpcRoute(route: C2IpcRouteInfo, routeName: string, contract: C2RouteContractIdentity): void {
  if (
    route.name !== routeName ||
    route.crmNs !== contract.namespace ||
    route.crmName !== contract.name ||
    route.crmVer !== contract.version ||
    route.abiHash !== contract.abiHash ||
    route.signatureHash !== contract.signatureHash
  ) {
    throw new C2IpcTransportError(`C-Two IPC route ${routeName} does not match the expected CRM contract.`);
  }
}

function findIpcMethod(route: C2IpcRouteInfo, method: string): C2IpcMethodInfo {
  const methodInfo = route.methods.find((candidate) => candidate.name === method);
  if (methodInfo === undefined) {
    throw new C2IpcTransportError(`C-Two IPC route ${route.name} does not advertise method ${method}.`);
  }
  return methodInfo;
}

function decodeIpcBuddyPayload(payload: Uint8Array): C2IpcShmBlock {
  if (payload.byteLength < C2_IPC_BUDDY_PAYLOAD_BYTES) {
    throw new C2IpcTransportError("C-Two IPC buddy payload is truncated.");
  }
  const flags = payload[14];
  if ((flags & ~C2_IPC_BUDDY_FLAG_DEDICATED) !== 0) {
    throw new C2IpcTransportError(`C-Two IPC buddy payload flags ${flags} are invalid.`);
  }
  const byteLength = readU32LE(payload, 10);
  if (byteLength <= 0) {
    throw new C2IpcTransportError("C-Two IPC SHM response data_size must be positive.");
  }
  if (byteLength > C2_MAX_RESPONSE_PAYLOAD_BYTES) {
    throw new C2IpcTransportError(`C-Two IPC SHM response data_size ${byteLength} must be no greater than ${C2_MAX_RESPONSE_PAYLOAD_BYTES}.`);
  }
  const generation = readU32LE(payload, 2);
  const dedicated = (flags & C2_IPC_BUDDY_FLAG_DEDICATED) !== 0;
  if (generation === 0 || (dedicated && readU32LE(payload, 6) !== 0)) {
    throw new C2IpcTransportError("C-Two IPC SHM generation must be positive and dedicated offset must be 0.");
  }
  const loanId = readU64LEBigInt(payload, 15);
  requireIpcLoanId(loanId);
  return {
    segmentIndex: readU16LE(payload, 0),
    generation,
    offset: readU32LE(payload, 6),
    byteLength,
    loanId,
    dedicated,
  };
}

async function readIpcShmSuccessPayload<Payload extends C2ResponsePayload>(
  response: unknown,
  byteLength: number,
  allocator: C2ResponsePayloadAllocator<Payload> | undefined,
  memory: C2IpcMemoryContext,
  flush: () => Promise<void>,
): Promise<Payload> {
  let consumed = false;
  let allocated: C2AllocatedResponsePayload<Payload> | undefined;
  try {
    if (allocator !== undefined) allocated = requireAllocatedResponsePayload(allocator(byteLength), byteLength);
    const destination = allocated?.view ?? new Uint8Array(byteLength);
    consumed = true;
    memory.consumeResponse(response, destination);
    return allocated?.payload ?? destination as Payload;
  } catch (error) {
    if (allocated !== undefined) tryReleaseAllocatedResponsePayload(allocated);
    throw new C2IpcTransportError(`C-Two IPC response copy failed: ${String(error)}`);
  } finally {
    if (!consumed) memory.discardResponse(response);
    // The flush callback deactivates and discards a failed stream. A copied
    // result and a copy failure each retain their own terminal outcome.
    try { await flush(); } catch { /* peer end settles releases not delivered */ }
  }
}

async function readIpcChunkedSuccessPayload(open: C2IpcOpenConnection, firstFrame: C2IpcFrame, requestId: bigint): Promise<Uint8Array> {
  const firstChunk = validateIpcChunkedReplyFrame(firstFrame, requestId);
  if (firstChunk.chunkIndex !== 0) {
    throw new C2IpcTransportError(`C-Two IPC first chunked response frame has chunk_idx ${firstChunk.chunkIndex}, expected 0.`);
  }
  if (firstChunk.totalSize <= 0) {
    throw new C2IpcTransportError("C-Two IPC chunked response total_size must be positive.");
  }
  if (firstChunk.totalSize > C2_MAX_RESPONSE_PAYLOAD_BYTES) {
    throw new C2IpcTransportError(`C-Two IPC chunked response total_size ${firstChunk.totalSize} must be no greater than ${C2_MAX_RESPONSE_PAYLOAD_BYTES}.`);
  }
  if (firstChunk.totalChunks <= 0) {
    throw new C2IpcTransportError("C-Two IPC chunked response total_chunks must be positive.");
  }
  if (firstChunk.totalChunks > firstChunk.totalSize) {
    throw new C2IpcTransportError("C-Two IPC chunked response total_chunks cannot exceed total_size.");
  }
  const chunkSize = firstChunk.totalChunks === 1 ? firstChunk.totalSize : firstChunk.data.byteLength;
  if (chunkSize <= 0) {
    throw new C2IpcTransportError("C-Two IPC chunked response chunk_size must be positive.");
  }
  let expectedFinalChunkSize = firstChunk.totalSize;
  if (firstChunk.totalChunks > 1) {
    const maxNonFinalChunkSize = Math.floor((firstChunk.totalSize - 1) / (firstChunk.totalChunks - 1));
    if (chunkSize > maxNonFinalChunkSize) {
      throw new C2IpcTransportError("C-Two IPC chunked response chunk layout cannot fit total_size.");
    }
    expectedFinalChunkSize = firstChunk.totalSize - (firstChunk.totalChunks - 1) * chunkSize;
  }
  const assembled = new Uint8Array(firstChunk.totalSize);
  const received = new Set<number>();
  let writtenEnd = 0;
  const feed = (frame: C2IpcFrame): void => {
    const chunk = validateIpcChunkedReplyFrame(frame, requestId);
    if (chunk.totalSize !== firstChunk.totalSize || chunk.totalChunks !== firstChunk.totalChunks) {
      throw new C2IpcTransportError("C-Two IPC chunked response metadata changed between chunks.");
    }
    if (chunk.chunkIndex >= firstChunk.totalChunks) {
      throw new C2IpcTransportError(`C-Two IPC chunked response chunk_idx ${chunk.chunkIndex} is outside total_chunks ${firstChunk.totalChunks}.`);
    }
    if (received.has(chunk.chunkIndex)) {
      throw new C2IpcTransportError(`C-Two IPC chunked response duplicate chunk_idx ${chunk.chunkIndex}.`);
    }
    const isLastChunk = chunk.chunkIndex === firstChunk.totalChunks - 1;
    const frameSaysLast = (frame.flags & C2_IPC_FLAG_CHUNK_LAST) !== 0;
    if (isLastChunk !== frameSaysLast) {
      throw new C2IpcTransportError("C-Two IPC chunked response CHUNK_LAST flag does not match chunk index.");
    }
    if (!isLastChunk && chunk.data.byteLength !== chunkSize) {
      throw new C2IpcTransportError("C-Two IPC chunked response non-final chunk length does not match chunk_size.");
    }
    if (isLastChunk && chunk.data.byteLength === 0) {
      throw new C2IpcTransportError("C-Two IPC chunked response final chunk must not be empty.");
    }
    if (isLastChunk && chunk.data.byteLength > chunkSize) {
      throw new C2IpcTransportError("C-Two IPC chunked response final chunk exceeds chunk_size.");
    }
    if (isLastChunk && chunk.data.byteLength !== expectedFinalChunkSize) {
      throw new C2IpcTransportError("C-Two IPC chunked response final chunk did not complete total_size.");
    }
    const offset = chunk.chunkIndex * chunkSize;
    const end = offset + chunk.data.byteLength;
    if (end > firstChunk.totalSize) {
      throw new C2IpcTransportError("C-Two IPC chunked response chunk data exceeds total_size.");
    }
    assembled.set(chunk.data, offset);
    if (end > writtenEnd) {
      writtenEnd = end;
    }
    received.add(chunk.chunkIndex);
  };

  feed(firstFrame);
  while (received.size < firstChunk.totalChunks) {
    feed(await readIpcResponseFrame(open));
  }
  if (writtenEnd !== firstChunk.totalSize) {
    throw new C2IpcTransportError("C-Two IPC chunked response final chunk did not complete total_size.");
  }
  return assembled;
}

function validateIpcChunkedReplyFrame(frame: C2IpcFrame, requestId: bigint): C2IpcReplyChunk {
  if (frame.requestId !== requestId) {
    throw new C2IpcTransportError(`C-Two IPC chunked response request id ${frame.requestId} did not match request id ${requestId}.`);
  }
  if ((frame.flags & C2_IPC_FLAG_RESPONSE) === 0 || (frame.flags & C2_IPC_FLAG_REPLY_V2) === 0 || (frame.flags & C2_IPC_FLAG_CHUNKED) === 0) {
    throw new C2IpcTransportError("C-Two IPC chunked response frame is not a v2 chunked reply.");
  }
  if ((frame.flags & C2_IPC_FLAG_BUDDY) !== 0) {
    throw new C2IpcTransportError("C-Two IPC chunked SHM responses are not supported by this generated transport yet.");
  }
  return decodeIpcReplyChunkMeta(frame.payload);
}

function decodeIpcReplyChunkMeta(payload: Uint8Array): C2IpcReplyChunk {
  if (payload.byteLength < C2_IPC_REPLY_CHUNK_META_BYTES) {
    throw new C2IpcTransportError("C-Two IPC chunked response metadata is truncated.");
  }
  const totalSize = readU64LENumber(payload, 0, "chunked response total_size");
  const totalChunks = readU32LE(payload, 8);
  const chunkIndex = readU32LE(payload, 12);
  return {
    totalSize,
    totalChunks,
    chunkIndex,
    data: payload.subarray(C2_IPC_REPLY_CHUNK_META_BYTES),
  };
}

function decodeIpcReplyControl(payload: Uint8Array): C2IpcReplyControl {
  if (payload.byteLength < 1) {
    throw new C2IpcTransportError("C-Two IPC reply control is empty.");
  }
  const status = payload[0];
  if (status === C2_IPC_REPLY_STATUS_SUCCESS) {
    return { kind: "success", payload: payload.subarray(1) };
  }
  if (status === C2_IPC_REPLY_STATUS_ERROR) {
    const errorPayload = readLengthPrefixedReplyBytes(payload, "error payload");
    return { kind: "error", payload: errorPayload };
  }
  if (status === C2_IPC_REPLY_STATUS_ROUTE_NOT_FOUND) {
    const routeName = utf8Decode(readLengthPrefixedReplyBytes(payload, "route name"), "route name");
    requireRouteNamePathValue(routeName);
    return { kind: "route-not-found", routeName };
  }
  throw new C2IpcTransportError(`C-Two IPC reply status ${status} is invalid.`);
}

function readLengthPrefixedReplyBytes(payload: Uint8Array, field: string): Uint8Array {
  if (payload.byteLength < 5) {
    throw new C2IpcTransportError(`C-Two IPC reply ${field} is truncated.`);
  }
  const length = readU32LE(payload, 1);
  const expected = 5 + length;
  if (payload.byteLength !== expected) {
    throw new C2IpcTransportError(`C-Two IPC reply ${field} length ${length} does not match payload length ${payload.byteLength}.`);
  }
  return payload.slice(5);
}

function concatUint8Arrays(chunks: readonly C2ByteArray[]): C2ByteArray {
  let total = 0;
  for (const chunk of chunks) {
    total += chunk.byteLength;
  }
  const out = new Uint8Array(total);
  let offset = 0;
  for (const chunk of chunks) {
    out.set(chunk, offset);
    offset += chunk.byteLength;
  }
  return out;
}

function writeU16LE(value: number): Uint8Array {
  const out = new Uint8Array(2);
  new DataView(out.buffer).setUint16(0, value, true);
  return out;
}

function readU16LE(value: Uint8Array, offset: number): number {
  return new DataView(value.buffer, value.byteOffset, value.byteLength).getUint16(offset, true);
}

function writeU32LE(value: number): Uint8Array {
  const out = new Uint8Array(4);
  new DataView(out.buffer).setUint32(0, value, true);
  return out;
}

function writeU64LEBigInt(value: bigint, field: string): Uint8Array {
  if (typeof value !== "bigint" || value < 0n || value > 0xffffffffffffffffn) {
    throw new C2IpcTransportError(`C-Two IPC ${field} must be a u64 bigint.`);
  }
  const out = new Uint8Array(8);
  new DataView(out.buffer).setBigUint64(0, value, true);
  return out;
}

function readU64LEBigInt(value: Uint8Array, offset: number): bigint {
  return new DataView(value.buffer, value.byteOffset, value.byteLength).getBigUint64(offset, true);
}

function readU32LE(value: Uint8Array, offset: number): number {
  return new DataView(value.buffer, value.byteOffset, value.byteLength).getUint32(offset, true);
}

function readU64LENumber(value: Uint8Array, offset: number, field: string): number {
  const raw = new DataView(value.buffer, value.byteOffset, value.byteLength).getBigUint64(offset, true);
  if (raw > BigInt(Number.MAX_SAFE_INTEGER)) {
    throw new C2IpcTransportError(`C-Two IPC ${field} ${raw} exceeds Number.MAX_SAFE_INTEGER.`);
  }
  return Number(raw);
}

function utf8Encode(value: string): Uint8Array {
  return C2_TEXT_ENCODER.encode(value);
}

function utf8Decode(value: Uint8Array, field: string): string {
  try {
    return C2_TEXT_DECODER.decode(value);
  } catch (error) {
    throw new C2IpcTransportError(`C-Two IPC ${field} is not valid UTF-8: ${String(error)}`);
  }
}

function normalizeResponsePayloadAllocator<Payload extends C2ResponsePayload>(value: C2ResponsePayloadAllocator<Payload> | undefined, label: string): C2ResponsePayloadAllocator<Payload> | undefined {
  if (value === undefined) {
    return undefined;
  }
  if (typeof value !== "function") {
    throw new Error(`${label} must be a function.`);
  }
  return value;
}

function normalizeResponsePayloadUnknownLengthStrategy(value: C2ResponsePayloadUnknownLengthStrategy | undefined, label: string): C2ResponsePayloadUnknownLengthStrategy {
  if (value === undefined) {
    return "reject";
  }
  if (value !== "reject" && value !== "buffer") {
    throw new Error(`${label} must be "reject" or "buffer".`);
  }
  return value;
}

function normalizeUnknownLengthBufferMaxBytes(value: number | undefined, label: string): number {
  const defaultMaxBytes = 64 * 1024 * 1024;
  if (value === undefined) {
    return defaultMaxBytes;
  }
  if (!Number.isFinite(value) || !Number.isSafeInteger(value) || value < 0 || value > C2_MAX_RESPONSE_PAYLOAD_BYTES) {
    throw new Error(`${label} must be a non-negative safe integer no greater than ${C2_MAX_RESPONSE_PAYLOAD_BYTES}.`);
  }
  return value;
}

function isProviderOwnedResponsePayload(value: unknown): value is C2ProviderOwnedResponsePayload {
  if (typeof value !== "object" || value === null) {
    return false;
  }
  const byteLength = (value as { byteLength?: unknown }).byteLength;
  return typeof byteLength === "number" && Number.isSafeInteger(byteLength) && byteLength >= 0;
}

function retainableResponsePayload(value: C2ResponsePayload): C2ResponsePayload {
  return value;
}

function copyResponsePayloadToAllocator<Payload extends C2ResponsePayload>(
  payload: Uint8Array,
  allocator: C2ResponsePayloadAllocator<Payload> | undefined,
): Payload {
  if (allocator === undefined) {
    return payload as Payload;
  }
  const allocated = requireAllocatedResponsePayload<Payload>(allocator(payload.byteLength), payload.byteLength);
  try {
    allocated.view.set(payload);
    return allocated.payload;
  } catch (error) {
    tryReleaseAllocatedResponsePayload(allocated);
    throw error;
  }
}

async function readHttpSuccessResponsePayload<Payload extends C2ResponsePayload>(
  response: C2FetchResponse,
  allocator: C2ResponsePayloadAllocator<Payload> | undefined,
  unknownLengthStrategy: C2ResponsePayloadUnknownLengthStrategy,
  unknownLengthMaxBytes: number,
): Promise<Payload> {
  if (allocator !== undefined) {
    const contentLength = responseContentLength(response);
    if (contentLength === undefined) {
      if (unknownLengthStrategy !== "buffer") {
        throw new Error("C-Two HTTP relay response Content-Length is required when responsePayloadAllocator is configured.");
      }
      if (response.body === undefined || response.body === null) {
        throw new Error("C-Two HTTP relay response body stream is required when responsePayloadAllocator is configured.");
      }
      return await readUnknownLengthAllocatedResponsePayload(response.body, allocator, unknownLengthMaxBytes);
    }
    if (contentLength === 0) {
      return requireAllocatedResponsePayload<Payload>(allocator(0), 0).payload;
    }
    if (response.body === undefined || response.body === null) {
      throw new Error("C-Two HTTP relay response body stream is required when responsePayloadAllocator is configured.");
    }
    return await readAllocatedResponsePayload(response.body, contentLength, allocator);
  }
  return await readHttpArrayBufferResponsePayload(response) as Payload;
}

async function readHttpArrayBufferResponsePayload(response: C2FetchResponse): Promise<Uint8Array> {
  const contentLength = responseContentLength(response);
  const payload = new Uint8Array(await response.arrayBuffer());
  if (contentLength !== undefined && payload.byteLength !== contentLength) {
    throw new Error("C-Two HTTP relay response body byte length does not match Content-Length.");
  }
  return payload;
}

async function readHttpTextResponseBody(response: C2FetchResponse): Promise<string> {
  responseContentLength(response);
  return await response.text();
}

function responseContentLength(response: C2FetchResponse): number | undefined {
  const rawValue = response.headers?.get("content-length");
  if (rawValue === undefined || rawValue === null) {
    return undefined;
  }
  if (!/^[0-9]+$/.test(rawValue)) {
    throw new Error("C-Two HTTP relay response Content-Length must be a non-negative decimal integer.");
  }
  const value = Number(rawValue);
  if (!Number.isSafeInteger(value)) {
    throw new Error("C-Two HTTP relay response Content-Length must be a safe integer.");
  }
  if (value > C2_MAX_RESPONSE_PAYLOAD_BYTES) {
    throw new Error(`C-Two HTTP relay response Content-Length must be no greater than ${C2_MAX_RESPONSE_PAYLOAD_BYTES}.`);
  }
  return value;
}

async function readAllocatedResponsePayload<Payload extends C2ResponsePayload>(body: C2ReadableStream, byteLength: number, allocator: C2ResponsePayloadAllocator<Payload>): Promise<Payload> {
  const allocated = requireAllocatedResponsePayload<Payload>(allocator(byteLength), byteLength);
  let reader: C2ReadableStreamReader | undefined;
  let offset = 0;
  try {
    reader = body.getReader();
    while (true) {
      const chunk = await reader.read();
      if (chunk.done) {
        break;
      }
      if (!(chunk.value instanceof Uint8Array)) {
        throw new Error("C-Two HTTP relay response stream chunk must be a Uint8Array.");
      }
      if (offset + chunk.value.byteLength > byteLength) {
        throw new Error("C-Two HTTP relay response stream exceeded Content-Length.");
      }
      allocated.view.set(chunk.value, offset);
      offset += chunk.value.byteLength;
    }
    if (offset !== byteLength) {
      throw new Error("C-Two HTTP relay response stream ended before Content-Length bytes were read.");
    }
    return allocated.payload;
  } catch (error) {
    tryReleaseAllocatedResponsePayload(allocated);
    throw error;
  } finally {
    reader?.releaseLock?.();
  }
}

async function readUnknownLengthAllocatedResponsePayload<Payload extends C2ResponsePayload>(
  body: C2ReadableStream,
  allocator: C2ResponsePayloadAllocator<Payload>,
  maxBytes: number,
): Promise<Payload> {
  const chunks: C2ByteArray[] = [];
  let byteLength = 0;
  let reader: C2ReadableStreamReader | undefined;
  try {
    reader = body.getReader();
    while (true) {
      const chunk = await reader.read();
      if (chunk.done) {
        break;
      }
      if (!(chunk.value instanceof Uint8Array)) {
        throw new Error("C-Two HTTP relay unknown-length response stream chunk must be a Uint8Array.");
      }
      const nextByteLength = byteLength + chunk.value.byteLength;
      if (!Number.isSafeInteger(nextByteLength)) {
        throw new Error("C-Two HTTP relay unknown-length response stream byte length must be a safe integer.");
      }
      if (nextByteLength > maxBytes) {
        throw new Error(`C-Two HTTP relay unknown-length response exceeded responsePayloadUnknownLengthMaxBytes (${maxBytes}).`);
      }
      chunks.push(chunk.value.slice());
      byteLength = nextByteLength;
    }
  } finally {
    reader?.releaseLock?.();
  }
  const allocated = requireAllocatedResponsePayload<Payload>(allocator(byteLength), byteLength);
  try {
    let offset = 0;
    for (const chunk of chunks) {
      allocated.view.set(chunk, offset);
      offset += chunk.byteLength;
    }
    return allocated.payload;
  } catch (error) {
    tryReleaseAllocatedResponsePayload(allocated);
    throw error;
  }
}

function requireAllocatedResponsePayload<Payload extends C2ResponsePayload>(value: unknown, byteLength: number): C2AllocatedResponsePayload<Payload> {
  if (typeof value !== "object" || value === null) {
    throw new Error("C-Two responsePayloadAllocator must return an object.");
  }
  const allocated = value as Partial<C2AllocatedResponsePayload<Payload>>;
  try {
    if (!(allocated.view instanceof Uint8Array)) {
      throw new Error("C-Two responsePayloadAllocator result view must be a Uint8Array.");
    }
    if (allocated.view.byteLength !== byteLength) {
      throw new Error("C-Two responsePayloadAllocator result view length must match expected response byte length.");
    }
    if (!isResponsePayload(allocated.payload)) {
      throw new Error("C-Two responsePayloadAllocator result payload must be a response payload.");
    }
    if (allocated.release !== undefined && typeof allocated.release !== "function") {
      throw new Error("C-Two responsePayloadAllocator result release must be a function when provided.");
    }
  } catch (error) {
    tryReleaseInvalidAllocatedResponsePayload(allocated);
    throw error;
  }
  return allocated as C2AllocatedResponsePayload<Payload>;
}

function isResponsePayload(value: unknown): value is C2ResponsePayload {
  return value instanceof Uint8Array || value instanceof ArrayBuffer || isProviderOwnedResponsePayload(value);
}

function releaseAllocatedResponsePayload(value: C2AllocatedResponsePayload<C2ResponsePayload>): void {
  if (value.release !== undefined) {
    value.release();
    return;
  }
  releaseHeldValue(value.payload);
}

function tryReleaseAllocatedResponsePayload(value: C2AllocatedResponsePayload<C2ResponsePayload>): void {
  try {
    releaseAllocatedResponsePayload(value);
  } catch (cleanupError) {
    console.warn("Warning: allocated response payload cleanup failed:", cleanupError);
  }
}

function releaseInvalidAllocatedResponsePayload(value: Partial<C2AllocatedResponsePayload<C2ResponsePayload>>): void {
  if (typeof value.release === "function") {
    value.release();
    return;
  }
  releaseHeldValue(value.payload);
}

function tryReleaseInvalidAllocatedResponsePayload(value: Partial<C2AllocatedResponsePayload<C2ResponsePayload>>): void {
  try {
    releaseInvalidAllocatedResponsePayload(value);
  } catch (cleanupError) {
    console.warn("Warning: invalid allocated response payload cleanup failed:", cleanupError);
  }
}

function createHeldResult<T>(value: T, releaseValue: boolean, buffer?: C2ResponsePayload, releaseBuffer = false): C2HeldResult<T> {
  return new DefaultC2HeldResult(value, releaseValue, buffer, releaseBuffer);
}

class DefaultC2HeldResult<T> implements C2HeldResult<T> {
  private released = false;
  private currentValue: T | undefined;
  private currentBuffer: C2ResponsePayload | undefined;

  constructor(value: T, private readonly releaseValue: boolean, buffer?: C2ResponsePayload, private readonly releaseBuffer = false) {
    this.currentValue = value;
    this.currentBuffer = buffer;
  }

  get value(): T {
    if (this.released) {
      throw new Error("C2HeldResult released: value no longer accessible.");
    }
    return this.currentValue as T;
  }

  get buffer(): C2ResponsePayload {
    if (this.released) {
      throw new Error("C2HeldResult released: buffer no longer accessible.");
    }
    if (this.currentBuffer === undefined) {
      throw new Error("C2HeldResult has no retained buffer.");
    }
    return this.currentBuffer;
  }

  release(): void {
    if (this.released) {
      return;
    }
    const value = this.currentValue;
    const buffer = this.currentBuffer;
    this.released = true;
    this.currentValue = undefined;
    this.currentBuffer = undefined;
    let releaseError: unknown;
    if (this.releaseValue) {
      try {
        releaseHeldValue(value);
      } catch (error) {
        releaseError = error;
      }
    }
    if (this.releaseBuffer && buffer !== value) {
      try {
        releaseHeldValue(buffer);
      } catch (error) {
        if (releaseError === undefined) {
          releaseError = error;
        } else {
          console.warn("Warning: held response cleanup failed:", error);
        }
      }
    }
    if (releaseError !== undefined) {
      throw releaseError;
    }
  }
}

function tryReleaseResponsePayload(value: C2ResponsePayload | undefined): void {
  try {
    releaseHeldValue(value);
  } catch (cleanupError) {
    console.warn("Warning: response payload cleanup failed:", cleanupError);
  }
}

function releaseHeldValue(value: unknown): void {
  if (typeof value !== "object" || value === null) {
    return;
  }
  const releasable = value as { invalidate?: unknown; dispose?: unknown; release?: unknown; close?: unknown };
  let releaseError: unknown;
  if (typeof releasable.invalidate === "function") {
    try {
      releasable.invalidate.call(value);
    } catch (error) {
      releaseError = error;
    }
  }
  try {
    if (typeof releasable.dispose === "function") {
      releasable.dispose.call(value);
    } else if (typeof releasable.release === "function") {
      releasable.release.call(value);
    } else if (typeof releasable.close === "function") {
      releasable.close.call(value);
    }
  } catch (error) {
    if (releaseError === undefined) {
      releaseError = error;
    } else {
      console.warn("Warning: held value cleanup failed:", error);
    }
  }
  if (releaseError !== undefined) {
    throw releaseError;
  }
}

const C2_RESERVED_HTTP_HEADER_NAMES = new Set([
  "content-type",
  "x-c2-expected-crm-ns",
  "x-c2-expected-crm-name",
  "x-c2-expected-crm-ver",
  "x-c2-expected-abi-hash",
  "x-c2-expected-signature-hash",
  "x-c2-route-uid",
]);
const C2_MAX_RESPONSE_PAYLOAD_BYTES = 2_147_483_647;
const C2_MAX_WIRE_TEXT_BYTES = 255;
const C2_HTTP_HEADER_NAME_RE = /^[!#$%&'*+\-.^_`|~0-9A-Za-z]+$/;

function normalizeHttpRelayTransportOptions<T extends C2HttpRelayTransportOptions<C2ResponsePayload>>(value: T, label: string): T {
  if (typeof value !== "object" || value === null || Array.isArray(value)) {
    throw new Error(`${label} must be an object.`);
  }
  return value;
}

function normalizeFetch(value: C2Fetch | undefined, label: string): C2Fetch | undefined {
  if (value === undefined) {
    return undefined;
  }
  if (typeof value !== "function") {
    throw new Error(`${label} must be a function.`);
  }
  return value;
}

function globalFetch(): C2Fetch {
  const candidate = (globalThis as unknown as { fetch?: unknown }).fetch;
  if (typeof candidate !== "function") {
    throw new Error("C-Two HTTP relay transport requires a fetch implementation.");
  }
  return candidate.bind(globalThis) as C2Fetch;
}

function normalizeRelayBaseUrl(value: string, label: string): string {
  if (typeof value !== "string") {
    throw new Error(`${label} must be a string.`);
  }
  if (/\s/.test(value)) {
    throw new Error(`${label} cannot include whitespace.`);
  }
  const normalized = value.replace(/\/+$/, "");
  if (!normalized) {
    throw new Error(`${label} cannot be empty.`);
  }
  let parsed: URL;
  try {
    parsed = new URL(normalized);
  } catch {
    throw new Error(`${label} must be an absolute HTTP(S) URL.`);
  }
  if (parsed.protocol !== "http:" && parsed.protocol !== "https:") {
    throw new Error(`${label} must use HTTP(S).`);
  }
  if (parsed.username || parsed.password) {
    throw new Error(`${label} cannot include credentials.`);
  }
  if (parsed.search || parsed.hash) {
    throw new Error(`${label} cannot include query or fragment components.`);
  }
  return normalized;
}

function normalizeHttpHeaders(headers: Readonly<Record<string, string>> | undefined, label: string): Readonly<Record<string, string>> {
  if (headers === undefined) {
    return Object.create(null) as Record<string, string>;
  }
  if (typeof headers !== "object" || headers === null || Array.isArray(headers)) {
    throw new Error(`${label} must be a record of string HTTP headers.`);
  }
  const normalized = Object.create(null) as Record<string, string>;
  for (const [name, value] of Object.entries(headers as Record<string, unknown>)) {
    if (!C2_HTTP_HEADER_NAME_RE.test(name)) {
      throw new Error(`${label} contains invalid HTTP header name.`);
    }
    if (typeof value !== "string") {
      throw new Error(`${label} ${name} must be a string.`);
    }
    if (/[\r\n\0]/.test(value)) {
      throw new Error(`${label} ${name} contains invalid HTTP header value.`);
    }
    if (C2_RESERVED_HTTP_HEADER_NAMES.has(name.toLowerCase())) {
      throw new Error(`${label} cannot override reserved C-Two HTTP header ${name}.`);
    }
    normalized[name] = value;
  }
  return normalized;
}

function requestHeadersForRouteContract(customHeaders: Readonly<Record<string, string>>, routeContract: C2RouteContractIdentity): Readonly<Record<string, string>> {
  const headers = Object.create(null) as Record<string, string>;
  for (const [name, value] of Object.entries(customHeaders)) {
    headers[name] = value;
  }
  headers["Content-Type"] = "application/octet-stream";
  headers["x-c2-expected-crm-ns"] = routeContract.namespace;
  headers["x-c2-expected-crm-name"] = routeContract.name;
  headers["x-c2-expected-crm-ver"] = routeContract.version;
  headers["x-c2-expected-abi-hash"] = routeContract.abiHash;
  headers["x-c2-expected-signature-hash"] = routeContract.signatureHash;
  return headers;
}

function requestHeadersForResolvedRoute(
  customHeaders: Readonly<Record<string, string>>,
  routeContract: C2RouteContractIdentity,
  route: C2RouteToken,
): Readonly<Record<string, string>> {
  const headers = {
    ...requestHeadersForRouteContract(customHeaders, routeContract),
  } as Record<string, string>;
  headers["x-c2-route-uid"] = route.routeUid;
  return headers;
}

async function probeResolvedHttpRoute(
  fetchImpl: C2Fetch,
  relayUrl: string,
  routeName: string,
  routeContract: C2RouteContractIdentity,
  route: C2RelayRouteInfo,
  headers: Readonly<Record<string, string>>,
  timeoutMs: number,
): Promise<void> {
  await fetchWithTimeout(async (signal) => {
    let response: C2FetchResponse;
    try {
      response = await fetchImpl(
        `${relayUrl}/_probe/${encodePathSegment(routeName)}`,
        {
          method: "GET",
          headers: requestHeadersForResolvedRoute(headers, routeContract, route),
          signal,
        },
      );
    } catch (error) {
      throw new C2HttpTransportError(`C-Two HTTP relay probe fetch failed: ${String(error)}`);
    }
    if (response.status === 200) {
      return;
    }
    let body = "";
    try {
      body = await readHttpTextResponseBody(response);
    } catch (error) {
      throw new C2HttpTransportError(`C-Two HTTP relay probe body read failed: ${String(error)}`);
    }
    throw new C2HttpRelayError(response.status, body);
  }, timeoutMs, "C-Two HTTP relay probe");
}

async function callResolvedHttpRoute<Payload extends C2ResponsePayload>(
  fetchImpl: C2Fetch,
  relayUrl: string,
  routeName: string,
  routeContract: C2RouteContractIdentity,
  route: C2RelayRouteInfo,
  method: string,
  requestPayload: Uint8Array,
  headers: Readonly<Record<string, string>>,
  callTimeoutMs: number,
  responsePayloadAllocator: C2ResponsePayloadAllocator<Payload> | undefined,
  responsePayloadUnknownLengthStrategy: C2ResponsePayloadUnknownLengthStrategy,
  responsePayloadUnknownLengthMaxBytes: number,
): Promise<Payload> {
  return await fetchWithTimeout(async (signal) => {
    let response: C2FetchResponse;
    try {
      response = await fetchImpl(
        `${relayUrl}/${encodePathSegment(routeName)}/${encodePathSegment(method)}`,
        {
          method: "POST",
          headers: requestHeadersForResolvedRoute(headers, routeContract, route),
          body: requestPayload,
          signal,
        },
      );
    } catch (error) {
      throw new C2HttpTransportError(`C-Two HTTP relay call fetch failed: ${String(error)}`);
    }
    if (response.status === 200) {
      try {
        return await readHttpSuccessResponsePayload(
          response,
          responsePayloadAllocator,
          responsePayloadUnknownLengthStrategy,
          responsePayloadUnknownLengthMaxBytes,
        );
      } catch (error) {
        throw new C2HttpTransportError(`C-Two HTTP relay call body read failed: ${String(error)}`);
      }
    }
    if (response.status === 500) {
      let errorPayload: Uint8Array;
      try {
        errorPayload = await readHttpArrayBufferResponsePayload(response);
      } catch (error) {
        throw new C2HttpTransportError(`C-Two HTTP relay call body read failed: ${String(error)}`);
      }
      throw new C2CrmMethodError(errorPayload);
    }
    let body = "";
    try {
      body = await readHttpTextResponseBody(response);
    } catch (error) {
      throw new C2HttpTransportError(`C-Two HTTP relay call body read failed: ${String(error)}`);
    }
    throw new C2HttpRelayError(response.status, body);
  }, callTimeoutMs, "C-Two HTTP relay call");
}

async function callPreparedLocalIpcAndClose<Payload extends C2ResponsePayload>(
  transport: C2IpcEncodedTransport<Payload>,
  routeName: string,
  contract: C2ContractIdentity,
  method: string,
  requestPayload: C2ByteArray,
): Promise<Payload> {
  let response: Payload;
  try {
    response = await transport.call(routeName, contract, method, requestPayload);
  } catch (error) {
    try {
      await transport.close();
    } catch (cleanupError) {
      console.warn("Warning: local IPC transport cleanup failed:", cleanupError);
    }
    throw error;
  }
  try {
    await transport.close();
  } catch (error) {
    tryReleaseResponsePayload(response);
    throw error;
  }
  return response;
}

function observeTransportResponse<Payload extends C2ResponsePayload>(
  response: Payload,
  observe: C2TransportObserver | undefined,
  observation: C2TransportObservation,
): Payload {
  if (observe === undefined) {
    return response;
  }
  try {
    observe(Object.freeze({ ...observation }));
    return response;
  } catch (error) {
    tryReleaseResponsePayload(response);
    throw error;
  }
}

function normalizeTransportObserver(
  value: C2TransportObserver | undefined,
  label: string,
): C2TransportObserver | undefined {
  if (value === undefined) {
    return undefined;
  }
  if (typeof value !== "function") {
    throw new Error(`${label} must be a function.`);
  }
  return value;
}

function normalizeExpectedServerIdentity(
  value: C2ServerIdentity | undefined,
  label: string,
): C2ServerIdentity | undefined {
  if (value === undefined) {
    return undefined;
  }
  if (typeof value !== "object" || value === null) {
    throw new Error(`${label} must be an object.`);
  }
  const serverId = requireNonEmptyText(value.serverId, `${label}.serverId`);
  const serverInstanceId = requireNonEmptyText(
    value.serverInstanceId,
    `${label}.serverInstanceId`,
  );
  return Object.freeze({ serverId, serverInstanceId });
}

function normalizeExpectedRouteToken(
  value: C2RouteToken | undefined,
  label: string,
): C2RouteToken | undefined {
  if (value === undefined) {
    return undefined;
  }
  if (typeof value !== "object" || value === null) {
    throw new Error(`${label} must be an object.`);
  }
  const routeUid = requireNonEmptyText(value.routeUid, `${label}.routeUid`);
  if ("routeRevision" in value) throw new Error(`${label} contains unknown fields.`);
  return Object.freeze({ routeUid });
}

function normalizeIpcObservationPath(
  value: C2IpcTransportOptions<C2ResponsePayload>["observationPath"],
): "DirectIpc" | "RelayAwareLocalIpc" {
  if (value === undefined) {
    return "DirectIpc";
  }
  if (value !== "DirectIpc" && value !== "RelayAwareLocalIpc") {
    throw new Error('C-Two IPC observationPath must be "DirectIpc" or "RelayAwareLocalIpc".');
  }
  return value;
}

function normalizeRelayAwareIpcOptions<Payload extends C2ResponsePayload>(
  value: C2IpcTransportOptions<Payload> | undefined,
): C2IpcTransportOptions<Payload> | undefined {
  if (value === undefined) {
    return undefined;
  }
  if (typeof value !== "object" || value === null || Array.isArray(value)) {
    throw new Error("C-Two relay-aware ipc options must be an object.");
  }
  normalizeIpcConnect(value, "C-Two relay-aware ipc options");
  if (
    value.expectedServerIdentity !== undefined ||
    value.expectedRouteToken !== undefined ||
    value.observationPath !== undefined ||
    value.observe !== undefined
  ) {
    throw new Error(
      "C-Two relay-aware ipc expected route facts and observation are owned by relay resolution.",
    );
  }
  return value;
}

function requireExpectedServerIdentity(
  actual: C2ServerIdentity,
  expected: C2ServerIdentity | undefined,
): void {
  if (
    expected !== undefined &&
    (
      actual.serverId !== expected.serverId ||
      actual.serverInstanceId !== expected.serverInstanceId
    )
  ) {
    throw new C2IpcTransportError(
      `C-Two IPC server identity mismatch: expected ${expected.serverId}/${expected.serverInstanceId}, got ${actual.serverId}/${actual.serverInstanceId}.`,
    );
  }
}

function requireExpectedRouteToken(
  actual: C2IpcRouteInfo,
  expected: C2RouteToken | undefined,
): void {
  if (
    expected !== undefined &&
    (
      actual.routeUid !== expected.routeUid
    )
  ) {
    throw new C2IpcTransportError(
      `C-Two IPC route token mismatch: expected ${expected.routeUid}, got ${actual.routeUid}.`,
    );
  }
}

function requireNonEmptyText(value: unknown, label: string): string {
  if (typeof value !== "string" || value.length === 0) {
    throw new Error(`${label} must be a non-empty string.`);
  }
  return value;
}

function selectLocalIpcRoute<Payload extends C2ResponsePayload>(
  routes: readonly C2RelayRouteInfo[],
  anchorUrl: string,
  ipcOptions: C2IpcTransportOptions<Payload> | undefined,
  excludedRouteTokens: ReadonlySet<string>,
): C2RelayRouteInfo | undefined {
  if (ipcOptions === undefined || !relayAnchorAllowsLocalIpc(anchorUrl)) {
    return undefined;
  }
  return routes.find(
    (route) =>
      route.ipcAddress !== undefined &&
      route.serverId !== undefined &&
      route.serverInstanceId !== undefined &&
      !excludedRouteTokens.has(routeTokenKey(route)),
  );
}

function relayAnchorAllowsLocalIpc(anchorUrl: string): boolean {
  const hostname = new URL(anchorUrl).hostname.toLowerCase();
  return (
    hostname === "localhost" ||
    hostname.startsWith("127.") ||
    hostname === "::1" ||
    hostname === "[::1]"
  );
}

function routeTokenKey(route: C2RouteToken): string {
  return JSON.stringify([route.routeUid]);
}

function samePathFallbackDeniedError(
  routeName: string,
  route: C2RelayRouteInfo,
): C2HttpRelayError {
  return new C2HttpRelayError(
    409,
    canonicalRelayErrorBody({
      code: 714,
      name: "FallbackDenied",
      message: "same-path fallback denied after verified local IPC failure",
      details: {
        route: routeName,
        route_uid: route.routeUid,
      },
    }),
  );
}

function resourceNotFoundError(routeName: string): C2HttpRelayError {
  return new C2HttpRelayError(
    404,
    canonicalRelayErrorBody({
      code: 701,
      name: "ResourceNotFound",
      message: `route not found: ${routeName}`,
      details: { route: routeName },
    }),
  );
}

function canonicalRelayErrorBody(error: {
  readonly code: number;
  readonly name: string;
  readonly message: string;
  readonly details: Readonly<Record<string, string>>;
}): string {
  return JSON.stringify({
    version: 1,
    code: error.code,
    name: error.name,
    message: error.message,
    details: error.details,
  });
}

function normalizeRelayMaxAttempts(value: number | undefined): number {
  if (value === undefined) {
    return 3;
  }
  const maxAttempts = 32;
  if (!Number.isFinite(value) || !Number.isSafeInteger(value) || value < 0 || value > maxAttempts) {
    throw new Error(`C-Two relay maxAttempts must be a non-negative safe integer no greater than ${maxAttempts}.`);
  }
  return Math.max(1, value);
}

function normalizeRelayRouteCacheTtlMs(value: number | undefined): number {
  return normalizeNonNegativeSafeIntegerMs(value, "C-Two relay routeCacheTtlMs", 30_000);
}

function relayRouteCacheKey(routeName: string, contract: C2RouteContractIdentity): string {
  return JSON.stringify([
    routeName,
    contract.namespace,
    contract.name,
    contract.version,
    contract.abiHash,
    contract.signatureHash,
  ]);
}

function normalizeTimeoutMs(value: number | undefined, label: string, defaultMs: number): number {
  return normalizeNonNegativeSafeIntegerMs(value, label, defaultMs);
}

function normalizeNonNegativeSafeIntegerMs(value: number | undefined, label: string, defaultMs: number): number {
  if (value === undefined) {
    return defaultMs;
  }
  const maxTimeoutMs = 2_147_483_647;
  if (!Number.isFinite(value) || !Number.isSafeInteger(value) || value < 0 || value > maxTimeoutMs) {
    throw new Error(`${label} must be a non-negative safe integer no greater than ${maxTimeoutMs}.`);
  }
  return value;
}

type C2AbortController = {
  readonly signal: C2AbortSignal;
  abort(reason?: unknown): void;
};

async function fetchWithTimeout<T>(
  operation: (signal: C2AbortSignal | undefined) => Promise<T>,
  timeoutMs: number,
  label: string,
): Promise<T> {
  if (timeoutMs === 0) {
    return await operation(undefined);
  }
  const controllerCtor = (globalThis as unknown as { AbortController?: new () => C2AbortController }).AbortController;
  const controller = controllerCtor ? new controllerCtor() : undefined;
  let timedOut = false;
  let timeoutHandle: ReturnType<typeof setTimeout> | undefined;
  const timeoutError = () => new C2HttpTransportError(`${label} timed out after ${timeoutMs}ms.`);
  const timeoutPromise = new Promise<never>((_resolve, reject) => {
    timeoutHandle = setTimeout(() => {
      timedOut = true;
      const error = timeoutError();
      controller?.abort(error);
      reject(error);
    }, timeoutMs);
  });
  try {
    return await Promise.race([operation(controller?.signal), timeoutPromise]);
  } catch (error) {
    if (timedOut) {
      throw timeoutError();
    }
    throw error;
  } finally {
    if (timeoutHandle !== undefined) {
      clearTimeout(timeoutHandle);
    }
  }
}

async function resolveRelayRoutesCached(
  fetchImpl: C2Fetch,
  anchorUrl: string,
  routeName: string,
  contract: C2RouteContractIdentity,
  headers: Readonly<Record<string, string>>,
  cache: Map<string, C2RelayRouteCacheEntry>,
  cacheTtlMs: number,
  resolveTimeoutMs: number,
  forceRefresh: boolean,
): Promise<readonly C2RelayRouteInfo[]> {
  const cacheKey = relayRouteCacheKey(routeName, contract);
  if (forceRefresh) {
    invalidateRelayRouteCache(cache, cacheKey);
  }
  const now = Date.now();
  if (cacheTtlMs > 0) {
    const cached = cache.get(cacheKey);
    if (cached) {
      if (cached.expiresAtMs > now) {
        return cached.routes;
      }
      cache.delete(cacheKey);
    }
  }
  const routes = await resolveRelayRoutes(fetchImpl, anchorUrl, routeName, contract, headers, resolveTimeoutMs);
  if (cacheTtlMs > 0) {
    cache.set(cacheKey, {
      routeName,
      routes,
      expiresAtMs: now + cacheTtlMs,
    });
  }
  return routes;
}

function invalidateRelayRouteCache(cache: Map<string, C2RelayRouteCacheEntry>, cacheKey: string): void {
  cache.delete(cacheKey);
}

function orderRelayRoutes(
  routes: readonly C2RelayRouteInfo[],
  currentRelayUrl: string | undefined,
  excludedRoutes: ReadonlySet<string>,
): readonly C2RelayRouteInfo[] {
  const available = routes.filter((route) => {
    const relayUrl = normalizeRelayBaseUrl(route.relayUrl, "C-Two resolved relay URL");
    return !excludedRoutes.has(relayUrl);
  });
  if (!currentRelayUrl) {
    return available;
  }
  const preferred: C2RelayRouteInfo[] = [];
  const rest: C2RelayRouteInfo[] = [];
  for (const route of available) {
    const relayUrl = normalizeRelayBaseUrl(route.relayUrl, "C-Two resolved relay URL");
    if (relayUrl === currentRelayUrl) {
      preferred.push(route);
    } else {
      rest.push(route);
    }
  }
  return [...preferred, ...rest];
}

async function resolveRelayRoutes(
  fetchImpl: C2Fetch,
  anchorUrl: string,
  routeName: string,
  contract: C2RouteContractIdentity,
  headers: Readonly<Record<string, string>>,
  resolveTimeoutMs: number,
): Promise<readonly C2RelayRouteInfo[]> {
  const resolveUrl = `${anchorUrl}/_resolve/${encodePathSegment(routeName)}?crm_ns=${encodeQueryValue(contract.namespace)}&crm_name=${encodeQueryValue(contract.name)}&crm_ver=${encodeQueryValue(contract.version)}&abi_hash=${encodeQueryValue(contract.abiHash)}&signature_hash=${encodeQueryValue(contract.signatureHash)}`;
  return await fetchWithTimeout(async (signal) => {
    let response: C2FetchResponse;
    try {
      response = await fetchImpl(resolveUrl, {
        method: "GET",
        headers,
        signal,
      });
    } catch (error) {
      throw new C2HttpTransportError(`C-Two relay resolve fetch failed: ${String(error)}`);
    }
    if (response.status !== 200) {
      let body = "";
      try {
        body = await readHttpTextResponseBody(response);
      } catch (error) {
        throw new C2HttpTransportError(`C-Two relay resolve body read failed: ${String(error)}`);
      }
      throw new C2HttpRelayError(response.status, body);
    }
    let body: string;
    try {
      body = await readHttpTextResponseBody(response);
    } catch (error) {
      throw new C2HttpTransportError(`C-Two relay resolve body read failed: ${String(error)}`);
    }
    let payload: unknown;
    try {
      payload = JSON.parse(body) as unknown;
    } catch (error) {
      throw new C2HttpTransportError(`C-Two relay resolve returned invalid JSON: ${String(error)}`);
    }
    if (!Array.isArray(payload)) {
      throw new C2HttpTransportError("C-Two relay resolve returned a non-array route response.");
    }
    let routes: C2RelayRouteInfo[];
    try {
      routes = payload.map((route, index) => normalizeRelayRouteInfo(route, index));
    } catch (error) {
      throw new C2HttpTransportError(`C-Two relay resolve payload shape invalid: ${String(error)}`);
    }
    for (let index = 0; index < routes.length; index += 1) {
      validateRelayRouteContract(routes[index], index, routeName, contract);
    }
    return routes;
  }, resolveTimeoutMs, "C-Two relay resolve");
}

function normalizeRelayRouteInfo(route: unknown, index: number): C2RelayRouteInfo {
  if (typeof route !== "object" || route === null) {
    throw new Error(`C-Two relay resolve route ${index} is not an object.`);
  }
  const item = route as Record<string, unknown>;
  if ("route_revision" in item) throw new Error(`C-Two relay resolve route ${index} contains unknown fields.`);
  const ipcAddress = optionalStringField(item, "ipc_address", index);
  const serverId = optionalStringField(item, "server_id", index);
  const serverInstanceId = optionalStringField(item, "server_instance_id", index);
  const localFactCount = [ipcAddress, serverId, serverInstanceId].filter(
    (value) => value !== undefined,
  ).length;
  if (localFactCount !== 0 && localFactCount !== 3) {
    throw new Error(
      `C-Two relay resolve route ${index} must provide ipc_address, server_id, and server_instance_id together.`,
    );
  }
  return {
    name: stringField(item, "name", index),
    relayUrl: normalizeRelayBaseUrl(
      stringField(item, "relay_url", index),
      `C-Two relay resolve route ${index} relay_url`,
    ),
    routeUid: stringField(item, "route_uid", index),
    ipcAddress,
    serverId,
    serverInstanceId,
    crmNs: stringField(item, "crm_ns", index),
    crmName: stringField(item, "crm_name", index),
    crmVer: stringField(item, "crm_ver", index),
    abiHash: stringField(item, "abi_hash", index),
    signatureHash: stringField(item, "signature_hash", index),
    maxPayloadSize: numberField(item, "max_payload_size", index),
  };
}

function validateRelayRouteContract(route: C2RelayRouteInfo, index: number, routeName: string, contract: C2RouteContractIdentity): void {
  if (
    route.name !== routeName ||
    route.crmNs !== contract.namespace ||
    route.crmName !== contract.name ||
    route.crmVer !== contract.version ||
    route.abiHash !== contract.abiHash ||
    route.signatureHash !== contract.signatureHash
  ) {
    throw new Error(`C-Two relay resolve route ${index} does not match the expected CRM contract.`);
  }
}

function payloadTooLargeError(routeName: string, payloadSize: number, maxPayloadSize: number): C2HttpRelayError {
  return new C2HttpRelayError(413, JSON.stringify({
    error: "PayloadTooLarge",
    route: routeName,
    payload_size: payloadSize,
    max_payload_size: maxPayloadSize,
  }));
}

function stringField(item: Record<string, unknown>, field: string, index: number): string {
  const value = item[field];
  if (typeof value !== "string" || value.length === 0) {
    throw new Error(`C-Two relay resolve route ${index} is missing string field ${field}.`);
  }
  return value;
}

function optionalStringField(item: Record<string, unknown>, field: string, index: number): string | undefined {
  const value = item[field];
  if (value === undefined || value === null) {
    return undefined;
  }
  if (typeof value !== "string" || value.length === 0) {
    throw new Error(`C-Two relay resolve route ${index} has invalid optional string field ${field}.`);
  }
  return value;
}

function numberField(item: Record<string, unknown>, field: string, index: number): number {
  const value = item[field];
  if (typeof value !== "number" || !Number.isSafeInteger(value) || value <= 0) {
    throw new Error(`C-Two relay resolve route ${index} is missing positive integer field ${field}.`);
  }
  return value;
}

function isRetryableResolveError(error: unknown): boolean {
  if (error instanceof C2HttpTransportError) {
    return true;
  }
  return error instanceof C2HttpRelayError && error.status >= 500 && error.status <= 599;
}

function isRetryableRouteError(error: unknown): boolean {
  if (!(error instanceof C2HttpRelayError)) {
    return false;
  }
  const canonical = parseCanonicalRelayError(error.body);
  if (canonical === undefined) {
    return false;
  }
  if (error.status === 404 && canonical.name === "ResourceNotFound") {
    return true;
  }
  if (error.status === 409 && canonical.name === "RouteStale") {
    return true;
  }
  return (
    error.status === 502 &&
    canonical.name === "ResourceUnavailable" &&
    canonical.details.dispatch_phase === "pre_dispatch"
  );
}

function parseCanonicalRelayError(
  body: string,
): { readonly name: string; readonly details: Readonly<Record<string, unknown>> } | undefined {
  try {
    const parsed = JSON.parse(body) as unknown;
    if (typeof parsed !== "object" || parsed === null || Array.isArray(parsed)) {
      return undefined;
    }
    const record = parsed as Record<string, unknown>;
    if (typeof record.name !== "string") {
      return undefined;
    }
    if (
      typeof record.details !== "object" ||
      record.details === null ||
      Array.isArray(record.details)
    ) {
      return undefined;
    }
    return {
      name: record.name,
      details: record.details as Readonly<Record<string, unknown>>,
    };
  } catch {
    return undefined;
  }
}

function requireRouteContractIdentity(contract: C2ContractIdentity): C2RouteContractIdentity {
  if (typeof contract !== "object" || contract === null) {
    throw new Error("C-Two route contract identity must be an object.");
  }
  if (contract.schema !== "c-two.contract.v2") {
    throw new Error("C-Two route contract identity must use schema c-two.contract.v2.");
  }
  requireRouteContractTextField(contract.namespace, "namespace");
  requireRouteContractTextField(contract.name, "name");
  requireRouteContractTextField(contract.version, "version");
  if (!isLowerHexHash(contract.abiHash) || !isLowerHexHash(contract.signatureHash)) {
    throw new Error(`Contract ${contract.name} is missing relay route fingerprints.`);
  }
  return contract as C2RouteContractIdentity;
}

function requireRouteContractTextField(value: unknown, field: string): void {
  if (typeof value !== "string" || value.length === 0) {
    throw new Error(`C-Two route contract identity is missing non-empty ${field}.`);
  }
  if (utf8ByteLength(value) > C2_MAX_WIRE_TEXT_BYTES) {
    throw new Error(`C-Two route contract ${field} cannot exceed ${C2_MAX_WIRE_TEXT_BYTES} bytes.`);
  }
  if (value.trim() !== value) {
    throw new Error(`C-Two route contract ${field} cannot contain leading or trailing whitespace.`);
  }
  if (/[\u0000-\u001F\u007F-\u009F]/.test(value)) {
    throw new Error(`C-Two route contract ${field} cannot contain control characters.`);
  }
  if (value.includes("/") || value.includes("\\")) {
    throw new Error(`C-Two route contract ${field} cannot contain path or tag separators.`);
  }
}

function utf8ByteLength(value: string): number {
  let bytes = 0;
  for (const char of value) {
    const codePoint = char.codePointAt(0) ?? 0;
    if (codePoint <= 0x7F) {
      bytes += 1;
    } else if (codePoint <= 0x7FF) {
      bytes += 2;
    } else if (codePoint <= 0xFFFF) {
      bytes += 3;
    } else {
      bytes += 4;
    }
  }
  return bytes;
}

function requireRouteNamePathValue(value: unknown): void {
  requireRoutePathTextValue(value, "routeName");
}

function requireMethodPathValue(value: unknown): void {
  requireRoutePathTextValue(value, "method");
}

function requireRoutePathTextValue(value: unknown, field: "routeName" | "method"): void {
  if (typeof value !== "string" || value.length === 0) {
    throw new Error(`C-Two route ${field} must be a non-empty string.`);
  }
  if (utf8ByteLength(value) > C2_MAX_WIRE_TEXT_BYTES) {
    throw new Error(`C-Two route ${field} cannot exceed ${C2_MAX_WIRE_TEXT_BYTES} bytes.`);
  }
  if (value.trim() !== value) {
    throw new Error(`C-Two route ${field} cannot contain leading or trailing whitespace.`);
  }
  if (/[\u0000-\u001F\u007F-\u009F]/.test(value)) {
    throw new Error(`C-Two route ${field} cannot contain control characters.`);
  }
  if (field === "routeName" ? value.includes("\\") : value.includes("/") || value.includes("\\")) {
    throw new Error(`C-Two route ${field} cannot contain path or tag separators.`);
  }
}

function isLowerHexHash(value: string): boolean {
  return typeof value === "string" && /^[0-9a-f]{64}$/.test(value);
}

function encodePathSegment(value: string): string {
  return encodeURIComponent(value).replace(/[!'()*]/g, (char) =>
    `%${char.charCodeAt(0).toString(16).toUpperCase()}`,
  );
}

function encodeQueryValue(value: string): string {
  return encodeURIComponent(value);
}
