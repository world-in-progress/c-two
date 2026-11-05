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
  call(routeName: string, contract: C2ContractIdentity, method: string, encode: () => C2ByteArray): Promise<C2ResponsePayload>;
}

export interface C2StreamEncodedClientTransport extends C2EncodedClientTransport {
  stream<T>(routeName: string, contract: C2ContractIdentity, method: string, encode: () => C2ByteArray, decode: (bytes: Uint8Array) => T, hold?: false): C2ResponseStream<T>;
  stream<T>(routeName: string, contract: C2ContractIdentity, method: string, encode: () => C2ByteArray, decode: (bytes: Uint8Array) => T, hold: true): C2ResponseStream<C2HeldResult<T>>;
}

export interface C2HttpRelayEncodedTransport<Payload extends C2ResponsePayload = C2ByteArray> extends C2StreamEncodedClientTransport {
  call(routeName: string, contract: C2ContractIdentity, method: string, encode: () => C2ByteArray): Promise<Payload>;
}

export interface C2RelayAwareHttpEncodedTransport<Payload extends C2ResponsePayload = C2ByteArray> extends C2StreamEncodedClientTransport {
  call(routeName: string, contract: C2ContractIdentity, method: string, encode: () => C2ByteArray): Promise<Payload>;
  close(): Promise<void>;
}

export interface C2NativeBuffer extends Uint8Array { release(): void; }

export interface C2StreamReceipt {
  readonly completed: boolean;
  readonly error: Error | null;
  readonly completeItems: bigint | null;
}

export interface C2NativeCall {
  chargeInput(byteLength: bigint | number): Promise<void>;
  dispatch(bytes: Uint8Array): Promise<void>;
  next(): Promise<C2NativeBuffer | null>;
  cancel(): void;
  observe(timeoutMs: number | null): Promise<C2StreamReceipt>;
  checkDeadline(): void;
  close(): void;
}

export interface C2NativeEndpointContext {
  prepare(routeName: string, contract: C2RouteContractIdentity): Promise<{
    serverId: string; serverInstanceId: string; routeUid: string; connectionId: bigint | string;
  }>;
  beginCall(routeName: string, method: string, serverStream: boolean, timeoutMs: number | null): C2NativeCall;
  deactivate(): void;
  close(): Promise<void>;
}

export interface C2IpcNativeEndpointOptions {
  readonly connectTimeoutMs?: number;
  readonly expectedServerIdentity?: C2ServerIdentity;
  readonly expectedRouteToken?: C2RouteToken;
}

export type C2IpcConnect = (address: string, timeoutMs?: number, nativeOptions?: C2IpcNativeEndpointOptions) => Promise<C2NativeEndpointContext>;

export interface C2ResponseStream<T> extends AsyncIterableIterator<T> {
  close(timeoutMs?: number | null): Promise<C2StreamReceipt>;
}

export interface C2IpcTransportOptions<Payload extends C2ResponsePayload = C2ByteArray> {
  readonly connect: C2IpcConnect;
  readonly connectTimeoutMs?: number;
  readonly callTimeoutMs?: number | null;
  readonly responsePayloadAllocator?: C2ResponsePayloadAllocator<Payload>;
  readonly expectedServerIdentity?: C2ServerIdentity;
  readonly expectedRouteToken?: C2RouteToken;
  readonly observationPath?: "DirectIpc" | "RelayAwareLocalIpc";
  readonly observe?: C2TransportObserver | undefined;
}

