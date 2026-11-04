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
  /** Native write timeout can seal new admission while dispatched I/O finishes. */
  readonly deactivated?: boolean;
  readonly deactivation?: Promise<void>;
  write(data: C2ByteArray): void | Promise<void>;
  readExactly(byteLength: number): C2ByteArray | Promise<C2ByteArray>;
  close(): void | Promise<void>;
}

export type C2IpcConnect = (address: string, timeoutMs?: number) => C2IpcConnection | Promise<C2IpcConnection>;

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
  /** One deadline from native connection through the IPC metadata handshake. */
  readonly connectTimeoutMs?: number;
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
  onDeactivated?: () => void;
  readonly connection: C2IpcConnection;
  readonly handshake: C2IpcHandshake;
  writeChain: Promise<void>;
  closing: boolean;
  discarded?: boolean;
  reading: Promise<void> | undefined;
  nextRequestId: bigint;
  readonly activeCalls: Map<bigint, "control" | "unary">;
}

interface C2IpcFrame {
  readonly requestId: bigint;
  readonly flags: number;
  readonly payload: Uint8Array;
}

type C2IpcCallResponse =
  | { readonly kind: "inline"; readonly payload: Uint8Array }
  | { readonly kind: "shm"; readonly block: C2IpcShmBlock; readonly response: unknown };

interface C2IpcCanonicalError {
  readonly version: 1;
  readonly code: number;
  readonly name: string;
  readonly message: string;
  readonly details: Readonly<Record<string, string>>;
}

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

