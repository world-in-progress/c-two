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

export interface C2ServerIdentity {
  readonly serverId: string;
  readonly serverInstanceId: string;
}

export interface C2TransportObservation {
  readonly path: C2ObservedPath;
  readonly routeName: string;
  readonly requests: number;
  readonly responses: number;
  readonly relayUrl?: string;
  readonly ipcAddress?: string | undefined;
  readonly serverId?: string | undefined;
  readonly serverInstanceId?: string | undefined;
  readonly connectionId?: string | undefined;
}

export type C2TransportObserver = (observation: C2TransportObservation) => void;

export interface C2CallIdOptions {
  readonly callId?: string;
}

export interface C2CallOptions {
  readonly timeoutMs?: number | null;
  readonly metadata?: Readonly<Record<string, string>> | null;
}

function mergeCallOptions(parent: C2CallOptions, overlay: C2CallOptions): C2CallOptions {
  const timeout = overlay.timeoutMs === undefined ? parent.timeoutMs : overlay.timeoutMs;
  const metadata = overlay.metadata === undefined
    ? parent.metadata
    : overlay.metadata === null ? undefined : mergeCallMetadata(parent.metadata, overlay.metadata);
  return Object.freeze({
    ...(timeout === undefined ? {} : { timeoutMs: validateViewTimeout(timeout) }),
    ...(metadata == null ? {} : { metadata }),
  });
}

function mergeCallMetadata(
  parent: C2CallOptions["metadata"],
  overlay: Readonly<Record<string, string>>,
): Readonly<Record<string, string>> {
  if (typeof overlay !== "object" || overlay === null || Array.isArray(overlay)) {
    throw new TypeError("Call metadata must be a record of strings.");
  }
  const merged = { ...parent, ...overlay };
  const entries = Object.entries(merged);
  if (entries.length > 64) throw new TypeError("Call metadata has more than 64 entries.");

  let totalBytes = 0;
  for (const [key, value] of entries) {
    if (key.length < 1 || key.length > 64 || /[^a-z0-9_.-]/.test(key)) {
      throw new TypeError(`Invalid call metadata key ${JSON.stringify(key)}.`);
    }
    if (typeof value !== "string" || value.length > 4096 || /[^ -~]/.test(value) ||
        value.startsWith(" ") || value.endsWith(" ")) {
      throw new TypeError(`Invalid call metadata value for ${key}.`);
    }
    totalBytes += key.length + value.length;
  }
  if (totalBytes > 16_384) throw new TypeError("Call metadata exceeds 16384 bytes.");
  return Object.freeze(merged);
}

function validateViewTimeout(timeout: number | null): number | null {
  if (timeout !== null && (!Number.isSafeInteger(timeout) || timeout < 0)) {
    throw new TypeError("C-Two call timeoutMs must be a non-negative safe integer or null.");
  }
  return timeout;
}

export interface C2CallOutcome {
  readonly status: "in_flight" | "succeeded" | "failed" | "unknown";
  readonly method_name: string | null;
  readonly result_available: boolean;
  readonly result_unavailable_reason: "not_retained" | "evicted" | null;
  readonly error_code: number | null;
  readonly error_details_available: boolean;
}

type C2UnaryRequest =
  | { readonly encode: () => C2ByteArray; readonly callId?: string | undefined }
  | { readonly encode?: undefined; readonly callId: string };

export interface C2EncodedClientTransport {
  call(routeName: string, contract: C2ContractIdentity, method: string, encode: () => C2ByteArray, options?: C2CallIdOptions, view?: C2CallOptions): Promise<C2ResponsePayload>;
  recoverCall(routeName: string, contract: C2ContractIdentity, method: string, callId: string, view?: C2CallOptions): Promise<C2ResponsePayload>;
  queryCall(routeName: string, contract: C2ContractIdentity, callId: string, view?: C2CallOptions): Promise<C2CallOutcome>;
}

export interface TopicSubscribeOptions {
  readonly start?: "latest" | "next";
  readonly onLag?: "skip" | "error";
  readonly callId?: never;
  /** One deadline for acquisition, READY and deliveries. Inherits the client view when omitted. */
  readonly timeoutMs?: number | null;
}

export interface TopicDelivery<T> {
  readonly value: T;
  readonly seq: bigint;
  readonly dropped: bigint;
}

export interface C2TopicSubscription<T> extends AsyncIterableIterator<TopicDelivery<T>> {
  readonly startSeq: bigint;
  readonly topic: string;
  next(): Promise<IteratorResult<TopicDelivery<T>, undefined>>;
  cancel(): void;
  close(timeoutMs?: number | null): Promise<C2StreamReceipt>;
}

export interface C2ClientTransport extends C2EncodedClientTransport {
  subscribe<T>(routeName: string, contract: C2ContractIdentity, topic: string, decode: (bytes: Uint8Array) => T, options?: TopicSubscribeOptions, hold?: false, view?: C2CallOptions): Promise<C2TopicSubscription<T>>;
  subscribe<T>(routeName: string, contract: C2ContractIdentity, topic: string, decode: (bytes: Uint8Array) => T, options: TopicSubscribeOptions | undefined, hold: true, view?: C2CallOptions): Promise<C2TopicSubscription<C2HeldResult<T>>>;
}

export interface C2StreamEncodedClientTransport extends C2EncodedClientTransport {
  stream<T>(routeName: string, contract: C2ContractIdentity, method: string, encode: () => C2ByteArray, decode: (bytes: Uint8Array) => T, hold?: false, view?: C2CallOptions): C2ResponseStream<T>;
  stream<T>(routeName: string, contract: C2ContractIdentity, method: string, encode: () => C2ByteArray, decode: (bytes: Uint8Array) => T, hold: true, view?: C2CallOptions): C2ResponseStream<C2HeldResult<T>>;
}

export interface C2HttpRelayEncodedTransport<Payload extends C2ResponsePayload = C2ByteArray> extends C2StreamEncodedClientTransport, C2ClientTransport {
  call(routeName: string, contract: C2ContractIdentity, method: string, encode: () => C2ByteArray, options?: C2CallIdOptions, view?: C2CallOptions): Promise<Payload>;
  recoverCall(routeName: string, contract: C2ContractIdentity, method: string, callId: string, view?: C2CallOptions): Promise<Payload>;
}

export interface C2RelayAwareHttpEncodedTransport<Payload extends C2ResponsePayload = C2ByteArray> extends C2StreamEncodedClientTransport, C2ClientTransport {
  call(routeName: string, contract: C2ContractIdentity, method: string, encode: () => C2ByteArray, options?: C2CallIdOptions, view?: C2CallOptions): Promise<Payload>;
  recoverCall(routeName: string, contract: C2ContractIdentity, method: string, callId: string, view?: C2CallOptions): Promise<Payload>;
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
  dispatch(bytes: Uint8Array): Promise<C2NativeCallTarget>;
  next(): Promise<C2NativeBuffer | null>;
  cancel(): void;
  observe(timeoutMs: number | null): Promise<C2StreamReceipt>;
  checkDeadline(): void;
  close(): void;
}

export interface C2NativeCallTarget {
  readonly path: C2ObservedPath;
  readonly serverId: string | null;
  readonly serverInstanceId: string | null;
  readonly connectionId: bigint | string | null;
}

export interface C2NativeSubscription extends C2TopicSubscription<C2NativeBuffer> {
  checkDeadline(): void;
}

export interface C2NativeEndpointContext {
  prepare(routeName: string, contract: C2RouteContractIdentity, timeoutMs?: number): Promise<unknown>;
  connectionSnapshot(routeName: string): C2ConnectionSnapshot | null;
  beginCall(routeName: string, method: string, serverStream: boolean, timeoutMs: number | null, callId?: undefined, metadata?: Readonly<Record<string, string>>): C2NativeCall;
  beginCall(routeName: string, method: string, serverStream: false, timeoutMs: number | null, callId?: string, metadata?: Readonly<Record<string, string>>): C2NativeCall;
  recoverCall(routeName: string, method: string, callId: string, timeoutMs: number | null, metadata?: Readonly<Record<string, string>>): Promise<C2NativeBuffer>;
  queryCall(routeName: string, callId: string, timeoutMs: number | null, metadata?: Readonly<Record<string, string>>): Promise<C2CallOutcome>;
  openSubscription(routeName: string, topic: string, options: Omit<TopicSubscribeOptions, "timeoutMs"> & { readonly metadata?: Readonly<Record<string, string>> }, timeoutMs: number | null): Promise<C2NativeSubscription>;
  deactivate(): void;
  close(): Promise<void>;
}

export interface C2ConnectionSnapshot extends C2ServerIdentity {
  readonly connectionId: bigint | string;
  readonly endReason: "local_close" | "peer_close_message" | "peer_eof" | "connection_reset" | "protocol_error" | "io_error" | null;
}

export interface C2IpcNativeEndpointOptions {
  readonly mode?: "relay_aware" | "explicit_relay";
  readonly connectTimeoutMs?: number;
  readonly expectedServerIdentity?: C2ServerIdentity;
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
  readonly observationPath?: "DirectIpc" | "RelayAwareLocalIpc";
  readonly observe?: C2TransportObserver | undefined;
}

export interface C2IpcEncodedTransport<Payload extends C2ResponsePayload = C2ByteArray> extends C2StreamEncodedClientTransport, C2ClientTransport {
  prepare(routeName: string, contract: C2ContractIdentity): Promise<void>;
  call(routeName: string, contract: C2ContractIdentity, method: string, encode: () => C2ByteArray, options?: C2CallIdOptions, view?: C2CallOptions): Promise<Payload>;
  recoverCall(routeName: string, contract: C2ContractIdentity, method: string, callId: string, view?: C2CallOptions): Promise<Payload>;
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
  cancel?(reason?: unknown): Promise<unknown> | void;
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
}

export interface C2AbortSignal {
  readonly aborted: boolean;
  readonly reason?: unknown;
  addEventListener(type: "abort", listener: () => void, options?: { readonly once?: boolean }): void;
  removeEventListener?(type: "abort", listener: () => void): void | undefined;
}

export type C2Fetch = (
  input: string,
  init: {
    readonly method: "GET" | "POST";
    readonly redirect?: "error";
    readonly headers: Readonly<Record<string, string>>;
    readonly body?: C2ByteArray;
    readonly signal?: C2AbortSignal | undefined;
  },
) => Promise<C2FetchResponse>;

export interface C2HttpRelayTransportOptions<Payload extends C2ResponsePayload = C2ByteArray> {
  /** Native connector for topic subscriptions. Relay-aware mode can reuse ipc.connect. */
  readonly connect?: C2IpcConnect;
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
  readonly ipc?: C2IpcTransportOptions<Payload>;
}

interface C2RelayRouteInfo {
  readonly name: string;
  readonly relayUrl: string;
  readonly ipcAddress?: string | undefined;
  readonly serverId?: string | undefined;
  readonly serverInstanceId?: string | undefined;
  readonly crmNs: string;
  readonly crmName: string;
  readonly crmVer: string;
  readonly abiHash: string;
  readonly signatureHash: string;
}

const C2_STRICT_UTF8_DECODER = new TextDecoder("utf-8", { fatal: true, ignoreBOM: true });
const C2_LOSSY_UTF8_DECODER = new TextDecoder("utf-8");
const C2_HTTP_CONTROL_CODES: Readonly<Record<number, readonly number[]>> = {
  400: [713], 403: [724], 404: [701], 406: [716], 409: [709, 710, 719, 702, 721, 723, 714],
  410: [708], 413: [717], 429: [717], 502: [702, 713, 718], 503: [702, 707, 712, 717, 718, 715],
};

function isWellFormedUnicodeString(value: string): boolean {
  for (let index = 0; index < value.length; index += 1) {
    const code = value.charCodeAt(index);
    if (code >= 0xd800 && code <= 0xdbff) {
      if (index + 1 >= value.length) return false;
      const low = value.charCodeAt(index + 1);
      if (low < 0xdc00 || low > 0xdfff) return false;
      index += 1;
    } else if (code >= 0xdc00 && code <= 0xdfff) {
      return false;
    }
  }
  return true;
}

function decodeHttpErrorBody(raw: string | Uint8Array): { readonly body: string; readonly validUtf8: boolean } {
  if (typeof raw === "string") {
    return { body: raw, validUtf8: isWellFormedUnicodeString(raw) };
  }
  try {
    return { body: C2_STRICT_UTF8_DECODER.decode(raw), validUtf8: true };
  } catch {
    return { body: C2_LOSSY_UTF8_DECODER.decode(raw), validUtf8: false };
  }
}

function ownDetail(details: Readonly<Record<string, string>>, key: string): string | undefined {
  return Object.prototype.hasOwnProperty.call(details, key) ? details[key] : undefined;
}