export interface C2IpcEncodedTransport<Payload extends C2ResponsePayload = C2ByteArray> extends C2StreamEncodedClientTransport {
  prepare(routeName: string, contract: C2ContractIdentity): Promise<void>;
  call(routeName: string, contract: C2ContractIdentity, method: string, encode: () => C2ByteArray): Promise<Payload>;
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

export class C2HttpRelayError extends Error {
  constructor(readonly status: number, readonly body: string) {
    super(`C-Two HTTP relay call failed with status ${status}: ${body}`);
    this.name = "C2HttpRelayError";
    Object.setPrototypeOf(this, C2HttpRelayError.prototype);
  }
  get retrySafe(): boolean {
    return isRetryableRouteError(this);
  }
}

export class C2HttpTransportError extends Error {
  constructor(message: string) {
    super(`C-Two HTTP transport error: ${message}`);
    this.name = "C2HttpTransportError";
    Object.setPrototypeOf(this, C2HttpTransportError.prototype);
  }
  get retrySafe(): boolean { return false; }
}

export class C2CrmMethodError extends Error {
  constructor(readonly payload: Uint8Array) {
    super("CRM method error");
    this.name = "C2CrmMethodError";
    Object.setPrototypeOf(this, C2CrmMethodError.prototype);
  }
  get retrySafe(): boolean { return false; }
}

export interface C2NativeErrorDetails {
  readonly message: string;
  readonly code?: number;
  readonly canonicalName?: string;
  readonly details?: Readonly<Record<string, string>>;
  readonly transportPhase?: "pre_dispatch" | "dispatch_uncertain";
  readonly category?: string;
  readonly localProtocolVersion?: number;
  readonly peerProtocolVersion?: number;
  readonly ioOperation?: string;
  readonly ioKind?: string;
  readonly rawOsError?: number;
}

export class C2IpcTransportError extends Error {
  readonly category: string | null;
  readonly localProtocolVersion: number | null;
  readonly peerProtocolVersion: number | null;
  readonly transportPhase: "pre_dispatch" | "dispatch_uncertain" | null;
  readonly code: number | null;
  readonly canonicalName: string | null;
  readonly details: Readonly<Record<string, string>>;
  get retrySafe(): boolean { return this.transportPhase === "pre_dispatch"; }
  constructor(message: string, readonly nativeDetails?: C2NativeErrorDetails) {
    super(message);
    this.name = "C2IpcTransportError";
    this.category = nativeDetails?.category ?? null;
    this.localProtocolVersion = nativeDetails?.localProtocolVersion ?? null;
    this.peerProtocolVersion = nativeDetails?.peerProtocolVersion ?? null;
    this.transportPhase = nativeDetails?.transportPhase ?? null;
    this.code = nativeDetails?.code ?? null;
    this.canonicalName = nativeDetails?.canonicalName ?? null;
    this.details = nativeDetails?.details ?? {};
  }
}

export class C2UnsupportedCallModeError extends Error {
  readonly nativeDetails: C2NativeErrorDetails = {
    message: "server streams require direct IPC",
    code: 716,
    canonicalName: "UnsupportedCallMode",
    transportPhase: "pre_dispatch",
  };
  constructor() {
    super("C-Two server streams require direct IPC.");
    this.name = "UnsupportedCallMode";
  }
  get retrySafe(): boolean { return true; }
}

export class C2CoreError extends Error {
  readonly code: number;
  readonly canonicalName: string | null;
  readonly details: Readonly<Record<string, string>>;
  readonly transportPhase: "pre_dispatch" | "dispatch_uncertain" | null;
  get retrySafe(): boolean { return this.transportPhase === "pre_dispatch"; }
  constructor(message: string, readonly nativeDetails: C2NativeErrorDetails) {
    super(message);
    this.name = nativeDetails.canonicalName ?? "C2CoreError";
    this.code = nativeDetails.code as number;
    this.canonicalName = nativeDetails.canonicalName ?? null;
    this.details = nativeDetails.details ?? {};
    this.transportPhase = nativeDetails.transportPhase ?? null;
  }
}

function projectNativeError(error: unknown): Error {
  if (error instanceof C2IpcTransportError) return error;
  if (error instanceof Error) {
    const nativeDetails = (error as Error & { nativeDetails?: C2NativeErrorDetails }).nativeDetails;
    if (nativeDetails !== undefined) {
      if (nativeDetails.code !== undefined && nativeDetails.transportPhase === undefined) {
        const projected = new C2CoreError(error.message, nativeDetails);
        projected.cause = error;
        return projected;
      }
      const projected = new C2IpcTransportError(error.message, nativeDetails);
      projected.cause = error;
      return projected;
    }
    return error;
  }
  return new C2IpcTransportError(String(error));
}

interface C2StreamState {
  call: C2NativeCall | undefined;
  cancelled: boolean;
  collected: boolean;
  callClosed: boolean;
  done: boolean;
  nextPending: boolean;
}

function closeCollectedCall(state: C2StreamState): void {
  if (state.call !== undefined && !state.callClosed) {
    state.callClosed = true;
    state.call.close();
  }
}

const streamFinalizer = new FinalizationRegistry<C2StreamState>(state => {
  state.cancelled = true;
  state.collected = true;
  state.call?.cancel();
  closeCollectedCall(state);
});

function incompleteStreamReceipt(): C2StreamReceipt {
  return { completed: false, error: null, completeItems: null };
}

async function waitForCloseReady<T>(ready: Promise<T>, timeoutMs: number | null, startedAt: number): Promise<T | undefined> {
  if (timeoutMs === null) return ready;
  if (timeoutMs - (performance.now() - startedAt) <= 0) return undefined;
  let timer: ReturnType<typeof setTimeout> | undefined;
  try {
    const expired = new Promise<undefined>(resolve => {
      const observe = (): void => {
        const remaining = timeoutMs - (performance.now() - startedAt);
        if (remaining <= 0) resolve(undefined);
        else timer = setTimeout(observe, Math.min(remaining, 2_147_483_647));
      };
      observe();
    });
    return await Promise.race([ready, expired]);
  } finally {
    if (timer !== undefined) clearTimeout(timer);
  }
}

async function readNativeStreamNext<T>(state: C2StreamState, ready: Promise<C2NativeCall>, decode: (bytes: Uint8Array) => T, hold: boolean): Promise<IteratorResult<T>> {
    if (state.done || state.cancelled) return { done: true, value: undefined };
    if (state.nextPending) throw new Error("C-Two response stream next is already pending.");
    state.nextPending = true;
    try {
      const call = await ready;
      if (state.cancelled) return { done: true, value: undefined };
      call.checkDeadline();
      const response = await call.next();
      if (response === null) {
        state.done = true;
        return { done: true, value: undefined };
      }
      if (state.collected) { response.release(); return {done:true,value:undefined}; }
      let value: T | undefined;
      try {
        value = decode(response);
        call.checkDeadline();
      } catch (error) {
        tryReleaseStreamValue(value);
        response.release();
        throw error;
      }
      if (hold) return { done: false, value: createHeldResult(value, true, response, true) as T };
      try { response.release(); }
      catch (error) { tryReleaseStreamValue(value); throw error; }
      return { done: false, value };
    } catch (error) {
      state.done = true;
      const projected = projectNativeError(error);
      if (!state.callClosed && !((projected instanceof C2IpcTransportError || projected instanceof C2CoreError) && projected.code === 715)) {
        state.call?.cancel();
      }
      throw projected;
    } finally {
      state.nextPending = false;
    }
  }


class NativeC2ResponseStream<T> implements C2ResponseStream<T> {
  private readonly state: C2StreamState = { call: undefined, cancelled: false, collected: false, callClosed: false, done: false, nextPending: false };
  private readonly ready: Promise<C2NativeCall>;
  private finalReceipt: C2StreamReceipt | undefined;