export class C2IpcCallRejectedError extends Error {
  readonly transportPhase = "pre_dispatch";
  readonly retrySafe = true;
  readonly code: number;
  readonly details: Readonly<Record<string, string>>;
  readonly canonicalName: string;
  constructor(readonly payload: Uint8Array, canonical: C2IpcCanonicalError) {
    super(canonical.message);
    this.name = "C2IpcCallRejectedError";
    this.code = canonical.code;
    this.canonicalName = canonical.name;
    this.details = canonical.details;
    Object.setPrototypeOf(this, C2IpcCallRejectedError.prototype);
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

/** Framing, write commitment, or release authority cannot be trusted. */
class C2IpcConnectionFailure extends C2IpcTransportError {
  constructor(message: string) {
    super(message);
    Object.setPrototypeOf(this, C2IpcConnectionFailure.prototype);
  }
}

/** The native reader consumed one whole frame payload but could not allocate its local copy. */
class C2IpcLocalPayloadFailure extends C2IpcTransportError {
  constructor(readonly requestId: bigint, readonly flags: number, readonly payloadLength: number, cause: unknown, readonly prefix?: Uint8Array) {
    super(`C-Two IPC local response payload allocation failed: ${String(cause)}`);
    Object.setPrototypeOf(this, C2IpcLocalPayloadFailure.prototype);
  }
}

export function createIpcEncodedTransport<Payload extends C2ResponsePayload = C2ByteArray>(address: string, options: C2IpcTransportOptions<Payload>): C2IpcEncodedTransport<Payload> {
  validateIpcAddress(address);
  const connect = normalizeIpcConnect(options, "C-Two IPC transport options");
  const connectTimeoutMs = normalizeIpcConnectTimeoutMs(options.connectTimeoutMs);
  const responsePayloadAllocator = normalizeResponsePayloadAllocator(options.responsePayloadAllocator, "C-Two IPC responsePayloadAllocator");
  const requestShmThreshold = normalizeIpcRequestShmThreshold(options.requestShmThreshold, "C-Two IPC requestShmThreshold");
  const requestChunkSize = normalizeIpcRequestChunkSize(options.requestChunkSize, "C-Two IPC requestChunkSize");
  const expectedServerIdentity = normalizeExpectedServerIdentity(options.expectedServerIdentity, "C-Two IPC expectedServerIdentity");
  const expectedRouteToken = normalizeExpectedRouteToken(options.expectedRouteToken, "C-Two IPC expectedRouteToken");
  const observationPath = normalizeIpcObservationPath(options.observationPath);
  const observe = normalizeTransportObserver(options.observe, "C-Two IPC observe");
  let connectionPromise: Promise<C2IpcOpenConnection> | undefined;
  let deactivated = false;
  const queuedCalls = new Set<(error: C2IpcTransportError) => void>();
  const deactivateAdmission = (): void => {
    deactivated = true;
    for (const reject of queuedCalls) reject(new C2IpcTransportError("C-Two IPC transport is deactivated."));
    queuedCalls.clear();
  };
  let closePromise: Promise<void> | undefined;
  // Bindings survive stream loss and reconnect without redirecting acquired routes.
  // Explicit close seals this transport.
  let boundServerIdentity: C2ServerIdentity | undefined;
  const boundRoutes = new Map<string, C2IpcRouteInfo>();
  let callChain: Promise<void> = Promise.resolve();
  let requests = 0;
  let responses = 0;

  const openConnection = (): Promise<C2IpcOpenConnection> => {
    if (deactivated) throw new C2IpcTransportError("C-Two IPC transport is deactivated.");
    if (connectionPromise === undefined) {
      const pending = connectAndHandshakeIpc(address, connect, connectTimeoutMs).then(open => {
        open.onDeactivated = () => {
          // Closing a discarded stream must not seal a newer incarnation.
          if (connectionPromise !== pending) return;
          deactivateAdmission();
          void closeTransport().catch(error => console.warn("Warning: IPC connection retirement failed:", error));
        };
        void open.connection.deactivation?.then(open.onDeactivated);
        return open;
      }).catch((error) => {
        if (connectionPromise === pending) connectionPromise = undefined;
        throw error;
      });
      connectionPromise = pending;
    }
    return connectionPromise;
  };

  const allocateRequestId = (open: C2IpcOpenConnection, kind: "control" | "unary"): bigint => {
    const requestId = open.nextRequestId;
    if (requestId > 0xffffffffffffffffn) {
      open.connection.memory?.deactivate();
      throw new C2IpcConnectionFailure("C-Two IPC request ID space exhausted.");
    }
    open.nextRequestId += 1n;
    open.activeCalls.set(requestId, kind);
    return requestId;
  };

  const discardConnection = async (open: C2IpcOpenConnection): Promise<void> => {
    if (open.discarded) return;
    open.discarded = true;
    if (connectionPromise !== undefined) {
      const current = await connectionPromise.catch(() => undefined);
      if (current === open) connectionPromise = undefined;
    }
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
    catch (error) {
      if (error instanceof C2IpcConnectionFailure) await discardConnection(open);
      throw error;
    }
  };

  const control = async (
    open: C2IpcOpenConnection,
    tag: number,
    request: unknown,
    responseTag: number,
  ): Promise<C2IpcCatalogReply> => {
    let requestId = 0n;
    if (open.closing) throw new C2IpcTransportError("C-Two IPC connection is closing.");
    // New route queries require business admission, even with CTRL on wire.
    try {
      const body = concatUint8Arrays([new Uint8Array([tag]), C2_TEXT_ENCODER.encode(JSON.stringify(request))]);
      await writeIpcFrame(open, () => {
        requestId = allocateRequestId(open, "control");
        return encodeIpcFrame(requestId, C2_IPC_FLAG_CTRL, body);
      });
    } catch (error) {
      if (requestId !== 0n) open.activeCalls.delete(requestId);
      if (error instanceof C2IpcConnectionFailure) await discardConnection(open);
      if (error instanceof C2IpcTransportError) {
        throw error;
      }
      throw new C2IpcTransportError(`C-Two IPC route control write failed: ${String(error)}`);
    }
    let frame: C2IpcFrame;
    try {
      frame = await readIpcResponseFrame(open, requestId);
      if ((frame.flags & C2_IPC_FLAG_BUDDY) !== 0) await discardIpcShmFrame(open, frame);
    } catch (error) {
      if (requestId !== 0n) open.activeCalls.delete(requestId);
      if (error instanceof C2IpcConnectionFailure) await discardConnection(open);
      throw error;
    }
    if (frame.requestId !== requestId || frame.flags !== (C2_IPC_FLAG_RESPONSE | C2_IPC_FLAG_CTRL)) {
      open.activeCalls.delete(requestId);
      await discardConnection(open);
      throw new C2IpcConnectionFailure("C-Two IPC route control reply has invalid request id or flags.");
    }
    open.activeCalls.delete(requestId);
    return decodeIpcCatalogReply(frame.payload, responseTag);
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
    if (deactivated || open.connection.deactivated) throw new C2IpcTransportError("C-Two IPC transport is deactivated.");
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
    if (deactivated) return Promise.reject(new C2IpcTransportError("C-Two IPC transport is deactivated."));
    let rejectQueued!: (error: C2IpcTransportError) => void;
    const stopped = new Promise<T>((_, reject) => { rejectQueued = reject; });
    queuedCalls.add(rejectQueued);
    const begin = (): Promise<T> => {
      queuedCalls.delete(rejectQueued);
      if (deactivated) throw new C2IpcTransportError("C-Two IPC transport is deactivated.");
      return run();
    };
    const execution = callChain.then(begin, begin);
    callChain = execution.then(() => undefined, () => undefined);
    return Promise.race([execution, stopped]).finally(() => { queuedCalls.delete(rejectQueued); });
  };

  const closeTransport = async (): Promise<void> => {
    if (closePromise !== undefined) return await closePromise;
    deactivateAdmission();
    const pendingCalls = callChain;
    closePromise = (async () => {
      await pendingCalls;
      const current = connectionPromise;
      if (current === undefined) return;
      const open = await current.catch(() => undefined);
      if (open === undefined || open.discarded) return;
      let closeAttempted = false;
      try {
        open.closing = true;
        if (open.connection.memory !== undefined) {
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
    })();
    return await closePromise;
  };

  return {
    async prepare(routeName: string, contract: C2ContractIdentity): Promise<void> {
      if (deactivated) throw new C2IpcTransportError("C-Two IPC transport is deactivated.");
      await enqueue(async () => { await prepareRoute(routeName, contract); });
    },

    async call(routeName: string, contract: C2ContractIdentity, method: string, payload: Uint8Array): Promise<Payload> {
      const run = async (): Promise<Payload> => {
        if (deactivated) throw new C2IpcTransportError("C-Two IPC transport is deactivated.");
        requireMethodPathValue(method);
        const requestPayload = requireUint8Array(payload, "payload");
        const { open, route } = await prepareRoute(routeName, contract);
        if (deactivated || open.connection.deactivated) throw new C2IpcTransportError("C-Two IPC transport is deactivated.");
        if (requestPayload.byteLength > route.maxPayloadSize) {
          throw new C2IpcTransportError(`C-Two IPC route ${routeName} payload ${requestPayload.byteLength} exceeds max_payload_size ${route.maxPayloadSize}.`);
        }
        const methodInfo = findIpcMethod(route, method);
        let requestId = 0n;
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
        let stopChunkUpload = false;
        const receive = async (): Promise<C2IpcCallResponse> => {
          try {
            const first = await readIpcResponseFrame(open, requestId);
            if ((first.flags & C2_IPC_FLAG_CHUNKED) === 0) stopChunkUpload = true;
            const result = await readIpcCallEnd(open, first, requestId);
            stopChunkUpload = true;
            return result;
          } catch (error) {
            stopChunkUpload = true;
            if (error instanceof C2IpcConnectionFailure) await discardConnection(open);
            throw error instanceof C2IpcTransportError || error instanceof C2CrmMethodError || error instanceof C2IpcCallRejectedError ? error
              : new C2IpcTransportError(`C-Two IPC call response read failed: ${String(error)}`);
          } finally {
            if (requestId !== 0n) open.activeCalls.delete(requestId);
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
          let openMetadata: Uint8Array;
          try {
            openMetadata = encodeIpcOpenMetadata(route, methodInfo.index);
          } catch (error) {
            if (error instanceof C2IpcTransportError) throw error;
            throw new C2IpcTransportError(`C-Two IPC call write failed: ${String(error)}`);
          }
          let rejectSubmission!: (error: unknown) => void;
          let resolveSubmission!: (id: bigint) => void;
          const submitted = new Promise<bigint>((resolve, reject) => { resolveSubmission = resolve; rejectSubmission = reject; });
          void submitted.catch(() => undefined);
          const firstFrame = (flags: number, storage: C2ByteArray): C2ByteArray => {
            requestId = allocateRequestId(open, "unary");
            const encoded = encodeIpcFrame(requestId, flags, concatUint8Arrays([new Uint8Array([C2_IPC_OPEN]), openMetadata, storage]));
            resolveSubmission(requestId);
            return encoded;
          };
          const writing = (async (): Promise<void> => {
            try {
              if (useShm) {
                await writeIpcBuddyCallRequestFrame(open, block => firstFrame(C2_IPC_FLAG_RPC_RECORD | C2_IPC_FLAG_BUDDY, encodeIpcBuddyPayload(block)), requestPayload);
              } else if (chunked) {
                let offset = 0;
                const firstData = requestPayload.subarray(0, requestChunkSize);
                await writeIpcFrame(open, () => firstFrame(C2_IPC_FLAG_RPC_RECORD | C2_IPC_FLAG_CHUNKED | C2_IPC_FLAG_FIRST_CHUNK,
                  concatUint8Arrays([writeU64LEBigInt(BigInt(requestPayload.byteLength), "total_bytes"), firstData])));
                offset = firstData.byteLength;
                while (offset < requestPayload.byteLength) {
                  // A received service refusal stops after the preceding
                  // complete frame; the stream remains usable for the next call.
                  if (stopChunkUpload) break;
                  const chunk = requestPayload.subarray(offset, Math.min(offset + firstData.byteLength, requestPayload.byteLength));
                  await writeIpcFrame(open, encodeIpcFrame(requestId, C2_IPC_FLAG_RPC_RECORD | C2_IPC_FLAG_CHUNKED,
                    concatUint8Arrays([new Uint8Array([C2_IPC_OPEN]), chunk])), undefined, true);
                  offset += chunk.byteLength;
                }
              } else {
                await writeIpcFrame(open, () => firstFrame(C2_IPC_FLAG_RPC_RECORD, requestPayload));
              }
              requests += 1;
            } catch (error) {
              if (requestId === 0n) rejectSubmission(error);
              else if (!(error instanceof C2IpcConnectionFailure)) open.activeCalls.delete(requestId);
              if (error instanceof C2IpcTransportError) throw error;
              throw new C2IpcTransportError(`C-Two IPC call write failed: ${String(error)}`);
            }
          })();
          let reply: C2IpcCallResponse;
          if (chunked) {
            const receiving = submitted.then(() => receive());
            const finishingWrite = writing.catch(async error => {
              // A partial request chunk sequence leaves the peer's request
              // assembly incomplete and the concurrent read must be woken.
              await discardConnection(open);
              throw error;
            });
            const [written, received] = await Promise.allSettled([finishingWrite, receiving]);
            if (written.status === "rejected") {
              if (received.status === "fulfilled" && received.value.kind === "shm") {
                open.connection.memory!.discardResponse(received.value.response);
                try { await writeIpcFrame(open); } catch { /* preserve the original failed exchange */ }
              }
              if (received.status === "rejected" && received.reason instanceof C2IpcConnectionFailure) throw received.reason;
              throw written.reason;
            }
            if (received.status === "rejected") throw received.reason;
            reply = received.value;
          } else {
            try {
              await writing;
            } catch (error) {
              if (error instanceof C2IpcConnectionFailure) await discardConnection(open);
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
          if (error instanceof C2IpcConnectionFailure) await discardConnection(open);
        }
        if (responseError !== undefined) {
          throw responseError;
        }
        return responseValue as Payload;
      };
      if (deactivated) throw new C2IpcTransportError("C-Two IPC transport is deactivated.");
      try { return await enqueue(run); }
      finally {
        const open = await connectionPromise?.catch(() => undefined);
        if (open?.connection.deactivated && !deactivated) {
          void closeTransport().catch(error => console.warn("Warning: IPC connection retirement failed:", error));
        }
      }
    },

    async close(): Promise<void> {
      return await closeTransport();
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

const C2_IPC_HANDSHAKE_VERSION = 16;
const C2_IPC_FRAME_BODY_HEADER_BYTES = 12;
const C2_IPC_FRAME_HEADER_BYTES = 16;
const C2_IPC_BUDDY_PAYLOAD_BYTES = 23;
const C2_IPC_BUDDY_FLAG_DEDICATED = 1 << 0;
const C2_IPC_DEFAULT_REQUEST_SHM_THRESHOLD = 4096;
const C2_IPC_DEFAULT_REQUEST_CHUNK_SIZE = 131_072;
const C2_IPC_CAP_SHM_REQUEST = 1 << 3;
const C2_IPC_CAP_SHM_RESPONSE = 1 << 4;
const C2_IPC_FLAG_SIGNAL = 1 << 11;
const C2_IPC_FLAG_RESPONSE = 1 << 1;
const C2_IPC_FLAG_HANDSHAKE = 1 << 2;
const C2_IPC_FLAG_CTRL = 1 << 4;
const C2_IPC_FLAG_BUDDY = 0x40;
const C2_IPC_FLAG_CHUNKED = 0x200;
const C2_IPC_FLAG_RPC_RECORD = 0x1000;
const C2_IPC_FLAG_FIRST_CHUNK = 0x2000;
const C2_IPC_OPEN = 0x01;
const C2_IPC_CALL_END = 0x06;
const C2_IPC_RELEASE = 0x07;
// Canonical c2-wire route catalog MsgType discriminants.
const C2_IPC_ROUTE_LOOKUP = 0x10;
const C2_IPC_ROUTE_LOOKUP_ACK = 0x11;
const C2_IPC_ROUTE_NACK = 0x15;
const C2_IPC_REPLY_STATUS_SUCCESS = 0x00;
const C2_IPC_REPLY_STATUS_ERROR = 0x01;
const C2_IPC_REPLY_STATUS_NOT_EXECUTED = 0x02;
const C2_IPC_ERROR_NAMES: Readonly<Record<number, string>> = {
  0: "Unknown", 1: "ResourceInputDeserializing", 2: "ResourceOutputSerializing",
  3: "ResourceFunctionExecuting", 4: "ResourceInputFromBuffer", 5: "ClientInputSerializing",
  6: "ClientOutputDeserializing", 7: "ClientCallingResource", 8: "ClientOutputFromBuffer",
  701: "ResourceNotFound", 702: "ResourceUnavailable", 703: "ResourceAlreadyRegistered",
  704: "RouteStale", 705: "RegistryUnavailable", 706: "WriteConflict", 707: "ResourceClosed",
  708: "ResourceRemoved", 709: "ContractMismatch", 710: "IdentityMismatch",
  711: "RouteCatalogCompacted", 712: "RouteWatchUnavailable", 713: "ProtocolViolation",
  714: "FallbackDenied", 715: "CallDeadlineExceeded", 716: "UnsupportedCallMode",
  717: "CallCapacityExceeded", 718: "CallCancelled",
};
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

function normalizeIpcConnectTimeoutMs(value: number | undefined): number | undefined {
  if (value !== undefined && (!Number.isSafeInteger(value) || value <= 0)) {
    throw new C2IpcTransportError("C-Two IPC connectTimeoutMs must be a positive safe integer.");
  }
  return value;
}

function ipcConnectDeadlineError(): C2IpcTransportError {
  return new C2IpcTransportError("C-Two IPC connection deadline expired.");
}

interface C2IpcConnectDeadline {
  readonly startedAt: number;
  readonly timeoutMs: number;
}

function remainingIpcConnectMs(deadline: C2IpcConnectDeadline | undefined): number | undefined {
  if (deadline === undefined) return undefined;
  const remaining = deadline.timeoutMs - (performance.now() - deadline.startedAt);
  if (remaining <= 0) throw ipcConnectDeadlineError();
  return Math.max(1, Math.ceil(remaining));
}

async function withinIpcConnectDeadline<T>(
  deadline: C2IpcConnectDeadline | undefined,
  operation: () => T | Promise<T>,
  disposeLate?: (value: T) => void | Promise<void>,
): Promise<T> {
  if (deadline === undefined) return await operation();
  remainingIpcConnectMs(deadline);
  let timer: ReturnType<typeof setTimeout> | undefined;
  let expired = false;
  const pending = Promise.resolve().then(operation);
  pending.then(value => {
    if (expired && disposeLate !== undefined) void Promise.resolve().then(() => disposeLate(value)).catch(() => undefined);
  }, () => undefined);
  const timeout = new Promise<never>((_, reject) => {
    const schedule = (): void => {
      const remaining = deadline.timeoutMs - (performance.now() - deadline.startedAt);
      if (remaining <= 0) {
        expired = true;
        reject(ipcConnectDeadlineError());
      } else {
        timer = setTimeout(schedule, Math.min(Math.ceil(remaining), 2_147_483_647));
      }
    };
    schedule();
  });
  try {
    const value = await Promise.race([pending, timeout]);
    if (deadline.timeoutMs - (performance.now() - deadline.startedAt) <= 0) {
      expired = true;
      if (disposeLate !== undefined) void Promise.resolve().then(() => disposeLate(value)).catch(() => undefined);
      throw ipcConnectDeadlineError();
    }
    return value;
  } catch (error) {
    if (expired || deadline.timeoutMs - (performance.now() - deadline.startedAt) <= 0) throw ipcConnectDeadlineError();
    throw error;
  } finally {
    if (timer !== undefined) clearTimeout(timer);
  }
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
  const maxRequestChunkSize = C2_MAX_RESPONSE_PAYLOAD_BYTES - 1 - 8 - C2_MAX_WIRE_TEXT_BYTES * 7 - 2;
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
  connectTimeoutMs: number | undefined,
): Promise<C2IpcOpenConnection> {
  const deadline = connectTimeoutMs === undefined ? undefined : { startedAt: performance.now(), timeoutMs: connectTimeoutMs };
  let connection: C2IpcConnection;
  try {
    connection = requireIpcConnection(await withinIpcConnectDeadline(
      deadline,
      () => connect(address, remainingIpcConnectMs(deadline)),
      value => { if (typeof (value as C2IpcConnection)?.close === "function") return (value as C2IpcConnection).close(); },
    ));
  } catch (error) {
    if (error instanceof C2IpcTransportError) {
      throw error;
    }
    throw new C2IpcTransportError(`C-Two IPC connect failed: ${String(error)}`);
  }
  try {
    // Preserve local metadata validation before any bytes reach the peer.
    const metadata = encodeIpcFrame(0n, C2_IPC_FLAG_HANDSHAKE, encodeClientIpcHandshake(connection.memory));
    await withinIpcConnectDeadline(deadline, () => connection.write(encodeClientIpcHello()));
    await acceptServerIpcVersion(connection, deadline);
    await withinIpcConnectDeadline(deadline, () => connection.write(metadata));
    const frame = await withinIpcConnectDeadline(deadline, () => readIpcFrame(connection));
    if (frame.requestId !== 0n || frame.flags !== (C2_IPC_FLAG_RESPONSE | C2_IPC_FLAG_HANDSHAKE)) {
      throw new C2IpcTransportError("C-Two IPC server metadata response has invalid request id or flags.");
    }
    const handshake = decodeServerIpcHandshake(frame.payload);
    connection.memory?.finishHandshake((handshake.capabilityFlags & C2_IPC_CAP_SHM_RESPONSE) !== 0 ? handshake.shmPrefix : null);
    remainingIpcConnectMs(deadline);
    return { connection, handshake, writeChain: Promise.resolve(), closing: false, reading: undefined, nextRequestId: 1n, activeCalls: new Map() };
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

async function acceptServerIpcVersion(connection: C2IpcConnection, deadline?: C2IpcConnectDeadline): Promise<void> {
  // Never let an unnegotiated length select a read or payload allocation.
  const header = requireUint8Array(await withinIpcConnectDeadline(deadline, () => connection.readExactly(16)), "C-Two IPC version preamble header");
  if (header.byteLength !== 16) {
    throw new C2IpcTransportError("C-Two IPC version preamble header is truncated.");
  }
  const envelope = new DataView(header.buffer, header.byteOffset, header.byteLength);
  if (envelope.getUint32(0, true) !== 21 || envelope.getBigUint64(4, true) !== 0n || envelope.getUint32(12, true) !== 0x06) {
    throw new C2IpcTransportError("C-Two IPC version preamble has invalid length, request id or flags.");
  }
  const body = requireUint8Array(await withinIpcConnectDeadline(deadline, () => connection.readExactly(9)), "C-Two IPC version preamble body");
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

function encodeIpcOpenMetadata(route: C2IpcRouteInfo, methodIndex: number): Uint8Array {
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

async function writeIpcBuddyCallRequestFrame(open: C2IpcOpenConnection, firstFrame: (block: C2IpcShmBlock) => C2ByteArray, payload: C2ByteArray): Promise<void> {
  const memory = open.connection.memory!;
  const request = memory.prepareRequest(payload);
  let handedOff = false;
  try {
    handedOff = true;
    await writeIpcFrame(open, () => firstFrame(request.block), request);
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
async function writeIpcFrame(open: C2IpcOpenConnection, frame?: C2ByteArray | (() => C2ByteArray), request?: C2IpcPreparedRequest, control = false): Promise<void> {
  const write = open.writeChain.then(async () => {
    if (open.discarded) throw new C2IpcConnectionFailure("C-Two IPC connection is closed.");
    const memory = open.connection.memory;
    let batch: C2IpcMemoryWrite | undefined;
    let started = false;
    try {
      const readyFrame = typeof frame === "function" ? frame() : frame;
      batch = memory?.beginWrite(readyFrame === undefined ? 2 : control ? 1 : 0, request);
      if (readyFrame === undefined && batch === undefined) return;
      const bytes = batch === undefined ? readyFrame! : concatUint8Arrays([batch.prefix, readyFrame ?? new Uint8Array()]);
      batch?.start();
      started = true;
      try { await open.connection.write(bytes); }
      catch (error) {
        try { batch?.complete(false); } catch { /* the stream is already uncertain */ }
        throw new C2IpcConnectionFailure(`C-Two IPC frame write failed: ${String(error)}`);
      }
      try { batch?.complete(true); }
      catch (error) { throw new C2IpcConnectionFailure(`C-Two IPC frame write completion failed: ${String(error)}`); }
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

async function readIpcResponseFrame(open: C2IpcOpenConnection, expectedRequestId?: bigint): Promise<C2IpcFrame> {
  let finish!: () => void;
  open.reading = new Promise<void>(resolve => { finish = resolve; });
  try {
    for (;;) {
      const memory = open.connection.memory;
      if (open.closing && memory?.tryClose()) {
        await open.connection.close();
        throw new C2IpcTransportError("C-Two IPC connection is closed.");
      }
      let frame: C2IpcFrame;
      try { frame = await readIpcFrame(open.connection); }
      catch (error) {
        if (error instanceof C2IpcLocalPayloadFailure) {
          const retired = error.requestId > 0n && error.requestId < open.nextRequestId && !open.activeCalls.has(error.requestId);
          const lateFlags = C2_IPC_FLAG_RESPONSE | C2_IPC_FLAG_RPC_RECORD | C2_IPC_FLAG_CHUNKED;
          if (retired && (error.flags === lateFlags || error.flags === (lateFlags | C2_IPC_FLAG_BUDDY))
            && error.prefix?.[0] === C2_IPC_CALL_END && error.payloadLength > 1) {
            if ((error.flags & C2_IPC_FLAG_BUDDY) !== 0) {
              if (error.prefix.byteLength !== C2_IPC_BUDDY_PAYLOAD_BYTES + 1 || error.payloadLength !== C2_IPC_BUDDY_PAYLOAD_BYTES + 1) throw error;
              await discardIpcShmFrame(open, {requestId: error.requestId, flags: error.flags, payload: error.prefix});
            }
            continue;
          }
          if ((error.flags & C2_IPC_FLAG_BUDDY) !== 0 && error.prefix !== undefined) {
            await discardIpcShmFrame(open, {requestId: error.requestId, flags: error.flags, payload: error.prefix}, true);
          }
          throw error;
        }
        throw error;
      }
      if (frame.flags === C2_IPC_FLAG_RPC_RECORD || frame.flags === (C2_IPC_FLAG_RPC_RECORD | C2_IPC_FLAG_RESPONSE)) {
        if (frame.payload[0] === C2_IPC_RELEASE) {
          if (frame.requestId !== 0n || frame.payload.byteLength < 10 || frame.payload[1] === 0 || frame.payload[1] > 64 || frame.payload.byteLength !== 2 + frame.payload[1] * 8) {
            throw new C2IpcConnectionFailure("Invalid IPC 16 RELEASE record.");
          }
          if (memory === undefined) throw new C2IpcConnectionFailure("Release on a connection without native memory.");
          try { memory.receiveRelease(encodeIpcFrame(frame.requestId, frame.flags, frame.payload)); }
          catch {
            memory.deactivate();
            open.onDeactivated?.();
          }
          continue;
        }
      }
      if (frame.requestId !== 0n && !open.activeCalls.has(frame.requestId)) {
        if (frame.requestId >= open.nextRequestId) {
          if ((frame.flags & C2_IPC_FLAG_BUDDY) !== 0) await discardIpcShmFrame(open, frame);
          throw new C2IpcConnectionFailure(`C-Two IPC response names future request id ${frame.requestId}.`);
        }
        await discardRetiredIpcCallEnd(open, frame);
        continue;
      }
      if (expectedRequestId !== undefined && frame.requestId !== expectedRequestId) {
        if ((frame.flags & C2_IPC_FLAG_BUDDY) !== 0) await discardIpcShmFrame(open, frame);
        throw new C2IpcConnectionFailure(`C-Two IPC response request id ${frame.requestId} did not match request id ${expectedRequestId}.`);
      }
      if (!open.closing) return frame;
      if ((frame.flags & C2_IPC_FLAG_BUDDY) !== 0) {
        await discardIpcShmFrame(open, frame);
      }
    }
  } finally { open.reading = undefined; finish(); }
}

async function discardIpcShmFrame(open: C2IpcOpenConnection, frame: C2IpcFrame, partial = false): Promise<void> {
  const memory = open.connection.memory;
  if (memory === undefined || (open.handshake.capabilityFlags & C2_IPC_CAP_SHM_RESPONSE) === 0) {
    throw new C2IpcConnectionFailure("C-Two IPC server sent an SHM reply without the acknowledged SHM response capability.");
  }
  let block: C2IpcShmBlock;
  const offset = ipcResponseStorageOffset(frame);
  if (offset === undefined) throw new C2IpcConnectionFailure("C-Two IPC SHM response has no valid storage position.");
  const storage = frame.payload.subarray(offset);
  if (partial && storage.byteLength < C2_IPC_BUDDY_PAYLOAD_BYTES) return;
  if (storage.byteLength !== C2_IPC_BUDDY_PAYLOAD_BYTES) throw new C2IpcConnectionFailure("C-Two IPC SHM descriptor has invalid length.");
  try { block = decodeIpcBuddyPayload(storage); }
  catch (error) {
    if (error instanceof C2IpcConnectionFailure) throw error;
    throw new C2IpcConnectionFailure(`C-Two IPC SHM descriptor is invalid: ${String(error)}`);
  }
  if ((storage[14] & ~C2_IPC_BUDDY_FLAG_DEDICATED) !== 0 || block.byteLength === 0) throw new C2IpcConnectionFailure("C-Two IPC SHM descriptor has invalid size or kind.");
  let response: unknown;
  try { response = memory.admitResponse(block); }
  catch { await writeIpcFrame(open); return; } // Native admission queued its release.
  memory.discardResponse(response);
  await writeIpcFrame(open);
}

function ipcResponseStorageOffset(frame: C2IpcFrame): number | undefined {
  if ((frame.flags & C2_IPC_FLAG_RESPONSE) === 0 || (frame.flags & C2_IPC_FLAG_RPC_RECORD) === 0 || frame.payload[0] !== C2_IPC_CALL_END) return undefined;
  const first = (frame.flags & C2_IPC_FLAG_FIRST_CHUNK) !== 0;
  if ((frame.flags & C2_IPC_FLAG_CHUNKED) !== 0 && !first) return 1;
  return first ? 10 : 2;
}

async function discardRetiredIpcCallEnd(open: C2IpcOpenConnection, frame: C2IpcFrame): Promise<void> {
  const base = C2_IPC_FLAG_RESPONSE | C2_IPC_FLAG_RPC_RECORD;
  const allowed = base | C2_IPC_FLAG_BUDDY | C2_IPC_FLAG_CHUNKED | C2_IPC_FLAG_FIRST_CHUNK;
  if ((frame.flags & base) !== base || (frame.flags & ~allowed) !== 0 || frame.payload[0] !== C2_IPC_CALL_END
    || ((frame.flags & C2_IPC_FLAG_FIRST_CHUNK) !== 0 && (frame.flags & C2_IPC_FLAG_CHUNKED) === 0)) {
    throw new C2IpcConnectionFailure("Retired IPC 16 CALL_END has invalid flags or kind.");
  }
  const continuation = (frame.flags & C2_IPC_FLAG_CHUNKED) !== 0 && (frame.flags & C2_IPC_FLAG_FIRST_CHUNK) === 0;
  let offset = 1;
  if (!continuation) {
    if (frame.payload.byteLength < 2) throw new C2IpcConnectionFailure("Retired IPC 16 CALL_END status is missing.");
    const status = frame.payload[1];
    if (status === C2_IPC_REPLY_STATUS_ERROR || status === C2_IPC_REPLY_STATUS_NOT_EXECUTED) {
      if (frame.flags !== base) throw new C2IpcConnectionFailure("Retired IPC 16 CALL_END error has invalid storage.");
      decodeIpcCanonicalError(frame.payload.subarray(2));
      return;
    }
    if (status !== C2_IPC_REPLY_STATUS_SUCCESS) throw new C2IpcConnectionFailure("Retired IPC 16 CALL_END status is invalid.");
    offset = 2;
    if ((frame.flags & C2_IPC_FLAG_FIRST_CHUNK) !== 0) {
      if (frame.payload.byteLength < 11) throw new C2IpcConnectionFailure("Retired IPC 16 CALL_END first chunk is truncated.");
      offset = 10;
    }
  }
  const chunked = (frame.flags & C2_IPC_FLAG_CHUNKED) !== 0;
  if ((frame.flags & C2_IPC_FLAG_BUDDY) !== 0) {
    await discardIpcShmFrame(open, frame);
    if ((frame.flags & C2_IPC_FLAG_FIRST_CHUNK) !== 0) {
      const total = readU64LEBigInt(frame.payload, 2);
      const size = BigInt(readU32LE(frame.payload, offset + 10));
      if (size >= total) throw new C2IpcConnectionFailure("Retired IPC 16 CALL_END first chunk is not multi-part.");
    }
  } else {
    const size = frame.payload.byteLength - offset;
    if (chunked && size <= 0) throw new C2IpcConnectionFailure("Retired IPC 16 CALL_END chunk is empty.");
    if ((frame.flags & C2_IPC_FLAG_FIRST_CHUNK) !== 0 && BigInt(size) >= readU64LEBigInt(frame.payload, 2)) {
      throw new C2IpcConnectionFailure("Retired IPC 16 CALL_END first chunk is not multi-part.");
    }
  }
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
  try {
    let header: Uint8Array;
    try { header = requireUint8Array(await connection.readExactly(C2_IPC_FRAME_HEADER_BYTES), "C-Two IPC frame header"); }
    catch (error) {
      const failure = error as { c2LocalAllocationFailure?: unknown; c2ReadPrefix?: Uint8Array };
      if (failure?.c2LocalAllocationFailure !== true || failure.c2ReadPrefix?.byteLength !== C2_IPC_FRAME_HEADER_BYTES) throw error;
      header = failure.c2ReadPrefix;
    }
    if (header.byteLength !== C2_IPC_FRAME_HEADER_BYTES) {
      throw new C2IpcTransportError(`C-Two IPC frame header read returned ${header.byteLength} bytes.`);
    }
    const view = new DataView(header.buffer, header.byteOffset, header.byteLength);
    const totalLen = view.getUint32(0, true);
    const requestId = view.getBigUint64(4, true);
    const flags = view.getUint32(12, true);
    if (totalLen < C2_IPC_FRAME_BODY_HEADER_BYTES) {
      throw new C2IpcTransportError(`C-Two IPC frame total_len ${totalLen} is smaller than the fixed body header.`);
    }
    const payloadLen = totalLen - C2_IPC_FRAME_BODY_HEADER_BYTES;
    if (payloadLen > C2_MAX_RESPONSE_PAYLOAD_BYTES) {
      throw new C2IpcTransportError(`C-Two IPC frame payload length ${payloadLen} must be no greater than ${C2_MAX_RESPONSE_PAYLOAD_BYTES}.`);
    }
    let payload: Uint8Array;
    try {
      payload = payloadLen === 0
        ? new Uint8Array()
        : requireUint8Array(await connection.readExactly(payloadLen), "C-Two IPC frame payload");
    } catch (error) {
      const failure = error as { c2LocalAllocationFailure?: unknown; c2ReadPrefix?: Uint8Array };
      if (failure?.c2LocalAllocationFailure === true
        && (flags === C2_IPC_FLAG_RPC_RECORD || flags === (C2_IPC_FLAG_RPC_RECORD | C2_IPC_FLAG_RESPONSE))
        && requestId === 0n && failure.c2ReadPrefix?.[0] === C2_IPC_RELEASE
        && failure.c2ReadPrefix.byteLength === payloadLen) {
        // The reserved metadata contains this whole control payload. Process
        // its release records normally before reading the dispatched reply.
        payload = failure.c2ReadPrefix;
      } else if (failure?.c2LocalAllocationFailure === true && (flags & C2_IPC_FLAG_RESPONSE) !== 0) {
        throw new C2IpcLocalPayloadFailure(requestId, flags, payloadLen, error,
          failure.c2ReadPrefix);
      } else throw error;
    }
    if (payload.byteLength !== payloadLen) {
      throw new C2IpcTransportError(`C-Two IPC frame payload read returned ${payload.byteLength} bytes, expected ${payloadLen}.`);
    }
    return {
      requestId,
      flags,
      payload,
    };
  } catch (error) {
    if (error instanceof C2IpcLocalPayloadFailure) throw error;
    throw new C2IpcConnectionFailure(`C-Two IPC frame read failed: ${String(error)}`);
  }
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
  const byteLength = readU32LE(payload, 10);
  const generation = readU32LE(payload, 2);
  const dedicated = (flags & C2_IPC_BUDDY_FLAG_DEDICATED) !== 0;
  const loanId = readU64LEBigInt(payload, 15);
  if (loanId === 0n) throw new C2IpcConnectionFailure("C-Two IPC response loan ID is zero.");
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

async function readIpcCallEnd(open: C2IpcOpenConnection, first: C2IpcFrame, requestId: bigint): Promise<C2IpcCallResponse> {
  const baseFlags = C2_IPC_FLAG_RESPONSE | C2_IPC_FLAG_RPC_RECORD;
  const allowedFlags = baseFlags | C2_IPC_FLAG_BUDDY | C2_IPC_FLAG_CHUNKED | C2_IPC_FLAG_FIRST_CHUNK;
  if (first.requestId !== requestId || (first.flags & baseFlags) !== baseFlags || (first.flags & ~allowedFlags) !== 0 || first.payload[0] !== C2_IPC_CALL_END) {
    if ((first.flags & C2_IPC_FLAG_BUDDY) !== 0) await discardIpcShmFrame(open, first);
    throw new C2IpcConnectionFailure("C-Two IPC response is not an IPC 16 CALL_END record.");
  }
  if ((first.flags & C2_IPC_FLAG_FIRST_CHUNK) !== 0 && (first.flags & C2_IPC_FLAG_CHUNKED) === 0) {
    if ((first.flags & C2_IPC_FLAG_BUDDY) !== 0) await discardIpcShmFrame(open, first);
    throw new C2IpcConnectionFailure("C-Two IPC FIRST_CHUNK requires CHUNKED.");
  }
  if (first.payload.byteLength < 2) throw new C2IpcConnectionFailure("C-Two IPC CALL_END status is missing.");
  const status = first.payload[1];
  if (status !== C2_IPC_REPLY_STATUS_SUCCESS) {
    if (status !== C2_IPC_REPLY_STATUS_ERROR && status !== C2_IPC_REPLY_STATUS_NOT_EXECUTED) {
      if ((first.flags & C2_IPC_FLAG_BUDDY) !== 0) await discardIpcShmFrame(open, first);
      throw new C2IpcConnectionFailure(`C-Two IPC CALL_END status ${status} is invalid.`);
    }
    if (first.flags !== baseFlags) {
      if ((first.flags & C2_IPC_FLAG_BUDDY) !== 0) await discardIpcShmFrame(open, first);
      throw new C2IpcConnectionFailure("C-Two IPC CALL_END error must be inline and whole.");
    }
    const payload = first.payload.subarray(2);
    const canonical = decodeIpcCanonicalError(payload);
    if (status === C2_IPC_REPLY_STATUS_NOT_EXECUTED) throw new C2IpcCallRejectedError(payload.slice(), canonical);
    throw new C2CrmMethodError(payload.slice());
  }
  if ((first.flags & C2_IPC_FLAG_CHUNKED) === 0) {
    if ((first.flags & C2_IPC_FLAG_BUDDY) !== 0) {
      const value = await admitIpcResponseShm(open, first.payload.subarray(2));
      return { kind: "shm", block: value.block, response: value.response };
    }
    return { kind: "inline", payload: first.payload.subarray(2) };
  }
  if ((first.flags & C2_IPC_FLAG_FIRST_CHUNK) === 0 || first.payload.byteLength < 11) {
    throw new C2IpcConnectionFailure("C-Two IPC CALL_END is missing its first chunk metadata.");
  }
  const totalBytes = readU64LEBigInt(first.payload, 2);
  const firstPart = await copyIpcResponsePart(open, first, 10);
  const pieceBytes = BigInt(firstPart.byteLength);
  if (pieceBytes === 0n || pieceBytes >= totalBytes) throw new C2IpcConnectionFailure("C-Two IPC first chunk must start a multi-part value.");
  if (totalBytes > BigInt(Number.MAX_SAFE_INTEGER)) throw new C2IpcTransportError("C-Two IPC response exceeds local allocation range.");
  let assembled: Uint8Array;
  try { assembled = new Uint8Array(Number(totalBytes)); }
  catch (error) { throw new C2IpcTransportError(`C-Two IPC response allocation failed: ${String(error)}`); }
  assembled.set(firstPart);
  let copied = pieceBytes;
  while (copied < totalBytes) {
    const next = await readIpcResponseFrame(open, requestId);
    if (next.requestId !== requestId || (next.flags & ~(baseFlags | C2_IPC_FLAG_CHUNKED | C2_IPC_FLAG_BUDDY)) !== 0
      || (next.flags & (baseFlags | C2_IPC_FLAG_CHUNKED)) !== (baseFlags | C2_IPC_FLAG_CHUNKED)
      || next.payload[0] !== C2_IPC_CALL_END) {
      if ((next.flags & C2_IPC_FLAG_BUDDY) !== 0) await discardIpcShmFrame(open, next);
      throw new C2IpcConnectionFailure("C-Two IPC CALL_END continuation has invalid flags or kind.");
    }
    const part = await copyIpcResponsePart(open, next, 1);
    const expected = totalBytes - copied < pieceBytes ? totalBytes - copied : pieceBytes;
    if (BigInt(part.byteLength) !== expected) throw new C2IpcTransportError("C-Two IPC CALL_END continuation has invalid fixed piece length.");
    assembled.set(part, Number(copied));
    copied += BigInt(part.byteLength);
  }
  return { kind: "inline", payload: assembled };
}

async function admitIpcResponseShm(open: C2IpcOpenConnection, storage: Uint8Array): Promise<{ block: C2IpcShmBlock; response: unknown }> {
  if (storage.byteLength !== C2_IPC_BUDDY_PAYLOAD_BYTES) throw new C2IpcConnectionFailure("C-Two IPC SHM descriptor has invalid length.");
  const block = decodeIpcBuddyPayload(storage);
  if ((storage[14] & ~C2_IPC_BUDDY_FLAG_DEDICATED) !== 0 || block.byteLength === 0) {
    throw new C2IpcConnectionFailure("C-Two IPC SHM descriptor has invalid size or kind.");
  }
  const memory = open.connection.memory;
  if (memory === undefined || (open.handshake.capabilityFlags & C2_IPC_CAP_SHM_RESPONSE) === 0) {
    throw new C2IpcConnectionFailure("C-Two IPC server sent an SHM reply without the acknowledged SHM response capability.");
  }
  // Native admission owns the release obligation even when mapping fails.
  try { return { block, response: memory.admitResponse(block) }; }
  catch (error) {
    try { await writeIpcFrame(open); } catch { /* preserve native admission error */ }
    throw error;
  }
}

async function copyIpcResponsePart(open: C2IpcOpenConnection, frame: C2IpcFrame, offset: number): Promise<Uint8Array> {
  if ((frame.flags & C2_IPC_FLAG_BUDDY) === 0) {
    const part = frame.payload.subarray(offset);
    if (part.byteLength === 0) throw new C2IpcConnectionFailure("C-Two IPC chunk is empty.");
    return part;
  }
  const { block, response } = await admitIpcResponseShm(open, frame.payload.subarray(offset));
  return await readIpcShmSuccessPayload<Uint8Array>(response, block.byteLength, undefined, open.connection.memory!, () => writeIpcFrame(open));
}

function decodeIpcCanonicalError(payload: Uint8Array): C2IpcCanonicalError {
  if (payload.byteLength <= 4 || payload[0] !== 0x43 || payload[1] !== 0x32 || payload[2] !== 0x45 || payload[3] !== 0x31) {
    throw new C2IpcConnectionFailure("C-Two IPC CALL_END error must contain C2E1.");
  }
  let decoded: unknown;
  try { decoded = JSON.parse(utf8Decode(payload.subarray(4), "C2E1 error")); }
  catch (error) { throw new C2IpcConnectionFailure(`C-Two IPC CALL_END C2E1 is invalid: ${String(error)}`); }
  if (typeof decoded !== "object" || decoded === null || Array.isArray(decoded)) throw new C2IpcConnectionFailure("C-Two IPC CALL_END C2E1 envelope is invalid.");
  const record = decoded as Record<string, unknown>;
  const details = record.details;
  if (record.version !== 1 || !Number.isSafeInteger(record.code) || (record.code as number) < 0 || (record.code as number) > 0xffff
    || typeof record.name !== "string" || record.name.length === 0 || typeof record.message !== "string"
    || typeof details !== "object" || details === null || Array.isArray(details)
    || Object.values(details).some(value => typeof value !== "string")
    || (C2_IPC_ERROR_NAMES[record.code as number] !== undefined && C2_IPC_ERROR_NAMES[record.code as number] !== record.name)) {
    throw new C2IpcConnectionFailure("C-Two IPC CALL_END C2E1 envelope is invalid.");
  }
  return record as unknown as C2IpcCanonicalError;
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