function resolveHttpCallRelayErrorPhase(
  status: number,
  decoded: { readonly body: string; readonly validUtf8: boolean },
): "pre_dispatch" | "dispatch_uncertain" {
  if (!decoded.validUtf8 || C2_HTTP_CONTROL_CODES[status] === undefined) {
    return "dispatch_uncertain";
  }
  const canonical = parseCanonicalRelayError(decoded.body);
  if (canonical === undefined || !C2_HTTP_CONTROL_CODES[status]?.includes(canonical.code)) {
    return "dispatch_uncertain";
  }
  const dispatchPhase = ownDetail(canonical.details, "dispatch_phase");
  const transportPhase = ownDetail(canonical.details, "transport_phase");
  if (dispatchPhase !== undefined && dispatchPhase !== "pre_dispatch") {
    return "dispatch_uncertain";
  }
  if (transportPhase !== undefined && transportPhase !== "pre_dispatch") {
    return "dispatch_uncertain";
  }
  if (status === 403 || status === 404 || dispatchPhase === "pre_dispatch") {
    return "pre_dispatch";
  }
  return "dispatch_uncertain";
}

export class C2HttpRelayError extends Error {
  declare readonly callId?: string;
  readonly status: number;
  readonly body: string;
  readonly transportPhase: "pre_dispatch" | "dispatch_uncertain";
  readonly #status: number;
  readonly #decodedBody: string;
  readonly #validUtf8: boolean;
  constructor(
    status: number,
    body: string | Uint8Array,
    transportPhase?: "pre_dispatch" | "dispatch_uncertain",
    predecoded?: { readonly body: string; readonly validUtf8: boolean },
  ) {
    const decoded = predecoded ?? decodeHttpErrorBody(body);
    super(`C-Two HTTP relay call failed with status ${status}: ${decoded.body}`);
    this.name = "C2HttpRelayError";
    Object.setPrototypeOf(this, C2HttpRelayError.prototype);
    this.status = status;
    this.body = decoded.body;
    this.transportPhase = transportPhase ?? resolveHttpCallRelayErrorPhase(status, decoded);
    this.#status = status;
    this.#decodedBody = decoded.body;
    this.#validUtf8 = decoded.validUtf8;
  }
  get hasVerifiedUtf8Body(): boolean {
    return this.#validUtf8;
  }
  get retrySafe(): boolean {
    return isRetryableRouteError(this);
  }
  static _postDispatchCopy(source: C2HttpRelayError): C2HttpRelayError {
    const copy = new C2HttpRelayError(
      source.#status,
      source.#decodedBody,
      "dispatch_uncertain",
      { body: source.#decodedBody, validUtf8: source.#validUtf8 },
    );
    copy.cause = source;
    return copy;
  }
}

export class C2HttpTransportError extends Error {
  declare readonly callId?: string;
  readonly cannotConnect: boolean;
  readonly transportPhase: "pre_dispatch" | "dispatch_uncertain";
  readonly #rawMessage: string;
  constructor(message: string, cause?: unknown, transportPhase?: "pre_dispatch" | "dispatch_uncertain") {
    super(`C-Two HTTP transport error: ${message}`, cause === undefined ? undefined : { cause });
    this.name = "C2HttpTransportError";
    Object.setPrototypeOf(this, C2HttpTransportError.prototype);
    this.cannotConnect = httpConnectFailure(cause);
    this.transportPhase = transportPhase ?? (this.cannotConnect ? "pre_dispatch" : "dispatch_uncertain");
    this.#rawMessage = message;
  }
  get retrySafe(): boolean { return this.transportPhase === "pre_dispatch"; }
  static _postDispatchCopy(source: C2HttpTransportError): C2HttpTransportError {
    const copy = new C2HttpTransportError(source.#rawMessage, undefined, "dispatch_uncertain");
    copy.cause = source;
    return copy;
  }
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
  declare readonly callId?: string;
  readonly category: string | null;
  readonly localProtocolVersion: number | null;
  readonly peerProtocolVersion: number | null;
  readonly transportPhase: "pre_dispatch" | "dispatch_uncertain" | null;
  readonly code: number | null;
  readonly canonicalName: string | null;
  readonly details: Readonly<Record<string, string>>;
  get retrySafe(): boolean {
    return this.transportPhase === "pre_dispatch" && permitsRouteRetry(this.code, this.details);
  }
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
  declare readonly callId?: string;
  readonly code: number;
  readonly canonicalName: string | null;
  readonly details: Readonly<Record<string, string>>;
  readonly transportPhase: "pre_dispatch" | "dispatch_uncertain" | null;
  get retrySafe(): boolean {
    return this.transportPhase === "pre_dispatch" && permitsRouteRetry(this.code, this.details);
  }
  constructor(message: string, readonly nativeDetails: C2NativeErrorDetails) {
    super(message);
    this.name = nativeDetails.canonicalName ?? "C2CoreError";
    this.code = nativeDetails.code as number;
    this.canonicalName = nativeDetails.canonicalName ?? null;
    this.details = nativeDetails.details ?? {};
    this.transportPhase = nativeDetails.transportPhase ?? null;
  }
}

export class C2RouteConflictError extends C2CoreError {
  constructor(message: string, details: Readonly<Record<string, string>> = {}) {
    super(message, {
      message,
      code: 719,
      canonicalName: "RouteConflict",
      transportPhase: "pre_dispatch",
      details,
    });
  }
}

function formatCaughtError(error: unknown): string {
  try {
    return String(error);
  } catch {
    return "<unprintable error>";
  }
}

function projectNativeError(error: unknown): Error {
  if (error instanceof C2CoreError) return error;
  if (error instanceof C2IpcTransportError && error.code === 719 && error.canonicalName === "RouteConflict" && error.transportPhase === "pre_dispatch") {
    const projected = new C2RouteConflictError(error.message, error.details);
    projected.cause = error;
    return projected;
  }
  if (error instanceof C2IpcTransportError) return error;
  if (error instanceof Error) {
    const nativeDetails = (error as Error & { nativeDetails?: C2NativeErrorDetails }).nativeDetails;
    if (nativeDetails !== undefined) {
      if (nativeDetails.code === 719 && nativeDetails.canonicalName === "RouteConflict" && nativeDetails.transportPhase === "pre_dispatch") {
        const projected = new C2RouteConflictError(error.message, nativeDetails.details);
        projected.cause = error;
        return projected;
      }
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
  return new C2IpcTransportError(formatCaughtError(error));
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

function tryCancelStreamCall(state: C2StreamState): void {
  if (state.callClosed || state.call === undefined) return;
  try {
    state.call.cancel();
  } catch {
    // Best-effort cancellation cleanup must never overwrite a primary stream error.
  }
}

async function readNativeStreamNext<T>(
  state: C2StreamState,
  ready: Promise<C2NativeCall>,
  decode: (bytes: Uint8Array) => T,
  hold: boolean,
  onTransportFailure?: (error: unknown) => void,
): Promise<IteratorResult<T>> {
    if (state.done || state.cancelled) return { done: true, value: undefined };
    if (state.nextPending) throw new Error("C-Two response stream next is already pending.");
    state.nextPending = true;
    let readyFailed = false;
    let postItemCallbackFailed = false;
    try {
      let call: C2NativeCall;
      try {
        call = await ready;
      } catch (error) {
        readyFailed = true;
        throw error;
      }
      if (state.cancelled) return { done: true, value: undefined };
      call.checkDeadline();
      const response = await call.next();
      if (response === null) {
        state.done = true;
        return { done: true, value: undefined };
      }
      if (state.collected) { response.release(); return {done:true,value:undefined}; }
      let value: T | undefined;
      let decodeFailed = false;
      try {
        try {
          value = decode(response);
        } catch (decodeError) {
          decodeFailed = true;
          throw decodeError;
        }
        call.checkDeadline();
      } catch (error) {
        tryReleaseStreamValue(value);
        let releaseFailure: unknown;
        let hasReleaseFailure = false;
        try {
          response.release();
        } catch (relErr) {
          releaseFailure = relErr;
          hasReleaseFailure = true;
        }
        if (decodeFailed) {
          postItemCallbackFailed = true;
          state.done = true;
          tryCancelStreamCall(state);
          throw sanitizePostDispatchCallbackError(error);
        }
        if (hasReleaseFailure) {
          // Preserve primary deadline failure over cleanup failure.
          void releaseFailure;
        }
        throw error;
      }
      if (hold) return { done: false, value: createHeldResult(value, true, response, true) as T };
      try {
        response.release();
      } catch (releaseError) {
        tryReleaseStreamValue(value);
        postItemCallbackFailed = true;
        state.done = true;
        tryCancelStreamCall(state);
        throw sanitizePostDispatchCallbackError(releaseError);
      }
      return { done: false, value };
    } catch (error) {
      if (postItemCallbackFailed) {
        throw error;
      }
      state.done = true;
      const projected = projectNativeError(error);
      if (!readyFailed) {
        onTransportFailure?.(projected);
      }
      if (!((projected instanceof C2IpcTransportError || projected instanceof C2CoreError) && projected.code === 715)) {
        tryCancelStreamCall(state);
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

  constructor(
    start: (state: C2StreamState) => Promise<C2NativeCall>,
    private readonly decode: (bytes: Uint8Array) => T,
    private readonly hold: boolean,
    private readonly onTransportFailure?: (error: unknown) => void,
  ) {
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
    return readNativeStreamNext(this.state, this.ready, this.decode, this.hold, this.onTransportFailure);
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
    let receipt: C2StreamReceipt;
    try {
      receipt = await call.observe(remaining);
    } catch (error) {
      const projectedErr = projectNativeError(error);
      this.onTransportFailure?.(projectedErr);
      throw projectedErr;
    }
    if (receipt.completed) {
      streamFinalizer.unregister(this);
      call.close();
      this.state.callClosed = true;
      this.state.call = undefined;
      this.state.done = true;
    }
    const projectedError = receipt.error === null ? null : projectNativeError(receipt.error);
    if (projectedError !== null) {
      this.onTransportFailure?.(projectedError);
    }
    const projected = { completed: receipt.completed, error: projectedError, completeItems: receipt.completeItems };
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

class C2CallBudget {
  private readonly startedAt = performance.now();
  private readonly timeoutMs: number | null;
  readonly metadata: Readonly<Record<string, string>> | undefined;
  readonly explicitTimeout: boolean;
  constructor(defaultTimeout: number | null, view?: C2CallOptions) {
    this.timeoutMs = view?.timeoutMs === undefined ? defaultTimeout : validateViewTimeout(view.timeoutMs);
    this.explicitTimeout = view?.timeoutMs !== undefined;
    this.metadata = view?.metadata == null ? undefined : mergeCallMetadata(undefined, view.metadata);
  }
  remaining(): number | null {
    if (this.timeoutMs === null) return null;
    const remaining = this.timeoutMs - (performance.now() - this.startedAt);
    if (remaining <= 0) throw callDeadlineError("pre_dispatch");
    return Math.ceil(remaining);
  }
  expired(): boolean {
    return this.timeoutMs !== null && performance.now() - this.startedAt >= this.timeoutMs;
  }
  async wait<T>(pending: Promise<T>): Promise<T> {
    // Native setup may have exhausted the budget before returning its Promise.
    // Observe it before checking the deadline, retaining either outcome for waiters.
    const observed: Promise<PromiseSettledResult<T>> = pending.then(
      value => ({ status: "fulfilled", value }),
      reason => ({ status: "rejected", reason }),
    );
    const remaining = this.remaining();
    const result = remaining === null ? await observed : await waitForCloseReady(observed, remaining, performance.now());
    if (result === undefined) throw callDeadlineError("pre_dispatch");
    if (result.status === "rejected") throw result.reason;
    this.remaining();
    return result.value;
  }
  stageLimit(limit: number): number {
    const remaining = this.remaining();
    return remaining === null ? limit : limit === 0 ? remaining : Math.min(remaining, limit);
  }
}

function callDeadlineError(phase: "pre_dispatch" | "dispatch_uncertain", message = "call deadline exceeded"): C2CoreError {
  return new C2CoreError(message, {
    message, code: 715,
    canonicalName: "CallDeadlineExceeded", transportPhase: phase,
  });
}

interface C2IpcTargetTransport<Payload extends C2ResponsePayload> extends C2IpcEncodedTransport<Payload> {
  acquire(routeName: string, contract: C2ContractIdentity, budget: C2CallBudget): Promise<void>;
  snapshot(routeName: string): C2ConnectionSnapshot | null;
  queryAcquired(routeName: string, callId: string, budget: C2CallBudget): Promise<C2CallOutcome>;
  callAcquired(routeName: string, method: string, operation: C2UnaryRequest, budget: C2CallBudget, onTransportFailure?: (error: unknown) => void): Promise<Payload>;
  streamAcquired<T>(routeName: string, method: string, encode: () => C2ByteArray, decode: (bytes: Uint8Array) => T, hold: boolean, budget: C2CallBudget, cancelled?: boolean, onTransportFailure?: (error: unknown) => void): C2ResponseStream<T | C2HeldResult<T>>;
}

function nativeTopicSubscribe(
  acquire: (route: string, contract: C2ContractIdentity, budget: C2CallBudget) => Promise<C2NativeEndpointContext>,
): C2ClientTransport["subscribe"] {
  function subscribe<T>(route: string, contract: C2ContractIdentity, topic: string, decode: (bytes: Uint8Array) => T, options?: TopicSubscribeOptions, hold?: false, view?: C2CallOptions): Promise<C2TopicSubscription<T>>;
  function subscribe<T>(route: string, contract: C2ContractIdentity, topic: string, decode: (bytes: Uint8Array) => T, options: TopicSubscribeOptions | undefined, hold: true, view?: C2CallOptions): Promise<C2TopicSubscription<C2HeldResult<T>>>;
  async function subscribe<T>(route: string, contract: C2ContractIdentity, topic: string, decode: (bytes: Uint8Array) => T, options: TopicSubscribeOptions = {}, hold = false, view?: C2CallOptions): Promise<C2TopicSubscription<T | C2HeldResult<T>>> {
    if ("callId" in options) throw new TypeError("Topic subscriptions do not accept a callId.");
    const { timeoutMs, ...policy } = options;
    const budget = new C2CallBudget(null, timeoutMs === undefined ? view : { ...view, timeoutMs });
    budget.remaining();
    let native: C2NativeSubscription;
    try {
      const endpoint = await acquire(route, contract, budget);
      native = await endpoint.openSubscription(route, topic, {
        ...policy, ...(budget.metadata === undefined ? {} : { metadata: budget.metadata }),
      }, budget.remaining());
    } catch (error) {
      throw projectNativeError(error);
    }
    return new DecodedTopicSubscription(native, decode, hold);
  }
  return subscribe;
}

class DecodedTopicSubscription<T> implements C2TopicSubscription<T | C2HeldResult<T>> {
  constructor(
    private readonly native: C2NativeSubscription,
    private readonly decode: (bytes: Uint8Array) => T,
    private readonly hold: boolean,
  ) {}

  get startSeq(): bigint { return this.native.startSeq; }
  get topic(): string { return this.native.topic; }
  [Symbol.asyncIterator](): this { return this; }

  async next(): Promise<IteratorResult<TopicDelivery<T | C2HeldResult<T>>, undefined>> {
    let item: IteratorResult<TopicDelivery<C2NativeBuffer>, undefined>;
    try {
      item = await this.native.next();
    } catch (error) {
      throw projectNativeError(error);
    }
    if (item.done) return item;
    const { value: buffer, seq, dropped } = item.value;
    let decoded: T | undefined;
    try {
      this.native.checkDeadline();
      decoded = this.decode(buffer);
      this.native.checkDeadline();
      const value = this.hold ? createHeldResult(decoded, true, buffer, true) : decoded;
      if (!this.hold) buffer.release();
      return { done: false, value: Object.freeze({ value, seq, dropped }) };
    } catch (error) {
      tryReleaseResponsePayload(buffer);
      tryReleaseStreamValue(decoded);
      throw projectNativeError(error);
    }
  }

  async return(): Promise<IteratorResult<TopicDelivery<T | C2HeldResult<T>>, undefined>> {
    await this.close();
    return { done: true, value: undefined };
  }

  cancel(): void {
    try { this.native.cancel(); } catch (error) { throw projectNativeError(error); }
  }

  async close(timeoutMs?: number | null): Promise<C2StreamReceipt> {
    try {
      const receipt = await this.native.close(timeoutMs);
      return { ...receipt, error: receipt.error === null ? null : projectNativeError(receipt.error) };
    } catch (error) {
      throw projectNativeError(error);
    }
  }
}

export function createIpcEncodedTransport<Payload extends C2ResponsePayload = C2ByteArray>(address: string, options: C2IpcTransportOptions<Payload>): C2IpcEncodedTransport<Payload> {
  return createIpcTargetTransport(address, options);
}

function createIpcTargetTransport<Payload extends C2ResponsePayload>(address: string, options: C2IpcTransportOptions<Payload>, onDispatch?: () => void): C2IpcTargetTransport<Payload> {
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
  let nativeEndpoint: C2NativeEndpointContext | undefined;
  type Prepared = { native: C2NativeEndpointContext };
  const prepared = new Map<string, Prepared>();
  let closed = false;
  let requests = 0;
  let responses = 0;
  const requireOpen = (): void => {
    if (closed) throw new C2IpcTransportError("C-Two IPC transport is deactivated.");
  };
  const endpoint = (): Promise<C2NativeEndpointContext> => {
    requireOpen();
    if (endpointPromise === undefined) {
      const nativeOptions: C2IpcNativeEndpointOptions = {
        ...(options.connectTimeoutMs === undefined ? {} : { connectTimeoutMs: options.connectTimeoutMs }),
        ...(options.expectedServerIdentity === undefined ? {} : { expectedServerIdentity: options.expectedServerIdentity }),
      };
      const pending = Promise.resolve().then(() => options.connect(address, options.connectTimeoutMs, nativeOptions)).then(native => {
        nativeEndpoint = native;
        return native;
      }).catch(error => {
        if (endpointPromise === pending) endpointPromise = undefined;
        throw projectNativeError(error);
      });
      endpointPromise = pending;
    }
    return endpointPromise;
  };
  const prepare = async (routeName: string, contract: C2ContractIdentity, budget?: C2CallBudget): Promise<void> => {
    const routeContract = requireRouteContractIdentity(contract);
    budget?.remaining();
    const native = budget === undefined ? await endpoint() : await budget.wait(endpoint());
    requireOpen();
    if (native.connectionSnapshot(routeName)?.endReason === "protocol_error") {
      throw new C2IpcTransportError("IPC connection ended on a protocol error", {
        message: "IPC connection ended on a protocol error", code: 713,
        canonicalName: "ProtocolViolation", transportPhase: "pre_dispatch",
      });
    }
    const remaining = budget?.remaining();
    const pending = native.prepare(routeName, routeContract, remaining ?? undefined).catch(error => { throw projectNativeError(error); });
    if (budget === undefined) await pending;
    else await budget.wait(pending);
    requireOpen();
    prepared.set(routeName, { native });
  };
  const acquired = (routeName: string): Prepared => {
    requireOpen();
    const value = prepared.get(routeName);
    if (value === undefined) throw new Error("C-Two route must be acquired before dispatch.");
    return value;
  };
  const nativeLimit = (budget: C2CallBudget): number | null => {
    const remaining = budget.remaining();
    return budget.explicitTimeout || callTimeoutMs === null ? remaining : remaining === null ? callTimeoutMs : Math.min(callTimeoutMs, remaining);
  };
  const callAcquired = async (
    routeName: string,
    method: string,
    operation: C2UnaryRequest,
    budget: C2CallBudget,
    onTransportFailure?: (error: unknown) => void,
  ): Promise<Payload> => {
    const { native } = acquired(routeName);
    let call: C2NativeCall | undefined;
    let delivery: C2PendingResponseDelivery<Payload> | undefined;
    let delivered = false;
    let postResponse = false;
    let inputEncodeFailed = false;
    let hasPrimaryError = false;
    try {
      let target: C2NativeCallTarget;
      let response: C2NativeBuffer | null;
      if (operation.encode === undefined) {
        // Native recovery returns a buffer without a dispatch identity.
        target = { path: observationPath, serverId: null, serverInstanceId: null, connectionId: null };
        requests += 1;
        onDispatch?.();
        response = await native.recoverCall(routeName, method, operation.callId, nativeLimit(budget), budget.metadata);
      } else {
        call = native.beginCall(routeName, method, false, nativeLimit(budget), operation.callId, budget.metadata);
        let request: C2ByteArray;
        try {
          request = requireUint8Array(operation.encode(), "encoded input");
        } catch (encodeError) {
          inputEncodeFailed = true;
          throw encodeError;
        }
        budget.remaining();
        call.checkDeadline();
        await call.chargeInput(request.byteLength);
        requests += 1;
        onDispatch?.();
        target = await call.dispatch(request);
        response = await call.next();
      }
      if (response === null) throw new C2IpcTransportError("C-Two unary call ended without a response.");
      postResponse = true;
      responses += 1;
      let postError: unknown;
      let hasPostError = false;
      try {
        delivery = copyResponsePayloadToAllocator(response, allocator);
      } catch (copyError) {
        postError = copyError;
        hasPostError = true;
      } finally {
        if (allocator !== undefined) {
          try {
            response.release();
          } catch (releaseError) {
            if (!hasPostError) {
              postError = releaseError;
              hasPostError = true;
            }
          }
        }
      }
      const activeCall = call;
      call = undefined;
      try {
        activeCall?.close();
      } catch (closeError) {
        if (!hasPostError) {
          postError = closeError;
          hasPostError = true;
        }
      }
      if (hasPostError) {
        throw postError;
      }
      const payload = observeTransportResponse(delivery!, observe, () => ({
        path: observationPath === "RelayAwareLocalIpc" ? observationPath : target.path, routeName,
        serverId: target.serverId ?? undefined, serverInstanceId: target.serverInstanceId ?? undefined,
        connectionId: target.connectionId === null ? undefined : String(target.connectionId),
        ipcAddress: address, requests, responses,
      }));
      delivered = true;
      return payload;
    } catch (error) {
      hasPrimaryError = true;
      if (postResponse) {
        throw withUncertainCallId(sanitizePostDispatchCallbackError(error), operation.callId);
      }
      const projected = projectNativeError(error);
      if (!inputEncodeFailed) {
        onTransportFailure?.(projected);
      }
      throw withUncertainCallId(projected, operation.callId);
    } finally {
      if (!delivered) {
        delivery?.release();
      }
      if (call !== undefined) {
        try {
          call.close();
        } catch (closeError) {
          if (!hasPrimaryError) {
            if (postResponse) {
              throw sanitizePostDispatchCallbackError(closeError);
            }
            const projectedClose = projectNativeError(closeError);
            if (!inputEncodeFailed) {
              onTransportFailure?.(projectedClose);
            }
            throw projectedClose;
          }
        }
      }
    }
  };
  const startStream = <T>(
    routeName: string,
    method: string,
    encode: () => C2ByteArray,
    decode: (bytes: Uint8Array) => T,
    hold: boolean,
    budget: C2CallBudget,
    preparation?: Promise<void>,
    cancelled = false,
    onTransportFailure?: (error: unknown) => void,
  ): C2ResponseStream<T | C2HeldResult<T>> => {
    return new NativeC2ResponseStream<T | C2HeldResult<T>>(async state => {
      if (preparation !== undefined) await preparation;
      state.cancelled ||= cancelled;
      let inputEncodeFailed = false;
      try {
        const { native } = acquired(routeName);
        const call = native.beginCall(routeName, method, true, nativeLimit(budget), undefined, budget.metadata);
        state.call = call;
        if (state.collected) {
          call.cancel();
          closeCollectedCall(state);
          return call;
        }
        if (state.cancelled) { call.cancel(); return call; }
        let request: C2ByteArray;
        try {
          request = requireUint8Array(encode(), "encoded stream input");
        } catch (encodeError) {
          inputEncodeFailed = true;
          throw encodeError;
        }
        budget.remaining();
        call.checkDeadline();
        await call.chargeInput(request.byteLength);
        if (state.cancelled) { call.cancel(); return call; }
        requests += 1;
        onDispatch?.();
        await call.dispatch(request);
        return call;
      } catch (error) {
        const projected = projectNativeError(error);
        if (!inputEncodeFailed) {
          onTransportFailure?.(projected);
        }
        if (!((projected instanceof C2IpcTransportError || projected instanceof C2CoreError) && projected.code === 715)) tryCancelStreamCall(state);
        if (state.collected) closeCollectedCall(state);
        throw projected;
      }
    }, decode as (bytes: Uint8Array) => T | C2HeldResult<T>, hold, onTransportFailure);
  };
  function stream<T>(routeName: string, contract: C2ContractIdentity, method: string, encode: () => C2ByteArray, decode: (bytes: Uint8Array) => T, hold?: false, view?: C2CallOptions): C2ResponseStream<T>;
  function stream<T>(routeName: string, contract: C2ContractIdentity, method: string, encode: () => C2ByteArray, decode: (bytes: Uint8Array) => T, hold: true, view?: C2CallOptions): C2ResponseStream<C2HeldResult<T>>;
  function stream<T>(routeName: string, contract: C2ContractIdentity, method: string, encode: () => C2ByteArray, decode: (bytes: Uint8Array) => T, hold = false, view?: C2CallOptions): C2ResponseStream<T | C2HeldResult<T>> {
    const budget = new C2CallBudget(callTimeoutMs, view);
    return startStream(routeName, method, encode, decode, hold, budget, prepare(routeName, contract, budget));
  }
  const unary = async (routeName: string, contract: C2ContractIdentity, method: string, operation: C2UnaryRequest, view?: C2CallOptions): Promise<Payload> => {
    const budget = new C2CallBudget(callTimeoutMs, view);
    await prepare(routeName, contract, budget);
    return callAcquired(routeName, method, operation, budget);
  };
  const queryAcquired = async (routeName: string, callId: string, budget: C2CallBudget): Promise<C2CallOutcome> => {
    try {
      return await acquired(routeName).native.queryCall(routeName, callId, nativeLimit(budget), budget.metadata);
    } catch (error) {
      throw withUncertainCallId(projectNativeError(error), callId);
    }
  };
  return {
    async prepare(routeName, contract): Promise<void> { await prepare(routeName, contract); },
    acquire: prepare,
    snapshot(routeName) { return nativeEndpoint?.connectionSnapshot(routeName) ?? null; },
    callAcquired,
    streamAcquired: (routeName, method, encode, decode, hold, budget, cancelled, onTransportFailure) =>
      startStream(routeName, method, encode, decode, hold, budget, undefined, cancelled, onTransportFailure),
    async call(routeName, contract, method, encode, options, view) {
      return unary(routeName, contract, method, submitRequest(encode, options), view);
    },
    async recoverCall(routeName, contract, method, callId, view) {
      return unary(routeName, contract, method, { callId: requireCallId(callId) }, view);
    },
    queryAcquired,
    async queryCall(routeName, contract, callId, view) {
      requireCallId(callId);
      const budget = new C2CallBudget(callTimeoutMs, view);
      await prepare(routeName, contract, budget);
      return queryAcquired(routeName, callId, budget);
    },
    stream,
    subscribe: nativeTopicSubscribe(async (route, contract, budget) => {
      await prepare(route, contract, budget);
      return acquired(route).native;
    }),
    async close(): Promise<void> {
      closed = true;
      const native = await endpointPromise?.catch(() => undefined);
      await native?.close();
    },
  };
}

export function createHttpRelayEncodedTransport<Payload extends C2ResponsePayload = C2ByteArray>(baseUrl: string, options: C2HttpRelayTransportOptions<Payload> = {}): C2HttpRelayEncodedTransport<Payload> {
  return createRelayTransport(baseUrl, options, true);
}

export function createRelayAwareHttpEncodedTransport<Payload extends C2ResponsePayload = C2ByteArray>(anchorUrl: string, options: C2RelayAwareHttpTransportOptions<Payload> = {}): C2RelayAwareHttpEncodedTransport<Payload> {
  return createRelayTransport(anchorUrl, options, false);
}

interface C2RelayLocalTransport<Payload extends C2ResponsePayload> {
  readonly key: string;
  readonly transport: C2IpcTargetTransport<Payload>;
  targets: number;
  acquiring: number;
  retiring: boolean;
}

type C2RelayTarget<Payload extends C2ResponsePayload> = {
  readonly relayUrl: string;
  readonly local?: C2RelayLocalTransport<Payload>;
  needsAcquire: boolean;
  terminal?: Error;
};

interface C2RelayTargetSlot<Payload extends C2ResponsePayload> {
  current?: C2RelayTarget<Payload>;
  acquiring?: Promise<C2RelayTarget<Payload>>;
  acquiringBudget?: C2CallBudget;
}

function createRelayTransport<Payload extends C2ResponsePayload>(baseUrl: string, options: C2RelayAwareHttpTransportOptions<Payload>, explicit: boolean): C2RelayAwareHttpEncodedTransport<Payload> {
  const normalizedOptions = normalizeHttpRelayTransportOptions(options, "C-Two relay transport options");
  const anchorUrl = normalizeRelayBaseUrl(baseUrl, "C-Two relay URL");
  const fetchImpl = normalizeFetch(normalizedOptions.fetch, "C-Two relay fetch") ?? globalFetch();
  const headers = normalizeHttpHeaders(normalizedOptions.headers, "C-Two relay headers");
  const callTimeoutMs = normalizeTimeoutMs(normalizedOptions.callTimeoutMs, "C-Two relay callTimeoutMs", 300_000);
  const resolveTimeoutMs = normalizeTimeoutMs(normalizedOptions.resolveTimeoutMs, "C-Two relay resolveTimeoutMs", 5_000);
  const allocator = normalizeResponsePayloadAllocator(normalizedOptions.responsePayloadAllocator, "C-Two relay responsePayloadAllocator");
  const unknownLengthStrategy = normalizeResponsePayloadUnknownLengthStrategy(normalizedOptions.responsePayloadUnknownLengthStrategy, "C-Two relay responsePayloadUnknownLengthStrategy");
  const unknownLengthMaxBytes = normalizeUnknownLengthBufferMaxBytes(normalizedOptions.responsePayloadUnknownLengthMaxBytes, "C-Two relay responsePayloadUnknownLengthMaxBytes");
  const observe = normalizeTransportObserver(normalizedOptions.observe, "C-Two relay observe");
  const ipcOptions = explicit ? undefined : normalizeRelayAwareIpcOptions(normalizedOptions.ipc);
  const slots = new Map<string, C2RelayTargetSlot<Payload>>();
  const localTransports = new Map<string, C2RelayLocalTransport<Payload>>();
  const retired: Promise<PromiseSettledResult<void>>[] = [];
  let closed = false;
  let closePromise: Promise<void> | undefined;
  let requests = 0;
  let responses = 0;
  const requireOpen = (): void => {
    if (closed) throw new C2IpcTransportError("C-Two relay transport is deactivated.");
  };
  let subscriptionEndpoint: Promise<C2NativeEndpointContext> | undefined;
  const subscribe = nativeTopicSubscribe(async (route, contract, budget) => {
    requireOpen();
    const connect = normalizedOptions.connect ?? ipcOptions?.connect;
    if (connect === undefined) throw new TypeError("C-Two relay subscriptions require a native connect option.");
    if (subscriptionEndpoint === undefined) {
      const mode = explicit ? "explicit_relay" : "relay_aware";
      const pending = Promise.resolve().then(() => connect(anchorUrl, undefined, { mode }));
      subscriptionEndpoint = pending;
      void pending.catch(() => {
        if (subscriptionEndpoint === pending) subscriptionEndpoint = undefined;
      });
    }
    const endpoint = await budget.wait(subscriptionEndpoint);
    requireOpen();
    const limit = budget.stageLimit(resolveTimeoutMs) || undefined;
    await budget.wait(endpoint.prepare(route, requireRouteContractIdentity(contract), limit));
    requireOpen();
    return endpoint;
  });
  const retire = (local: C2RelayLocalTransport<Payload>): Promise<PromiseSettledResult<void>> | undefined => {
    if (local.retiring) return undefined;
    local.retiring = true;
    if (localTransports.get(local.key) === local) localTransports.delete(local.key);
    const pending = local.transport.close().then(
      (): PromiseFulfilledResult<void> => ({ status: "fulfilled", value: undefined }),
      (reason): PromiseRejectedResult => ({ status: "rejected", reason }),
    );
    retired.push(pending);
    return pending;
  };
  const localFor = (route: C2RelayRouteInfo): C2RelayLocalTransport<Payload> => {
    requireOpen();
    const key = localRouteEndpointKey(route);
    let local = localTransports.get(key);
    if (local === undefined) {
      local = {
        key, targets: 0, acquiring: 0, retiring: false,
        transport: createIpcTargetTransport(route.ipcAddress!, {
          ...ipcOptions!,
          expectedServerIdentity: { serverId: route.serverId!, serverInstanceId: route.serverInstanceId! },
          observationPath: "RelayAwareLocalIpc",
          observe: value => {
            responses += 1;
            observe?.(Object.freeze({ ...value, requests, responses }));
          },
        }, () => { requests += 1; }),
      };
      localTransports.set(key, local);
    }
    return local;
  };
  const needsAcquire = (target: C2RelayTarget<Payload>, routeName: string): boolean => {
    if (target.local !== undefined) {
      const snapshot = target.local.transport.snapshot(routeName);
      if (snapshot?.endReason === "protocol_error") {
        target.terminal ??= new C2IpcTransportError("IPC connection ended on a protocol error", {
          message: "IPC connection ended on a protocol error", code: 713,
          canonicalName: "ProtocolViolation", transportPhase: "pre_dispatch",
        });
      } else if (snapshot !== null && (snapshot.endReason === "peer_eof" || snapshot.endReason === "peer_close_message" || snapshot.endReason === "connection_reset")) {
        retire(target.local);
        return true;
      }
    }
    if (target.terminal !== undefined) throw target.terminal;
    return target.needsAcquire;
  };
  const observeFailure = (target: C2RelayTarget<Payload>, failure: unknown): void => {
    const error = projectNativeError(failure);
    const code = preDispatchRouteCode(error);
    if (code === 701 || code === 707 || code === 708 || code === 709 || code === 719) target.needsAcquire = true;
    if (terminalRouteRefusal(error)) target.terminal = error;
    if (error instanceof C2HttpTransportError && error.cannotConnect) target.needsAcquire = true;
    if (target.local !== undefined && error instanceof C2IpcTransportError && error.code === 707 && error.transportPhase === "pre_dispatch" && error.details.route === undefined) {
      retire(target.local);
    }
  };
  const acquire = async (routeName: string, contract: C2RouteContractIdentity, budget: C2CallBudget): Promise<C2RelayTarget<Payload>> => {
    let refreshed = false;
    let firstLocalFailure: unknown;
    let lastError: unknown;
    const excluded = new Set<string>();
    const resolve = () => resolveRelayRoutes(fetchImpl, anchorUrl, routeName, contract, headers, budget.stageLimit(resolveTimeoutMs));
    let routes = await resolve();
    while (true) {
      budget.remaining();
      requireOpen();
      const route = selectLocalIpcRoute(routes, anchorUrl, ipcOptions, excluded);
      if (route !== undefined) {
        const local = localFor(route);
        local.acquiring += 1;
        try {
          await local.transport.acquire(routeName, contract, budget);
          local.acquiring -= 1;
          local.targets += 1;
          return { relayUrl: route.relayUrl, local, needsAcquire: false };
        } catch (failure) {
          local.acquiring -= 1;
          const error = projectNativeError(failure);
          const closing = local.targets === 0 && local.acquiring === 0 ? retire(local) : undefined;
          if (localCandidateFailureIsTerminal(error)) {
            const cleanup = await closing;
            if (cleanup?.status === "rejected") Object.defineProperty(error, "cleanupError", { value: cleanup.reason });
            throw error;
          }
          firstLocalFailure ??= error;
          lastError = error;
          excluded.add(localRouteEndpointKey(route));
          if (!refreshed) {
            refreshed = true;
            try { routes = await resolve(); }
            catch (failure) {
              if (preDispatchRouteCode(failure) === 701) throw samePathFallbackDeniedError(routeName, firstLocalFailure);
              throw failure;
            }
          }
          continue;
        }
      }
      const available = routes.filter(route => !localRouteEndpointIsExcluded(route, excluded));
      if (available.length === 0) {
        if (excluded.size > 0) throw samePathFallbackDeniedError(routeName, firstLocalFailure);
        throw resourceNotFoundError(routeName);
      }
      let resolveAgain = false;
      for (const route of available) {
        try {
          await probeResolvedHttpRoute(fetchImpl, route.relayUrl, routeName, contract, headers, budget.stageLimit(resolveTimeoutMs));
          budget.remaining();
          return { relayUrl: route.relayUrl, needsAcquire: false };
        } catch (failure) {
          const error = projectNativeError(failure);
          lastError = error;
          if (error instanceof C2RouteConflictError || terminalRouteRefusal(error) || (error instanceof C2CoreError && error.code === 715) || preDispatchRouteCode(error) === 709) throw error;
          if (preDispatchRouteCode(error) === 701 && !refreshed) {
            refreshed = true;
            resolveAgain = true;
            break;
          }
        }
      }
      if (resolveAgain) { routes = await resolve(); continue; }
      throw lastError;
    }
  };
  const targetForCall = async (routeName: string, contract: C2RouteContractIdentity, budget: C2CallBudget): Promise<C2RelayTarget<Payload>> => {
    requireOpen();
    budget.remaining();
    const key = routeContractKey(routeName, contract);
    let slot = slots.get(key);
    if (slot === undefined) { slot = {}; slots.set(key, slot); }
    if (slot.current !== undefined && !needsAcquire(slot.current, routeName)) return slot.current;
    while (true) {
      if (slot.acquiring === undefined) {
        const owner = slot;
        const pending = Promise.resolve().then(async () => {
          const next = await acquire(routeName, contract, budget);
          try { budget.remaining(); requireOpen(); }
          catch (error) {
            if (next.local !== undefined) {
              next.local.targets -= 1;
              if (next.local.targets === 0 && next.local.acquiring === 0) retire(next.local);
            }
            throw error;
          }
          const previous = owner.current;
          owner.current = next;
          if (previous?.local !== undefined) {
            previous.local.targets -= 1;
            if (previous.local.targets === 0 && previous.local !== next.local) retire(previous.local);
          }
          return next;
        });
        owner.acquiring = pending;
        owner.acquiringBudget = budget;
        void pending.finally(() => {
          if (owner.acquiring === pending) {
            delete owner.acquiring;
            delete owner.acquiringBudget;
          }
        }).catch(() => undefined);
      }
      const pending = slot.acquiring!;
      const ownerBudget = slot.acquiringBudget!;
      try { return await budget.wait(pending); }
      catch (error) {
        budget.remaining();
        // A longer-lived waiter may start the next acquisition only after the
        // bounded attempt it shared has really returned.
        if ((error instanceof C2CoreError || error instanceof C2IpcTransportError) && error.code === 715 && ownerBudget !== budget && ownerBudget.expired() && slot.acquiring !== pending) continue;
        throw error;
      }
    }
  };
  function stream<T>(routeName: string, contract: C2ContractIdentity, method: string, encode: () => C2ByteArray, decode: (bytes: Uint8Array) => T, hold?: false, view?: C2CallOptions): C2ResponseStream<T>;
  function stream<T>(routeName: string, contract: C2ContractIdentity, method: string, encode: () => C2ByteArray, decode: (bytes: Uint8Array) => T, hold: true, view?: C2CallOptions): C2ResponseStream<C2HeldResult<T>>;
  function stream<T>(routeName: string, contract: C2ContractIdentity, method: string, encode: () => C2ByteArray, decode: (bytes: Uint8Array) => T, hold = false, view?: C2CallOptions): C2ResponseStream<T | C2HeldResult<T>> {
      if (explicit) throw new C2UnsupportedCallModeError();
      const budget = new C2CallBudget(callTimeoutMs === 0 ? null : callTimeoutMs, view);
      return new DeferredC2ResponseStream<T | C2HeldResult<T>>(async state => {
        requireRouteNamePathValue(routeName);
        requireMethodPathValue(method);
        const target = await targetForCall(routeName, requireRouteContractIdentity(contract), budget);
        if (target.local === undefined) throw new C2UnsupportedCallModeError();
        const stream = target.local.transport.streamAcquired(
          routeName,
          method,
          encode,
          decode,
          hold,
          budget,
          state.cancelled,
          (transportError) => observeFailure(target, transportError),
        );
        if (state.cancelled) void stream.return?.();
        return stream;
      });
  }
  const unary = async (routeName: string, contract: C2ContractIdentity, method: string, operation: C2UnaryRequest, view?: C2CallOptions): Promise<Payload> => {
    requireRouteNamePathValue(routeName);
    requireMethodPathValue(method);
    const budget = new C2CallBudget(callTimeoutMs === 0 ? null : callTimeoutMs, view);
    const routeContract = requireRouteContractIdentity(contract);
    const target = await targetForCall(routeName, routeContract, budget);
    if (target.local !== undefined) {
      return await target.local.transport.callAcquired(
        routeName,
        method,
        operation,
        budget,
        (transportError) => observeFailure(target, transportError),
      );
    }
    const request = operation.encode === undefined ? undefined : encodeHttpUnaryInput(operation.encode, budget);
    let delivery: C2PendingResponseDelivery<Payload>;
    try {
      requests += 1;
      delivery = await callResolvedHttpRoute(fetchImpl, target.relayUrl, routeName, routeContract, method, request, operation.callId, headers, budget.remaining() ?? 0, budget.metadata, allocator, unknownLengthStrategy, unknownLengthMaxBytes);
    } catch (error) {
      observeFailure(target, error);
      throw withUncertainCallId(error, operation.callId);
    }
    responses += 1;
    try {
      return observeTransportResponse(delivery, observe, () => ({
        path: explicit ? "ExplicitRelay" : "RelayAwareHttp", routeName,
        relayUrl: target.relayUrl, requests, responses,
      }));
    } catch (error) {
      throw withUncertainCallId(error, operation.callId);
    }
  };
  return {
    async call(routeName, contract, method, encode, options, view) {
      return unary(routeName, contract, method, submitRequest(encode, options), view);
    },
    async recoverCall(routeName, contract, method, callId, view) {
      return unary(routeName, contract, method, { callId: requireCallId(callId) }, view);
    },
    async queryCall(routeName, contract, callId, view) {
      requireCallId(callId);
      requireRouteNamePathValue(routeName);
      const routeContract = requireRouteContractIdentity(contract);
      const budget = new C2CallBudget(callTimeoutMs === 0 ? null : callTimeoutMs, view);
      const target = await targetForCall(routeName, routeContract, budget);
      try {
        if (target.local !== undefined) return await target.local.transport.queryAcquired(routeName, callId, budget);
        return await queryResolvedHttpRoute(fetchImpl, target.relayUrl, routeName, routeContract, callId, headers, budget.remaining() ?? 0, budget.metadata);
      } catch (error) {
        observeFailure(target, error);
        throw withUncertainCallId(error, callId);
      }
    },
    stream,
    subscribe,
    close(): Promise<void> {
      if (closePromise !== undefined) return closePromise;
      closed = true;
      closePromise = (async () => {
        await Promise.allSettled([...slots.values()].flatMap(slot => slot.acquiring === undefined ? [] : [slot.acquiring]));
        for (const local of localTransports.values()) retire(local);
        slots.clear();
        const native = await subscriptionEndpoint?.catch(() => undefined);
        if (native !== undefined) retired.push(Promise.resolve().then(() => native.close()).then(
          (): PromiseFulfilledResult<void> => ({ status: "fulfilled", value: undefined }),
          (reason): PromiseRejectedResult => ({ status: "rejected", reason }),
        ));
        const results = await Promise.all(retired);
        for (const result of results) if (result.status === "rejected") throw result.reason;
      })();
      return closePromise;
    },
  };
}

function requireCallId(value: unknown): string {
  if (typeof value !== "string" || value.length < 1 || value.length > 128 || /[^A-Za-z0-9_-]/.test(value)) {
    throw new TypeError("C-Two callId must be 1 to 128 ASCII letters, digits, underscores or hyphens.");
  }
  return value;
}

function submitRequest(encode: () => C2ByteArray, options?: C2CallIdOptions): C2UnaryRequest {
  if (typeof encode !== "function") throw new TypeError("C-Two unary input requires an encoder.");
  const callId = options?.callId;
  return { encode, callId: callId === undefined ? undefined : requireCallId(callId) };
}

function withUncertainCallId(error: unknown, callId: string | undefined): unknown {
  if (callId !== undefined &&
      (error instanceof C2HttpRelayError || error instanceof C2HttpTransportError || error instanceof C2CoreError || error instanceof C2IpcTransportError) &&
      error.transportPhase === "dispatch_uncertain" && error.callId === undefined) {
    Object.defineProperty(error, "callId", { value: callId, enumerable: true });
  }
  return error;
}

function encodeHttpUnaryInput(encode: () => C2ByteArray, budget: C2CallBudget): C2ByteArray {
  budget.remaining();
  const payload = requireUint8Array(encode(), "encoded input");
  budget.remaining();
  return payload;
}

function requireUint8Array(value: unknown, label: string): C2ByteArray {
  if (!(value instanceof Uint8Array)) {
    throw new Error(`${label} must be a Uint8Array.`);
  }
  return value as C2ByteArray;
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

interface C2PendingResponseDelivery<Payload extends C2ResponsePayload> {
  readonly payload: Payload;
  release(): void;
}

interface C2HttpAbortContext {
  readonly signal: C2AbortSignal | undefined;
  trackAllocationCleanup(done: Promise<void>): void;
}

function makePlainDelivery<Payload extends C2ResponsePayload>(payload: Payload): C2PendingResponseDelivery<Payload> {
  let released = false;
  return {
    payload,
    release() {
      if (released) return;
      released = true;
      tryReleaseResponsePayload(payload);
    },
  };
}

type C2AllocatedReleaseState =
  | { readonly hasPayload: false }
  | { readonly hasPayload: true; readonly payload: unknown };

function createAllocatedReleaseGuard<Payload extends C2ResponsePayload>(
  allocated: C2AllocatedResponsePayload<Payload>,
): (state?: C2AllocatedReleaseState) => void {
  let released = false;
  return (state?: C2AllocatedReleaseState) => {
    if (released) return;
    released = true;
    try {
      if (allocated.release !== undefined) {
        allocated.release();
        return;
      }
      if (state !== undefined) {
        if (state.hasPayload) {
          releaseHeldValue(state.payload);
        }
        return;
      }
      releaseHeldValue(allocated.payload);
    } catch (cleanupError) {
      console.warn("Warning: allocated response payload cleanup failed:", cleanupError);
    }
  };
}

function finalizeAllocatedDelivery<Payload extends C2ResponsePayload>(
  allocated: C2AllocatedResponsePayload<Payload>,
  releaseOnce: (state?: C2AllocatedReleaseState) => void,
): C2PendingResponseDelivery<Payload> {
  let payload: Payload;
  let hasPayload = false;
  try {
    payload = allocated.payload;
    hasPayload = true;
    if (!isResponsePayload(payload)) {
      releaseOnce({ hasPayload: true, payload });
      throw new Error("C-Two responsePayloadAllocator result payload must be a response payload.");
    }
  } catch (error) {
    if (hasPayload) {
      releaseOnce({ hasPayload: true, payload: payload! });
    } else {
      releaseOnce({ hasPayload: false });
    }
    throw error;
  }
  return {
    payload,
    release() {
      releaseOnce({ hasPayload: true, payload });
    },
  };
}

function copyResponsePayloadToAllocator<Payload extends C2ResponsePayload>(
  payload: Uint8Array,
  allocator: C2ResponsePayloadAllocator<Payload> | undefined,
): C2PendingResponseDelivery<Payload> {
  if (allocator === undefined) {
    return makePlainDelivery(payload as Payload);
  }
  const allocated = requireAllocatedResponseView<Payload>(allocator(payload.byteLength), payload.byteLength);
  const releaseOnce = createAllocatedReleaseGuard(allocated);
  try {
    allocated.view.set(payload);
  } catch (error) {
    releaseOnce();
    throw error;
  }
  return finalizeAllocatedDelivery(allocated, releaseOnce);
}

async function readHttpSuccessResponsePayload<Payload extends C2ResponsePayload>(
  response: C2FetchResponse,
  allocator: C2ResponsePayloadAllocator<Payload> | undefined,
  unknownLengthStrategy: C2ResponsePayloadUnknownLengthStrategy,
  unknownLengthMaxBytes: number,
  abortContext?: C2HttpAbortContext,
): Promise<C2PendingResponseDelivery<Payload>> {
  const signal = abortContext?.signal;
  if (allocator !== undefined) {
    const wireContentLength = responseContentLength(response);
    const contentLength = responseHasIdentityContentEncoding(response) ? wireContentLength : undefined;
    if (contentLength === undefined) {
      if (unknownLengthStrategy !== "buffer") {
        throw new Error("C-Two HTTP relay response Content-Length is required when responsePayloadAllocator is configured.");
      }
      if (response.body === undefined || response.body === null) {
        throw new Error("C-Two HTTP relay response body stream is required when responsePayloadAllocator is configured.");
      }
      return await readUnknownLengthAllocatedResponsePayload(response.body, allocator, unknownLengthMaxBytes, abortContext);
    }
    if (contentLength === 0) {
      if (response.body === undefined || response.body === null) {
        const actualBytes = new Uint8Array(await response.arrayBuffer());
        if (signal?.aborted) {
          throw abortReason(signal);
        }
        if (actualBytes.byteLength !== 0) {
          throw new Error("C-Two HTTP relay response body byte length does not match Content-Length.");
        }
        const allocated = requireAllocatedResponseView<Payload>(allocator(0), 0);
        return finalizeAllocatedDelivery(allocated, createAllocatedReleaseGuard(allocated));
      }
      return await readAllocatedResponsePayload(response.body, 0, allocator, abortContext);
    }
    if (response.body === undefined || response.body === null) {
      throw new Error("C-Two HTTP relay response body stream is required when responsePayloadAllocator is configured.");
    }
    return await readAllocatedResponsePayload(response.body, contentLength, allocator, abortContext);
  }
  return makePlainDelivery(await readHttpArrayBufferResponsePayload(response, signal) as Payload);
}

async function readHttpArrayBufferResponsePayload(response: C2FetchResponse, signal?: C2AbortSignal): Promise<Uint8Array> {
  const contentLength = responseContentLength(response);
  const identityEncoding = responseHasIdentityContentEncoding(response);
  if (!identityEncoding && response.body !== undefined && response.body !== null) {
    return await readBoundedStreamBytes(response.body, C2_MAX_RESPONSE_PAYLOAD_BYTES, signal);
  }
  const payload = new Uint8Array(await response.arrayBuffer());
  if (payload.byteLength > C2_MAX_RESPONSE_PAYLOAD_BYTES) {
    throw new Error(`C-Two HTTP relay response body must be no greater than ${C2_MAX_RESPONSE_PAYLOAD_BYTES} bytes.`);
  }
  if (contentLength !== undefined && identityEncoding && payload.byteLength !== contentLength) {
    throw new Error("C-Two HTTP relay response body byte length does not match Content-Length.");
  }
  return payload;
}

function abortReason(signal: C2AbortSignal): unknown {
  return signal.reason ?? new Error("C-Two HTTP relay response read aborted.");
}

function createStreamAbortGuard(
  signal: C2AbortSignal | undefined,
  onAbortCancelReader: (reason: unknown) => void,
): {
  readChunk: (reader: C2ReadableStreamReader) => Promise<{ done: boolean; value?: C2ByteArray }>;
  dispose: () => void;
} {
  if (signal === undefined) {
    return {
      readChunk: (reader) => reader.read(),
      dispose: () => undefined,
    };
  }
  let currentWake: ((reason: unknown) => void) | undefined;
  const handleAbort = () => {
    const reason = abortReason(signal);
    onAbortCancelReader(reason);
    const wake = currentWake;
    currentWake = undefined;
    wake?.(reason);
  };
  if (signal.aborted) {
    handleAbort();
  } else {
    signal.addEventListener("abort", handleAbort, { once: true });
  }
  return {
    readChunk: async (reader) => {
      if (signal.aborted) {
        const reason = abortReason(signal);
        onAbortCancelReader(reason);
        throw reason;
      }
      const wakePromise = new Promise<never>((_resolve, reject) => {
        currentWake = reject;
      });
      wakePromise.catch(() => undefined);
      try {
        const chunk = await Promise.race([reader.read(), wakePromise]);
        if (signal.aborted) {
          const reason = abortReason(signal);
          onAbortCancelReader(reason);
          throw reason;
        }
        return chunk;
      } finally {
        currentWake = undefined;
      }
    },
    dispose: () => {
      currentWake = undefined;
      signal.removeEventListener?.("abort", handleAbort);
    },
  };
}

function cancelStreamReaderDetached(reader: C2ReadableStreamReader | undefined, reason: unknown): void {
  try {
    if (reader === undefined || reader === null) {
      return;
    }
    const cancelFn = reader.cancel;
    if (typeof cancelFn !== "function") {
      return;
    }
    const pending = cancelFn.call(reader, reason);
    if (pending !== undefined && pending !== null && typeof (pending as PromiseLike<unknown>).then === "function") {
      (pending as PromiseLike<unknown>).then(
        () => undefined,
        () => undefined,
      );
    }
  } catch {
    // Preserve the primary protocol or size error.
  }
}

function releaseStreamReaderLock(reader: C2ReadableStreamReader | undefined, suppressError: boolean): void {
  if (reader === undefined || reader === null) {
    return;
  }
  try {
    const releaseFn = reader.releaseLock;
    if (typeof releaseFn === "function") {
      releaseFn.call(reader);
    }
  } catch (error) {
    if (!suppressError) {
      throw error;
    }
  }
}

async function readBoundedStreamBytes(body: C2ReadableStream, maxBytes: number, signal?: C2AbortSignal): Promise<Uint8Array> {
  if (signal?.aborted) {
    throw abortReason(signal);
  }
  const chunks: Uint8Array[] = [];
  let byteLength = 0;
  let reader: C2ReadableStreamReader | undefined;
  let readerCleanedUp = false;
  let abortGuard: ReturnType<typeof createStreamAbortGuard> | undefined;
  const cleanupReaderOnFailure = (reason: unknown) => {
    if (readerCleanedUp) return;
    readerCleanedUp = true;
    cancelStreamReaderDetached(reader, reason);
    releaseStreamReaderLock(reader, true);
  };
  try {
    reader = body.getReader();
    abortGuard = createStreamAbortGuard(signal, cleanupReaderOnFailure);
    while (true) {
      const chunk = await abortGuard.readChunk(reader);
      if (chunk.done) {
        break;
      }
      if (chunk.value === undefined) {
        continue;
      }
      if (!(chunk.value instanceof Uint8Array)) {
        throw new Error("C-Two HTTP relay response stream chunk must be a Uint8Array.");
      }
      if (chunk.value.byteLength === 0) {
        continue;
      }
      const nextByteLength = byteLength + chunk.value.byteLength;
      if (!Number.isSafeInteger(nextByteLength) || nextByteLength > maxBytes) {
        throw new Error(`C-Two HTTP relay response body must be no greater than ${maxBytes} bytes.`);
      }
      chunks.push(new Uint8Array(chunk.value));
      byteLength = nextByteLength;
    }
    readerCleanedUp = true;
    releaseStreamReaderLock(reader, false);
  } catch (error) {
    cleanupReaderOnFailure(error);
    throw error;
  } finally {
    abortGuard?.dispose();
  }
  const out = new Uint8Array(byteLength);
  let offset = 0;
  for (const chunk of chunks) {
    out.set(chunk, offset);
    offset += chunk.byteLength;
  }
  return out;
}

function responseHasIdentityContentEncoding(response: C2FetchResponse): boolean {
  const rawValue = response.headers?.get("content-encoding");
  if (rawValue === undefined || rawValue === null) {
    return true;
  }
  const normalized = rawValue.trim().toLowerCase();
  return normalized.length === 0 || normalized === "identity";
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

async function readAllocatedResponsePayload<Payload extends C2ResponsePayload>(
  body: C2ReadableStream,
  byteLength: number,
  allocator: C2ResponsePayloadAllocator<Payload>,
  abortContext?: C2HttpAbortContext,
): Promise<C2PendingResponseDelivery<Payload>> {
  const signal = abortContext?.signal;
  if (signal?.aborted) {
    throw abortReason(signal);
  }
  const allocated = requireAllocatedResponseView<Payload>(allocator(byteLength), byteLength);
  const releaseOnce = createAllocatedReleaseGuard(allocated);
  let finishCleanup!: () => void;
  const cleanupDone = new Promise<void>((resolve) => {
    finishCleanup = resolve;
  });
  abortContext?.trackAllocationCleanup(cleanupDone);
  let reader: C2ReadableStreamReader | undefined;
  let readerCleanedUp = false;
  let offset = 0;
  let abortGuard: ReturnType<typeof createStreamAbortGuard> | undefined;
  const cleanupReaderOnFailure = (reason: unknown) => {
    if (readerCleanedUp) return;
    readerCleanedUp = true;
    cancelStreamReaderDetached(reader, reason);
    releaseStreamReaderLock(reader, true);
  };
  try {
    reader = body.getReader();
    abortGuard = createStreamAbortGuard(signal, cleanupReaderOnFailure);
    while (true) {
      const chunk = await abortGuard.readChunk(reader);
      if (chunk.done) {
        break;
      }
      if (signal?.aborted) {
        const reason = abortReason(signal);
        cleanupReaderOnFailure(reason);
        throw reason;
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
    if (signal?.aborted) {
      const reason = abortReason(signal);
      cleanupReaderOnFailure(reason);
      throw reason;
    }
    if (offset !== byteLength) {
      throw new Error("C-Two HTTP relay response stream ended before Content-Length bytes were read.");
    }
    readerCleanedUp = true;
    releaseStreamReaderLock(reader, false);
    return finalizeAllocatedDelivery(allocated, releaseOnce);
  } catch (error) {
    cleanupReaderOnFailure(error);
    releaseOnce();
    throw error;
  } finally {
    abortGuard?.dispose();
    finishCleanup();
  }
}

async function readUnknownLengthAllocatedResponsePayload<Payload extends C2ResponsePayload>(
  body: C2ReadableStream,
  allocator: C2ResponsePayloadAllocator<Payload>,
  maxBytes: number,
  abortContext?: C2HttpAbortContext,
): Promise<C2PendingResponseDelivery<Payload>> {
  const signal = abortContext?.signal;
  if (signal?.aborted) {
    throw abortReason(signal);
  }
  const chunks: Uint8Array[] = [];
  let byteLength = 0;
  let reader: C2ReadableStreamReader | undefined;
  let readerCleanedUp = false;
  let abortGuard: ReturnType<typeof createStreamAbortGuard> | undefined;
  const cleanupReaderOnFailure = (reason: unknown) => {
    if (readerCleanedUp) return;
    readerCleanedUp = true;
    cancelStreamReaderDetached(reader, reason);
    releaseStreamReaderLock(reader, true);
  };
  try {
    reader = body.getReader();
    abortGuard = createStreamAbortGuard(signal, cleanupReaderOnFailure);
    while (true) {
      const chunk = await abortGuard.readChunk(reader);
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
      chunks.push(new Uint8Array(chunk.value));
      byteLength = nextByteLength;
    }
    readerCleanedUp = true;
    releaseStreamReaderLock(reader, false);
  } catch (error) {
    cleanupReaderOnFailure(error);
    throw error;
  } finally {
    abortGuard?.dispose();
  }
  if (signal?.aborted) {
    throw abortReason(signal);
  }
  const allocated = requireAllocatedResponseView<Payload>(allocator(byteLength), byteLength);
  const releaseOnce = createAllocatedReleaseGuard(allocated);
  try {
    let offset = 0;
    for (const chunk of chunks) {
      allocated.view.set(chunk, offset);
      offset += chunk.byteLength;
    }
  } catch (error) {
    releaseOnce();
    throw error;
  }
  return finalizeAllocatedDelivery(allocated, releaseOnce);
}

function requireAllocatedResponseView<Payload extends C2ResponsePayload>(value: unknown, byteLength: number): C2AllocatedResponsePayload<Payload> {
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
  "x-c2-call-id",
  "x-c2-expected-crm-ns",
  "x-c2-expected-crm-name",
  "x-c2-expected-crm-ver",
  "x-c2-expected-abi-hash",
  "x-c2-expected-signature-hash",
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
    if (C2_RESERVED_HTTP_HEADER_NAMES.has(name.toLowerCase()) || name.toLowerCase().startsWith("x-c2-meta-")) {
      throw new Error(`${label} cannot override reserved C-Two HTTP header ${name}.`);
    }
    normalized[name] = value;
  }
  return normalized;
}

function requestHeadersForRouteContract(customHeaders: Readonly<Record<string, string>>, routeContract: C2RouteContractIdentity, callId?: string, metadata?: Readonly<Record<string, string>>): Readonly<Record<string, string>> {
  const headers = Object.create(null) as Record<string, string>;
  for (const [name, value] of Object.entries(customHeaders)) {
    headers[name] = value;
  }
  if (callId !== undefined) headers["x-c2-call-id"] = callId;
  for (const [key, value] of Object.entries(metadata ?? {})) headers[`x-c2-meta-${key}`] = value;
  headers["Content-Type"] = "application/octet-stream";
  headers["x-c2-expected-crm-ns"] = routeContract.namespace;
  headers["x-c2-expected-crm-name"] = routeContract.name;
  headers["x-c2-expected-crm-ver"] = routeContract.version;
  headers["x-c2-expected-abi-hash"] = routeContract.abiHash;
  headers["x-c2-expected-signature-hash"] = routeContract.signatureHash;
  return headers;
}

function controlRoutePath(operation: "_resolve" | "_probe", routeName: string, query = ""): string {
  if (routeName === "." || routeName === "..") {
    return `/${operation}?route=${encodeQueryValue(routeName)}${query ? "&" + query : ""}`;
  }
  return `/${operation}/${encodePathSegment(routeName)}${query ? "?" + query : ""}`;
}

function projectHttpCallError(status: number, bytes: Uint8Array): Error {
  const decoded = decodeHttpErrorBody(bytes);
  if (status === 500 && decoded.validUtf8 && decoded.body.startsWith("C2E1")) {
    const failure = parseCanonicalRelayError(decoded.body.slice(4));
    if (failure !== undefined) {
      return new C2CoreError(failure.message, {
        message: failure.message, code: failure.code, canonicalName: failure.name, details: stripCallbackDetailsRecord(failure.details),
      });
    }
  }
  const relay = new C2HttpRelayError(status, bytes, undefined, decoded);
  if (relay.transportPhase === "pre_dispatch") {
    const refusal = parseCanonicalRelayError(decoded.body);
    if (refusal !== undefined && (refusal.code === 721 || refusal.code === 723 ||
        refusal.code === 724 || isMetadataRefusal(refusal.code, refusal.details))) {
      return new C2CoreError(refusal.message, {
        message: refusal.message, code: refusal.code, canonicalName: refusal.name,
        details: refusal.details, transportPhase: "pre_dispatch",
      });
    }
  }
  return relay;
}

function decodeCallOutcome(bytes: Uint8Array): C2CallOutcome {
  const value: unknown = JSON.parse(C2_STRICT_UTF8_DECODER.decode(bytes));
  if (value === null || typeof value !== "object" || Array.isArray(value)) throw new Error("invalid outcome object");
  const record = value as Record<string, unknown>;
  const { status, method_name, result_available, result_unavailable_reason, error_code, error_details_available } = record;
  if (Object.keys(record).length !== 6 ||
      (status !== "unknown" && status !== "in_flight" && status !== "succeeded" && status !== "failed") ||
      (method_name !== null && typeof method_name !== "string") ||
      typeof result_available !== "boolean" || typeof error_details_available !== "boolean" ||
      (result_unavailable_reason !== null && result_unavailable_reason !== "not_retained" && result_unavailable_reason !== "evicted") ||
      (error_code !== null && (typeof error_code !== "number" || !Number.isInteger(error_code) || !Object.hasOwn(C2_ERROR_CODE_NAMES, error_code)))) {
    throw new Error("invalid outcome fields");
  }
  if (method_name !== null) {
    if (!isWellFormedUnicodeString(method_name)) throw new Error("invalid outcome method_name");
    requireMethodPathValue(method_name);
  }
  const hasResult = result_available || result_unavailable_reason !== null;
  if ((status === "unknown") !== (method_name === null) ||
      (status === "succeeded") !== hasResult || (result_available && result_unavailable_reason !== null) ||
      (status === "failed") !== (error_code !== null) || (error_details_available && status !== "failed")) {
    throw new Error(`inconsistent ${status} outcome`);
  }
  return Object.freeze({ status, method_name, result_available, result_unavailable_reason, error_code, error_details_available });
}

async function queryResolvedHttpRoute(
  fetchImpl: C2Fetch,
  relayUrl: string,
  routeName: string,
  routeContract: C2RouteContractIdentity,
  callId: string,
  headers: Readonly<Record<string, string>>,
  timeoutMs: number,
  metadata: C2CallBudget["metadata"],
): Promise<C2CallOutcome> {
  return fetchWithTimeout(async (signal, internalSignal) => {
    let response: C2FetchResponse;
    try {
      response = await fetchImpl(`${relayUrl}/_c2/calls/outcome?route=${encodeQueryValue(routeName)}`, {
        method: "GET", headers: requestHeadersForRouteContract(headers, routeContract, callId, metadata), signal,
        redirect: "error",
      });
    } catch (error) {
      throw new C2HttpTransportError(`C-Two relay outcome fetch failed: ${formatCaughtError(error)}`, error);
    }
    let bytes: Uint8Array;
    try {
      // Match c2-http's 1 KiB outcome JSON and 8 KiB control error envelope limits.
      const limit = response.status === 200 ? 1024 : 8192;
      if (response.body != null) {
        bytes = await readBoundedStreamBytes(response.body, limit, internalSignal ?? signal);
      } else {
        const length = responseContentLength(response);
        if (length === undefined || length > limit || !responseHasIdentityContentEncoding(response)) {
          throw new Error("bounded outcome response requires a body stream or a bounded Content-Length");
        }
        bytes = await readHttpArrayBufferResponsePayload(response, internalSignal ?? signal);
        if (bytes.byteLength !== length) throw new Error("outcome body length does not match Content-Length");
      }
      if (response.status === 200) return decodeCallOutcome(bytes);
    } catch (error) {
      throw new C2HttpTransportError(`invalid relay outcome response: ${formatCaughtError(error)}`, undefined, "dispatch_uncertain");
    }
    throw projectHttpCallError(response.status, bytes);
  }, timeoutMs, "C-Two relay outcome query", "dispatch_uncertain");
}

async function probeResolvedHttpRoute(
  fetchImpl: C2Fetch,
  relayUrl: string,
  routeName: string,
  routeContract: C2RouteContractIdentity,
  headers: Readonly<Record<string, string>>,
  timeoutMs: number,
): Promise<void> {
  await fetchWithTimeout(async (fetchSignal, internalSignal) => {
    const activeSignal = internalSignal ?? fetchSignal;
    let response: C2FetchResponse;
    try {
      response = await fetchImpl(
        `${relayUrl}${controlRoutePath("_probe", routeName)}`,
        {
          method: "GET",
          redirect: "error",
          headers: requestHeadersForRouteContract(headers, routeContract),
          signal: fetchSignal,
        },
      );
    } catch (error) {
      throw new C2HttpTransportError(`C-Two HTTP relay probe fetch failed: ${formatCaughtError(error)}`, error, "pre_dispatch");
    }
    if (response.status === 200) {
      return;
    }
    let bodyBytes: Uint8Array;
    try {
      bodyBytes = await readHttpArrayBufferResponsePayload(response, activeSignal);
    } catch (error) {
      throw new C2HttpTransportError(`C-Two HTTP relay probe body read failed: ${formatCaughtError(error)}`, error, "pre_dispatch");
    }
    throw projectHttpAcquisitionError(response.status, bodyBytes);
  }, timeoutMs, "C-Two HTTP relay probe");
}

async function callResolvedHttpRoute<Payload extends C2ResponsePayload>(
  fetchImpl: C2Fetch,
  relayUrl: string,
  routeName: string,
  routeContract: C2RouteContractIdentity,
  method: string,
  requestPayload: Uint8Array | undefined,
  callId: string | undefined,
  headers: Readonly<Record<string, string>>,
  callTimeoutMs: number,
  metadata: C2CallBudget["metadata"],
  responsePayloadAllocator: C2ResponsePayloadAllocator<Payload> | undefined,
  responsePayloadUnknownLengthStrategy: C2ResponsePayloadUnknownLengthStrategy,
  responsePayloadUnknownLengthMaxBytes: number,
): Promise<C2PendingResponseDelivery<Payload>> {
  let trackedCleanup: Promise<void> | undefined;
  return await fetchWithTimeout(
    async (fetchSignal, internalSignal) => {
      const activeSignal = internalSignal ?? fetchSignal;
      const abortContext: C2HttpAbortContext = {
        signal: activeSignal,
        trackAllocationCleanup(done) {
          trackedCleanup = done;
        },
      };
      let response: C2FetchResponse;
      const operation = callId === undefined ? "call" : requestPayload === undefined ? "recover" : "keyed";
      try {
        response = await fetchImpl(
          `${relayUrl}/_c2/calls/${operation}?route=${encodeQueryValue(routeName)}&method=${encodeQueryValue(method)}`,
          {
            method: "POST",
            redirect: "error",
            headers: requestHeadersForRouteContract(headers, routeContract, callId, metadata),
            ...(requestPayload === undefined ? {} : { body: requestPayload }),
            signal: fetchSignal,
          },
        );
      } catch (error) {
        throw new C2HttpTransportError(`C-Two HTTP relay call fetch failed: ${formatCaughtError(error)}`, error);
      }
      if (response.status === 200) {
        try {
          return await readHttpSuccessResponsePayload(
            response,
            responsePayloadAllocator,
            responsePayloadUnknownLengthStrategy,
            responsePayloadUnknownLengthMaxBytes,
            abortContext,
          );
        } catch (error) {
          throw new C2HttpTransportError(
            `C-Two HTTP relay call body read failed: ${formatCaughtError(error)}`,
            undefined,
            "dispatch_uncertain",
          );
        }
      }
      let bodyBytes: Uint8Array;
      try {
        const controlLimit = C2_HTTP_CONTROL_CODES[response.status] === undefined ? undefined : 8192;
        if (controlLimit !== undefined && response.body != null) {
          bodyBytes = await readBoundedStreamBytes(response.body, controlLimit, activeSignal);
        } else {
          const declaredLength = responseContentLength(response);
          if (controlLimit !== undefined && declaredLength !== undefined && declaredLength > controlLimit) {
            throw new Error("relay control error body exceeds 8192 bytes");
          }
          bodyBytes = await readHttpArrayBufferResponsePayload(response, activeSignal);
          if (controlLimit !== undefined && bodyBytes.byteLength > controlLimit) {
            throw new Error("relay control error body exceeds 8192 bytes");
          }
        }
      } catch (error) {
        throw new C2HttpTransportError(
          `C-Two HTTP relay call body read failed: ${formatCaughtError(error)}`,
          undefined,
          "dispatch_uncertain",
        );
      }
      throw projectHttpCallError(response.status, bodyBytes);
    },
    callTimeoutMs,
    "C-Two HTTP relay call",
    "dispatch_uncertain",
    (lateDelivery) => lateDelivery.release(),
    () => trackedCleanup,
  );
}

const CALLBACK_CONTROL_DETAIL_KEYS = new Set([
  "transport_phase",
  "dispatch_phase",
  "fallback_eligible",
  "route_withdrawal",
]);

function stripCallbackDetailsRecord(
  details: Readonly<Record<string, string>> | undefined,
): Record<string, string> {
  const out: Record<string, string> = {};
  if (details === undefined || details === null || typeof details !== "object") {
    return out;
  }
  for (const [key, value] of Object.entries(details)) {
    if (!CALLBACK_CONTROL_DETAIL_KEYS.has(key) && typeof value === "string") {
      out[key] = value;
    }
  }
  return out;
}

function stripNativeControlDetails(details: C2NativeErrorDetails): C2NativeErrorDetails {
  const { transportPhase: _dropped, details: nested, ...rest } = details;
  return {
    ...rest,
    details: stripCallbackDetailsRecord(nested),
  };
}

function hasCallbackControlDetailKey(record: unknown): boolean {
  if (record === undefined || record === null || typeof record !== "object") {
    return false;
  }
  for (const key of CALLBACK_CONTROL_DETAIL_KEYS) {
    if (Object.prototype.hasOwnProperty.call(record, key)) {
      return true;
    }
  }
  return false;
}

function sanitizePostDispatchCallbackError(error: unknown): unknown {
  if (error instanceof C2HttpRelayError) {
    return error.transportPhase === "dispatch_uncertain"
      ? error
      : C2HttpRelayError._postDispatchCopy(error);
  }
  if (error instanceof C2HttpTransportError) {
    return error.transportPhase === "dispatch_uncertain" && !error.cannotConnect
      ? error
      : C2HttpTransportError._postDispatchCopy(error);
  }
  if (error instanceof C2UnsupportedCallModeError) {
    const rebuilt = new C2CoreError(error.message, stripNativeControlDetails(error.nativeDetails));
    rebuilt.cause = error;
    return rebuilt;
  }
  if (error instanceof C2CoreError) {
    if (
      !(error instanceof C2RouteConflictError) &&
      error.transportPhase === null &&
      !hasCallbackControlDetailKey(error.details)
    ) {
      return error;
    }
    const rebuilt = new C2CoreError(error.message, stripNativeControlDetails(error.nativeDetails));
    rebuilt.cause = error;
    return rebuilt;
  }
  if (error instanceof C2IpcTransportError) {
    if (error.transportPhase === null && !hasCallbackControlDetailKey(error.details)) {
      return error;
    }
    const cleanNative = error.nativeDetails === undefined ? undefined : stripNativeControlDetails(error.nativeDetails);
    const rebuilt = new C2IpcTransportError(error.message, cleanNative);
    rebuilt.cause = error;
    return rebuilt;
  }
  return error;
}

function observeTransportResponse<Payload extends C2ResponsePayload>(
  delivery: C2PendingResponseDelivery<Payload>,
  observe: C2TransportObserver | undefined,
  getObservation: () => C2TransportObservation,
): Payload {
  try {
    if (observe !== undefined) {
      observe(Object.freeze({ ...getObservation() }));
    }
    return delivery.payload;
  } catch (error) {
    delivery.release();
    throw sanitizePostDispatchCallbackError(error);
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
  if (value.expectedServerIdentity !== undefined || value.observationPath !== undefined || value.observe !== undefined) {
    throw new TypeError("C-Two relay-aware ipc expected route facts and observation are owned by relay resolution.");
  }
  return value;
}

function selectLocalIpcRoute<Payload extends C2ResponsePayload>(routes: readonly C2RelayRouteInfo[], anchorUrl: string, ipcOptions: C2IpcTransportOptions<Payload> | undefined, excludedLocalEndpoints: ReadonlySet<string>): C2RelayRouteInfo | undefined {
  if (ipcOptions === undefined || !relayAnchorAllowsLocalIpc(anchorUrl)) return undefined;
  return routes.find(route => route.ipcAddress !== undefined && route.serverId !== undefined && route.serverInstanceId !== undefined && !localRouteEndpointIsExcluded(route, excludedLocalEndpoints));
}

function relayAnchorAllowsLocalIpc(anchorUrl: string): boolean {
  return isRelayLoopback(new URL(anchorUrl));
}

function isRelayLoopback(url: URL): boolean {
  const hostname = url.hostname;
  return hostname === "localhost" || /^127(?:\.\d{1,3}){3}$/.test(hostname) || hostname === "[::1]";
}

function localRouteEndpointKey(route: C2RelayRouteInfo): string {
  return JSON.stringify([route.ipcAddress, route.serverId, route.serverInstanceId]);
}

function localRouteEndpointIsExcluded(route: C2RelayRouteInfo, excluded: ReadonlySet<string>): boolean {
  return route.ipcAddress !== undefined && route.serverId !== undefined && route.serverInstanceId !== undefined && excluded.has(localRouteEndpointKey(route));
}

function localCandidateFailureIsTerminal(error: unknown): boolean {
  if (error instanceof C2RouteConflictError) return true;
  return error instanceof C2IpcTransportError && (
    error.category === "protocol_version_mismatch" || error.category === "runtime_retired" ||
    error.code === 709 || error.code === 710 || error.code === 713 || error.code === 715
  );
}

function samePathFallbackDeniedError(
  routeName: string,
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
      },
    }),
    "pre_dispatch",
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
      details: { route: routeName, dispatch_phase: "pre_dispatch" },
    }),
    "pre_dispatch",
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

function routeContractKey(routeName: string, contract: C2RouteContractIdentity): string {
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

function createFallbackAbortController(): C2AbortController {
  let aborted = false;
  let reason: unknown;
  const listeners = new Set<() => void>();
  const signal: C2AbortSignal = {
    get aborted() {
      return aborted;
    },
    get reason() {
      return reason;
    },
    addEventListener(_type, listener) {
      if (aborted) return;
      listeners.add(listener);
    },
    removeEventListener(_type, listener) {
      listeners.delete(listener);
    },
  };
  return {
    signal,
    abort(abortReasonValue?: unknown) {
      if (aborted) return;
      aborted = true;
      reason = abortReasonValue;
      const snapshot = [...listeners];
      listeners.clear();
      for (const listener of snapshot) {
        try {
          listener();
        } catch {
          // Ignore listener errors during abort notification.
        }
      }
    },
  };
}

async function fetchWithTimeout<T>(
  operation: (fetchSignal: C2AbortSignal | undefined, internalSignal?: C2AbortSignal) => Promise<T>,
  timeoutMs: number,
  label: string,
  phase: "pre_dispatch" | "dispatch_uncertain" = "pre_dispatch",
  onLateResult?: (value: T) => void,
  getPendingCleanup?: () => Promise<void> | undefined,
): Promise<T> {
  if (timeoutMs === 0) {
    return await operation(undefined, undefined);
  }
  const controllerCtor = (globalThis as unknown as { AbortController?: new () => C2AbortController }).AbortController;
  const nativeController = controllerCtor ? new controllerCtor() : undefined;
  const internalController = nativeController ?? createFallbackAbortController();
  let timedOut = false;
  let timeoutHandle: ReturnType<typeof setTimeout> | undefined;
  const timeoutError = () => callDeadlineError(phase, `${label} timed out after ${timeoutMs}ms.`);
  const timeoutPromise = new Promise<never>((_resolve, reject) => {
    const maxTimerMs = 2_147_483_647;
    const started = performance.now();
    const expire = (): void => {
      // Longer waits need multiple timers; shorter waits use the original timer.
      if (timeoutMs > maxTimerMs) {
        const remaining = timeoutMs - (performance.now() - started);
        if (remaining > 0) {
          timeoutHandle = setTimeout(expire, Math.min(remaining, maxTimerMs));
          return;
        }
      }
      timedOut = true;
      const error = timeoutError();
      internalController.abort(error);
      reject(error);
    };
    timeoutHandle = setTimeout(expire, Math.min(timeoutMs, maxTimerMs));
  });
  const opPromise = operation(nativeController?.signal, internalController.signal);
  if (onLateResult !== undefined) {
    void opPromise.then(
      (value) => {
        if (timedOut) {
          try {
            onLateResult(value);
          } catch {
            // Ignore late cleanup failures after timeout rejection.
          }
        }
      },
      () => undefined,
    );
  }
  try {
    return await Promise.race([opPromise, timeoutPromise]);
  } catch (error) {
    if (timedOut) {
      const pendingCleanup = getPendingCleanup?.();
      if (pendingCleanup !== undefined) {
        await pendingCleanup;
      }
      throw timeoutError();
    }
    throw error;
  } finally {
    if (timeoutHandle !== undefined) {
      clearTimeout(timeoutHandle);
    }
  }
}

async function resolveRelayRoutes(
  fetchImpl: C2Fetch,
  anchorUrl: string,
  routeName: string,
  contract: C2RouteContractIdentity,
  headers: Readonly<Record<string, string>>,
  resolveTimeoutMs: number,
): Promise<readonly C2RelayRouteInfo[]> {
  const query = `crm_ns=${encodeQueryValue(contract.namespace)}&crm_name=${encodeQueryValue(contract.name)}&crm_ver=${encodeQueryValue(contract.version)}&abi_hash=${encodeQueryValue(contract.abiHash)}&signature_hash=${encodeQueryValue(contract.signatureHash)}`;
  const resolveUrl = anchorUrl + controlRoutePath("_resolve", routeName, query);
  return await fetchWithTimeout(async (fetchSignal, internalSignal) => {
    const activeSignal = internalSignal ?? fetchSignal;
    let response: C2FetchResponse;
    try {
      response = await fetchImpl(resolveUrl, {
        method: "GET",
        redirect: "error",
        headers,
        signal: fetchSignal,
      });
    } catch (error) {
      throw new C2HttpTransportError(`C-Two relay resolve fetch failed: ${formatCaughtError(error)}`, error, "pre_dispatch");
    }
    if (response.status !== 200) {
      let bodyBytes: Uint8Array;
      try {
        bodyBytes = await readHttpArrayBufferResponsePayload(response, activeSignal);
      } catch (error) {
        throw new C2HttpTransportError(`C-Two relay resolve body read failed: ${formatCaughtError(error)}`, error, "pre_dispatch");
      }
      throw projectHttpAcquisitionError(response.status, bodyBytes);
    }
    let bodyBytes: Uint8Array;
    try {
      bodyBytes = await readHttpArrayBufferResponsePayload(response, activeSignal);
    } catch (error) {
      throw new C2HttpTransportError(`C-Two relay resolve body read failed: ${formatCaughtError(error)}`, error, "pre_dispatch");
    }
    let body: string;
    try {
      body = C2_STRICT_UTF8_DECODER.decode(bodyBytes);
    } catch (error) {
      throw new C2HttpTransportError(`C-Two relay resolve returned invalid UTF-8: ${formatCaughtError(error)}`, error, "pre_dispatch");
    }
    let payload: unknown;
    try {
      payload = JSON.parse(body) as unknown;
    } catch (error) {
      throw new C2HttpTransportError(`C-Two relay resolve returned invalid JSON: ${formatCaughtError(error)}`, error, "pre_dispatch");
    }
    if (!Array.isArray(payload)) {
      throw new C2HttpTransportError("C-Two relay resolve returned a non-array route response.", undefined, "pre_dispatch");
    }
    let routes: C2RelayRouteInfo[];
    try {
      routes = payload.map((route, index) => normalizeRelayRouteInfo(route, index));
    } catch (error) {
      throw new C2HttpTransportError(`C-Two relay resolve payload shape invalid: ${formatCaughtError(error)}`, error, "pre_dispatch");
    }
    try {
      for (let index = 0; index < routes.length; index += 1) {
        validateRelayRouteContract(routes[index], index, routeName, contract);
      }
    } catch (error) {
      throw new C2HttpTransportError(`C-Two relay resolve contract invalid: ${formatCaughtError(error)}`, error, "pre_dispatch");
    }
    if (new URL(anchorUrl).protocol === "https:") {
      for (const route of routes) {
        const target = new URL(route.relayUrl);
        if (target.protocol === "http:" && !isRelayLoopback(target)) {
          throw new C2HttpTransportError("HTTPS relay resolved to a non-loopback HTTP target.", undefined, "pre_dispatch");
        }
      }
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
    ipcAddress,
    serverId,
    serverInstanceId,
    crmNs: stringField(item, "crm_ns", index),
    crmName: stringField(item, "crm_name", index),
    crmVer: stringField(item, "crm_ver", index),
    abiHash: stringField(item, "abi_hash", index),
    signatureHash: stringField(item, "signature_hash", index),
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

function preDispatchRouteCode(error: unknown): number | undefined {
  if (error instanceof C2IpcTransportError || error instanceof C2CoreError) {
    return error.transportPhase === "pre_dispatch" && error.code !== null && error.code !== undefined && error.code !== 0
      ? error.code
      : undefined;
  }
  if (error instanceof C2HttpRelayError) {
    if (!error.hasVerifiedUtf8Body || error.transportPhase !== "pre_dispatch") {
      return undefined;
    }
    const canonical = parseCanonicalRelayError(error.body);
    if (canonical !== undefined && canonical.code !== 0) {
      return canonical.code;
    }
  }
  return undefined;
}

function isMetadataRefusal(code: number, details: Readonly<Record<string, string>>): boolean {
  return code === 713 && ownDetail(details, "stage") === "call_metadata_validation";
}

function terminalRouteRefusal(error: unknown): boolean {
  if (error instanceof C2IpcTransportError && (error.category === "protocol_version_mismatch" || error.category === "runtime_retired")) return true;
  const code = preDispatchRouteCode(error);
  if (code !== 713) return code === 710;
  const details = error instanceof C2HttpRelayError
    ? parseCanonicalRelayError(error.body)?.details
    : error instanceof C2CoreError || error instanceof C2IpcTransportError ? error.details : undefined;
  return details === undefined || !isMetadataRefusal(code, details);
}

function permitsRouteRetry(code: number | null, details: Readonly<Record<string, string>>): boolean {
  return code !== 724 && (code !== 713 || !isMetadataRefusal(code, details));
}

function isRetryableRouteError(error: C2HttpRelayError): boolean {
  const code = preDispatchRouteCode(error);
  return code !== undefined && permitsRouteRetry(code, parseCanonicalRelayError(error.body)?.details ?? {});
}

function httpConnectFailure(error: unknown): boolean {
  if (!(error instanceof Error)) return false;
  const cause = error as Error & { code?: unknown; syscall?: unknown };
  if (cause.code === "UND_ERR_CONNECT_TIMEOUT") return true;
  if (cause.syscall === "connect" && (cause.code === "ECONNREFUSED" || cause.code === "ENETUNREACH" || cause.code === "EHOSTUNREACH")) return true;
  if (cause.syscall === "getaddrinfo" && (cause.code === "ENOTFOUND" || cause.code === "EAI_AGAIN")) return true;
  return httpConnectFailure(cause.cause);
}

const C2_ERROR_CODE_NAMES: Readonly<Record<number, string>> = /* __C2_GENERATED_ERROR_CODE_NAMES__ */ Object.freeze({});

function parseCanonicalRelayError(
  body: string,
): { readonly version: 1; readonly code: number; readonly name: string; readonly message: string; readonly details: Readonly<Record<string, string>> } | undefined {
  try {
    const parsed = JSON.parse(body) as unknown;
    if (typeof parsed !== "object" || parsed === null || Array.isArray(parsed)) {
      return undefined;
    }
    const record = parsed as Record<string, unknown>;
    if (
      record.version !== 1 ||
      typeof record.code !== "number" ||
      !Number.isSafeInteger(record.code) ||
      record.code < 0 ||
      record.code > 65535 ||
      typeof record.name !== "string" ||
      typeof record.message !== "string"
    ) {
      return undefined;
    }
    if (
      typeof record.details !== "object" ||
      record.details === null ||
      Array.isArray(record.details)
    ) {
      return undefined;
    }
    const rawDetails = record.details as Record<string, unknown>;
    for (const value of Object.values(rawDetails)) {
      if (typeof value !== "string") {
        return undefined;
      }
    }
    const rawCode = record.code;
    const stringDetails = rawDetails as Readonly<Record<string, string>>;
    const expectedName = Object.prototype.hasOwnProperty.call(C2_ERROR_CODE_NAMES, rawCode)
      ? C2_ERROR_CODE_NAMES[rawCode]
      : undefined;
    if (expectedName !== undefined) {
      if (record.name !== expectedName) {
        return undefined;
      }
      return {
        version: 1,
        code: rawCode,
        name: record.name,
        message: record.message,
        details: stringDetails,
      };
    }
    const degradedDetails: Record<string, string> = { ...stringDetails };
    if (ownDetail(degradedDetails, "unknown_code") === undefined) {
      degradedDetails.unknown_code = String(rawCode);
    }
    if (ownDetail(degradedDetails, "unknown_name") === undefined) {
      degradedDetails.unknown_name = record.name;
    }
    return {
      version: 1,
      code: 0,
      name: "Unknown",
      message: `Unknown error code ${rawCode} (${record.name}): ${record.message}`,
      details: degradedDetails,
    };
  } catch {
    return undefined;
  }
}

function sanitizeAcquisitionDetails(details: Readonly<Record<string, string>>): Readonly<Record<string, string>> {
  const out: Record<string, string> = {};
  for (const [key, value] of Object.entries(details)) {
    if ((key === "dispatch_phase" || key === "transport_phase") && value !== "pre_dispatch") {
      continue;
    }
    out[key] = value;
  }
  return out;
}

function projectHttpAcquisitionError(status: number, rawBody: string | Uint8Array): Error {
  const decoded = decodeHttpErrorBody(rawBody);
  if (decoded.validUtf8) {
    const canonical = parseCanonicalRelayError(decoded.body);
    if (status === 409 && canonical?.code === 719) {
      return new C2RouteConflictError(canonical.message, sanitizeAcquisitionDetails(canonical.details));
    }
  }
  return new C2HttpRelayError(status, rawBody, "pre_dispatch");
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