  constructor(start: (state: C2StreamState) => Promise<C2NativeCall>, private readonly decode: (bytes: Uint8Array) => T, private readonly hold: boolean) {
    const state = this.state;
    this.ready = start(state).then(call => {
      state.call = call;
      if (state.cancelled && !state.callClosed) call.cancel();
      if (state.collected) closeCollectedCall(state);
      return call;
    });
    void this.ready.catch(() => undefined);
    streamFinalizer.register(this, state, this);
  }

  [Symbol.asyncIterator](): AsyncIterableIterator<T> { return this; }

  next(): Promise<IteratorResult<T>> {
    return readNativeStreamNext(this.state, this.ready, this.decode, this.hold);
  }

  async return(): Promise<IteratorResult<T>> {
    this.state.cancelled = true;
    this.state.call?.cancel();
    return { done: true, value: undefined };
  }

  async close(timeoutMs: number | null = 30_000): Promise<C2StreamReceipt> {
    if (this.finalReceipt !== undefined) return this.finalReceipt;
    if (timeoutMs !== null && (!Number.isSafeInteger(timeoutMs) || timeoutMs < 0)) {
      throw new TypeError("C-Two stream close timeoutMs must be a non-negative safe integer or null.");
    }
    const startedAt = performance.now();
    this.state.cancelled = true;
    const existing = this.state.call;
    existing?.cancel();
    const call = existing ?? await waitForCloseReady(this.ready, timeoutMs, startedAt);
    if (call === undefined) return incompleteStreamReceipt();
    if (call !== existing) call.cancel();
    const remaining = timeoutMs === null ? null : Math.max(0, Math.ceil(timeoutMs - (performance.now() - startedAt)));
    const receipt = await call.observe(remaining);
    if (receipt.completed) {
      streamFinalizer.unregister(this);
      call.close();
      this.state.callClosed = true;
      this.state.call = undefined;
      this.state.done = true;
    }
    const projected = { completed: receipt.completed, error: receipt.error === null ? null : projectNativeError(receipt.error), completeItems: receipt.completeItems };
    if (receipt.completed) this.finalReceipt = projected;
    return projected;
  }
}

interface C2DeferredStreamState<T> {
  stream?: C2ResponseStream<T>;
  cancelled: boolean;
  collected: boolean;
  dispose?: () => void;
}

const deferredStreamFinalizer = new FinalizationRegistry<C2DeferredStreamState<unknown>>(state => {
  state.cancelled = true;
  state.collected = true;
  if (state.stream !== undefined) void state.stream.return?.();
  state.dispose?.();
});

async function readDeferredStreamNext<T>(state: C2DeferredStreamState<T>, ready: Promise<C2ResponseStream<T>>): Promise<IteratorResult<T>> {
    if (state.cancelled) return { done: true, value: undefined };
    const stream = await ready;
    if (state.cancelled) return { done: true, value: undefined };
    const result = await stream.next();
    if (result.done) state.dispose?.();
    return result;
  }

class DeferredC2ResponseStream<T> implements C2ResponseStream<T> {
  private readonly state: C2DeferredStreamState<T> = { cancelled: false, collected: false };
  private readonly ready: Promise<C2ResponseStream<T>>;
  constructor(start: (state: C2DeferredStreamState<T>) => Promise<C2ResponseStream<T>>) {
    const state = this.state;
    this.ready = start(state).then(stream => {
      state.stream = stream;
      if (state.cancelled) void stream.return?.();
      if (state.collected) state.dispose?.();
      return stream;
    });
    void this.ready.catch(() => undefined);
    deferredStreamFinalizer.register(this, state, this);
  }
  [Symbol.asyncIterator](): AsyncIterableIterator<T> { return this; }
  next(): Promise<IteratorResult<T>> {
    return readDeferredStreamNext(this.state, this.ready);
  }

  async return(): Promise<IteratorResult<T>> {
    this.state.cancelled = true;
    if (this.state.stream !== undefined) void this.state.stream.return?.();
    return { done: true, value: undefined };
  }
  async close(timeoutMs: number | null = 30_000): Promise<C2StreamReceipt> {
    if (timeoutMs !== null && (!Number.isSafeInteger(timeoutMs) || timeoutMs < 0)) {
      throw new TypeError("C-Two stream close timeoutMs must be a non-negative safe integer or null.");
    }
    const startedAt = performance.now();
    this.state.cancelled = true;
    if (this.state.stream !== undefined) void this.state.stream.return?.();
    const stream = this.state.stream ?? await waitForCloseReady(this.ready, timeoutMs, startedAt);
    if (stream === undefined) return incompleteStreamReceipt();
    const remaining = timeoutMs === null ? null : Math.max(0, Math.ceil(timeoutMs - (performance.now() - startedAt)));
    const receipt = await stream.close(remaining);
    if (receipt.completed) {
      deferredStreamFinalizer.unregister(this);
      this.state.dispose?.();
    }
    return receipt;
  }
}

export function createIpcEncodedTransport<Payload extends C2ResponsePayload = C2ByteArray>(address: string, options: C2IpcTransportOptions<Payload>): C2IpcEncodedTransport<Payload> {
  if (typeof address !== "string" || !address.startsWith("ipc://") || address.length <= 6) {
    throw new TypeError("C-Two IPC address must be an ipc:// address.");
  }
  if (typeof options?.connect !== "function") throw new TypeError("C-Two IPC connect must be a native connector.");
  if (options.connectTimeoutMs !== undefined && (!Number.isSafeInteger(options.connectTimeoutMs) || options.connectTimeoutMs <= 0)) {
    throw new TypeError("C-Two IPC connectTimeoutMs must be a positive safe integer.");
  }
  const allocator = normalizeResponsePayloadAllocator(options.responsePayloadAllocator, "C-Two IPC responsePayloadAllocator");
  const observationPath = options.observationPath ?? "DirectIpc";
  const observe = normalizeTransportObserver(options.observe, "C-Two IPC observe");
  const callTimeoutMs = options.callTimeoutMs ?? null;
  let endpointPromise: Promise<C2NativeEndpointContext> | undefined;
  let closed = false;
  let requests = 0;
  let responses = 0;
  const endpoint = (): Promise<C2NativeEndpointContext> => {
    if (closed) throw new C2IpcTransportError("C-Two IPC transport is deactivated.");
    if (endpointPromise === undefined) {
      const nativeOptions: C2IpcNativeEndpointOptions = {
        ...(options.connectTimeoutMs === undefined ? {} : { connectTimeoutMs: options.connectTimeoutMs }),
        ...(options.expectedServerIdentity === undefined ? {} : { expectedServerIdentity: options.expectedServerIdentity }),
        ...(options.expectedRouteToken === undefined ? {} : { expectedRouteToken: options.expectedRouteToken }),
      };
      const pending = Promise.resolve().then(() => options.connect(address, options.connectTimeoutMs, nativeOptions)).catch(error => {
        if (endpointPromise === pending) endpointPromise = undefined;
        throw projectNativeError(error);
      });
      endpointPromise = pending;
    }
    return endpointPromise;
  };
  const prepare = async (routeName: string, contract: C2ContractIdentity) => {
    const routeContract = requireRouteContractIdentity(contract);
    const native = await endpoint();
    const binding = await native.prepare(routeName, routeContract).catch(error => { throw projectNativeError(error); });
    return { native, binding };
  };
  return {
    async prepare(routeName, contract): Promise<void> { await prepare(routeName, contract); },
    async call(routeName, contract, method, encode): Promise<Payload> {
      const { native, binding } = await prepare(routeName, contract);
      const call = native.beginCall(routeName, method, false, callTimeoutMs);
      requests += 1;
      try {
        const request = requireUint8Array(encode(), "encoded input");
        call.checkDeadline();
        await call.chargeInput(request.byteLength);
        await call.dispatch(request);
        const response = await call.next();
        if (response === null) throw new C2IpcTransportError("C-Two unary call ended without a response.");
        responses += 1;
        let result: Payload;
        try { result = copyResponsePayloadToAllocator(response, allocator); }
        finally { if (allocator !== undefined) response.release(); }
        return observeTransportResponse(result, observe, {
          path: observationPath, routeName, routeUid: binding.routeUid,
          serverId: binding.serverId, serverInstanceId: binding.serverInstanceId,
          ipcAddress: address, requests, responses,
        });
      } catch (error) { throw projectNativeError(error); }
      finally { call.close(); }
    },
    stream(routeName, contract, method, encode, decode, hold = false) {
      return new NativeC2ResponseStream(async state => {
        const { native } = await prepare(routeName, contract);
        const call = native.beginCall(routeName, method, true, callTimeoutMs);
        state.call = call;
        try {
          if (state.collected) {
            call.cancel();
            closeCollectedCall(state);
            return call;
          }
          if (state.cancelled) { call.cancel(); return call; }
          const request = requireUint8Array(encode(), "encoded stream input");
          await call.chargeInput(request.byteLength);
          if (state.cancelled) { call.cancel(); return call; }
          await call.dispatch(request);
          return call;
        } catch (error) {
          const projected = projectNativeError(error);
          if (!state.callClosed && !((projected instanceof C2IpcTransportError || projected instanceof C2CoreError) && projected.code === 715)) call.cancel();
          if (state.collected) closeCollectedCall(state);
          throw projected;
        }
      }, decode, hold);
    },
    async close(): Promise<void> {
      closed = true;
      const native = await endpointPromise?.catch(() => undefined);
      await native?.close();
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
    stream(): never { throw new C2UnsupportedCallModeError(); },
    async call(routeName: string, contract: C2ContractIdentity, method: string, encode: () => C2ByteArray): Promise<Payload> {
      requireRouteNamePathValue(routeName);
      requireMethodPathValue(method);
      const request = encodeHttpUnaryInput(encode, callTimeoutMs);
      const requestPayload = request.payload;
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
        request.remaining(),
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
    stream<T>(routeName: string, contract: C2ContractIdentity, method: string, encode: () => C2ByteArray, decode: (bytes: Uint8Array) => T, hold = false) {
      return new DeferredC2ResponseStream<T | C2HeldResult<T>>(async state => {
        const routeContract = requireRouteContractIdentity(contract);
        const routes = await resolveRelayRoutesCached(
          fetchImpl, normalizedAnchorUrl, routeName, routeContract, headers,
          routeCache, routeCacheTtlMs, resolveTimeoutMs, false,
        );
        const route = selectLocalIpcRoute(routes, normalizedAnchorUrl, ipcOptions, new Set());
        if (route === undefined || ipcOptions === undefined) throw new C2UnsupportedCallModeError();
        const transport = createIpcEncodedTransport(route.ipcAddress as string, {
          ...ipcOptions,
          expectedServerIdentity: { serverId: route.serverId as string, serverInstanceId: route.serverInstanceId as string },
          expectedRouteToken: { routeUid: route.routeUid },
          observationPath: "RelayAwareLocalIpc", observe,
        });
        state.dispose = () => { void transport.close().catch(error => console.warn("Warning: local stream transport cleanup failed:", error)); };
        try { await transport.prepare(routeName, routeContract); }
        catch (error) {
          await transport.close().catch(cleanupError => console.warn("Warning: local stream transport cleanup failed:", cleanupError));
          throw error;
        }
        const stream = hold
          ? transport.stream(routeName, routeContract, method, encode, decode, true)
          : transport.stream(routeName, routeContract, method, encode, decode);
        if (state.cancelled) void stream.return?.();
        return stream;
      });
    },
    async call(routeName: string, contract: C2ContractIdentity, method: string, encode: () => C2ByteArray): Promise<Payload> {
      requireRouteNamePathValue(routeName);
      requireMethodPathValue(method);
      const routeContract = requireRouteContractIdentity(contract);
      const cacheKey = relayRouteCacheKey(routeName, routeContract);
      let lastError: unknown;
      let firstLocalFailure: unknown;
      let httpRequest: ReturnType<typeof encodeHttpUnaryInput> | undefined;
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
              Object.defineProperty(primaryError, "cleanupError", { value: closeError });
              throw primaryError;
            }
            if (localCandidateFailureIsTerminal(error)) {
              throw error;
            }
            firstLocalFailure ??= error;
            lastError = error;
            excludedLocalRouteTokens.add(localTokenKey);
            invalidateRelayRouteCache(routeCache, cacheKey);
            const distinctRoutes = routes.filter(
              (route) => !excludedLocalRouteTokens.has(routeTokenKey(route)),
            );
            if (distinctRoutes.length === 0) {
              throw samePathFallbackDeniedError(routeName, localRoute, firstLocalFailure);
            }
            continue;
          }
          return await callPreparedLocalIpcAndClose(
            localTransport,
            routeName,
            routeContract,
            method,
            encode,
          );
        }
        const httpRoutes = routes.filter(
          (route) => !excludedLocalRouteTokens.has(routeTokenKey(route)),
        );
        if (httpRoutes.length === 0 && excludedLocalRouteTokens.size > 0) {
          throw samePathFallbackDeniedError(routeName, routes[0], firstLocalFailure);
        }
        const orderedRoutes = orderRelayRoutes(httpRoutes, currentRelayByCacheKey.get(cacheKey), excludedRoutes);
        if (orderedRoutes.length === 0) {
          if (lastError instanceof Error) {
            throw lastError;
          }
          throw resourceNotFoundError(routeName);
        }
        httpRequest ??= encodeHttpUnaryInput(encode, callTimeoutMs);
        const requestPayload = httpRequest.payload;
        let staleRoute = false;
        for (const route of orderedRoutes) {
          const relayUrl = normalizeRelayBaseUrl(route.relayUrl, "C-Two resolved relay URL");
          if (requestPayload.byteLength > route.maxPayloadSize) {
            lastError = payloadTooLargeError(routeName, requestPayload.byteLength, route.maxPayloadSize);
            continue;
          }
          let businessPostStarted = false;
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
            businessPostStarted = true;
            const result = await callResolvedHttpRoute(
              fetchImpl,
              relayUrl,
              routeName,
              routeContract,
              route,
              method,
              requestPayload,
              headers,
              httpRequest.remaining(),
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
            if (businessPostStarted) throw error;
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

function encodeHttpUnaryInput(encode: () => C2ByteArray, timeoutMs: number): { payload: C2ByteArray; remaining(): number } {
  const startedAt = performance.now();
  const remaining = (): number => {
    if (timeoutMs === 0) return 0;
    const budget = timeoutMs - (performance.now() - startedAt);
    if (budget <= 0) {
      throw new C2CoreError("call deadline exceeded before dispatch", {
        message: "call deadline exceeded before dispatch", code: 715,
        canonicalName: "CallDeadlineExceeded", transportPhase: "pre_dispatch",
      });
    }
    return Math.ceil(budget);
  };
  const payload = requireUint8Array(encode(), "encoded input");
  remaining();
  return { payload, remaining };
}

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

function tryReleaseStreamValue(value: unknown): void {
  try { releaseHeldValue(value); }
  catch (error) { console.warn("Warning: stream value cleanup failed:", error); }
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
  encode: () => C2ByteArray,
): Promise<Payload> {
  let response: Payload;
  try {
    response = await transport.call(routeName, contract, method, encode);
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

function normalizeRelayAwareIpcOptions<Payload extends C2ResponsePayload>(value: C2IpcTransportOptions<Payload> | undefined): C2IpcTransportOptions<Payload> | undefined {
  if (value === undefined) return undefined;
  if (typeof value !== "object" || value === null || typeof value.connect !== "function") {
    throw new TypeError("C-Two relay-aware ipc requires native connect.");
  }
  if (value.expectedServerIdentity !== undefined || value.expectedRouteToken !== undefined || value.observationPath !== undefined || value.observe !== undefined) {
    throw new TypeError("C-Two relay-aware ipc expected route facts and observation are owned by relay resolution.");
  }
  return value;
}

function selectLocalIpcRoute<Payload extends C2ResponsePayload>(routes: readonly C2RelayRouteInfo[], anchorUrl: string, ipcOptions: C2IpcTransportOptions<Payload> | undefined, excludedRouteTokens: ReadonlySet<string>): C2RelayRouteInfo | undefined {
  if (ipcOptions === undefined || !relayAnchorAllowsLocalIpc(anchorUrl)) return undefined;
  return routes.find(route => route.ipcAddress !== undefined && route.serverId !== undefined && route.serverInstanceId !== undefined && !excludedRouteTokens.has(routeTokenKey(route)));
}

function relayAnchorAllowsLocalIpc(anchorUrl: string): boolean {
  const hostname = new URL(anchorUrl).hostname.toLowerCase();
  return hostname === "localhost" || hostname.startsWith("127.") || hostname === "::1" || hostname === "[::1]";
}

function routeTokenKey(route: C2RouteToken): string { return JSON.stringify([route.routeUid]); }

function localCandidateFailureIsTerminal(error: unknown): boolean {
  return error instanceof C2IpcTransportError && (
    error.category === "protocol_version_mismatch" || error.category === "runtime_retired" ||
    error.code === 709 || error.code === 710 || error.code === 713 || error.code === 715
  );
}

function samePathFallbackDeniedError(
  routeName: string,
  route: C2RelayRouteInfo,
  firstCause?: unknown,
): C2HttpRelayError {
  const denied = new C2HttpRelayError(
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
  if (firstCause !== undefined) denied.cause = firstCause;
  return denied;
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
