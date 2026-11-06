import { createRequire } from "node:module";

export const C2_MEM_FFI_ABI_VERSION = 8;

export interface NativeErrorDetails {
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

/** A local Rust-owned response. Release detaches the ArrayBuffer before freeing it. */
export interface NativeBuffer extends Uint8Array {
  release(): void;
}

export interface NativeStreamReceipt {
  readonly completed: boolean;
  readonly error: Error | null;
  readonly completeItems: bigint | null;
}

export interface NativeCall {
  chargeInput(byteLength: bigint | number): Promise<void>;
  dispatch(bytes: Uint8Array): Promise<void>;
  next(): Promise<NativeBuffer | null>;
  cancel(): void;
  observe(timeoutMs: number | null): Promise<NativeStreamReceipt>;
  checkDeadline(): void;
  /** Frees the facade and submits cancellation without waiting for cleanup. */
  close(): void;
}

export interface NativePreparedRoute {
  readonly serverId: string;
  readonly serverInstanceId: string;
  readonly routeUid: string;
  readonly connectionId: bigint | string;
}

export interface NativeEndpointContext {
  prepare(routeName: string, contract: object, expectedRouteToken?: { readonly routeUid: string }): Promise<NativePreparedRoute>;
  beginCall(routeName: string, method: string, serverStream: boolean, timeoutMs: number | null): NativeCall;
  deactivate(): void;
  close(): Promise<void>;
}

export interface NativeEndpointOptions {
  readonly connectTimeoutMs?: number;
  readonly expectedServerIdentity?: { readonly serverId: string; readonly serverInstanceId: string };
}

export interface NativeClientDomainOptions {
  readonly requestShmThreshold?: number;
  readonly requestChunkSize?: number;
}

export interface NativeClientDomain {
  createEndpoint(address: string, options: NativeEndpointOptions): NativeEndpointContext;
  stats(): object;
  close(): void;
}

export interface NativeLocalEndpointContext {
  endpointName(address: string): string;
  namespaceId(): string;
  close(): void;
}

export interface C2MemFfiNodeNativeSymbols {
  c2_mem_ffi_abi_version(): number;
  captureEndpointContext(ipcRoot: string | null): NativeLocalEndpointContext;
  createClientDomain(context: NativeLocalEndpointContext | null, options: NativeClientDomainOptions): NativeClientDomain;
}

export interface C2MemFfiNodeNativeLoadOptions {
  readonly addonPath?: string;
}

export interface C2MemFfiLocalEndpointCaptureOptions {
  readonly ipcRoot?: string;
}

const endpointContextBrand: unique symbol = Symbol("C-Two local endpoint context");

export interface C2MemFfiLocalEndpointContext {
  readonly [endpointContextBrand]: true;
  endpointName(address: string): string;
  namespaceId(): string;
  close(): void;
}

export interface C2MemFfiLocalEndpointOptions extends C2MemFfiNodeNativeLoadOptions, C2MemFfiLocalEndpointCaptureOptions {
  readonly endpointContext?: C2MemFfiLocalEndpointContext;
  /** For a loaded ABI 8 addon or a test double; no raw socket injection exists. */
  readonly nativeSymbols?: C2MemFfiNodeNativeSymbols;
}

export interface C2NodeIpcConnectorOptions extends C2MemFfiLocalEndpointOptions, NativeEndpointOptions, NativeClientDomainOptions {}

export type C2NodeIpcConnector = ((address: string, timeoutMs?: number, nativeOptions?: NativeEndpointOptions) => Promise<NativeEndpointContext>) & {
  readonly resolveEndpoint: (address: string) => string;
  readonly stats: () => object;
  readonly close: () => void;
};

interface C2MemFfiNodeNativeAddon {
  load(libraryPath: string): C2MemFfiNodeNativeSymbols;
}

export class C2MemFfiBindingError extends Error {
  constructor(message: string) {
    super(message);
    this.name = "C2MemFfiBindingError";
  }
}

export class C2NodeIpcConnectionError extends Error {
  constructor(message: string) {
    super(message);
    this.name = "C2NodeIpcConnectionError";
  }
}

export class C2NativeError extends Error {
  readonly nativeDetails: NativeErrorDetails;
  readonly code: number | undefined;
  readonly canonicalName: string | undefined;
  readonly transportPhase: NativeErrorDetails["transportPhase"];
  readonly category: string | undefined;
  readonly details: Readonly<Record<string, string>>;
  readonly localProtocolVersion: number | undefined;
  readonly peerProtocolVersion: number | undefined;
  readonly ioOperation: string | undefined;
  readonly ioKind: string | undefined;
  readonly rawOsError: number | undefined;
  constructor(details: NativeErrorDetails, cause?: Error) {
    super(details.message, cause === undefined ? undefined : { cause });
    this.name = details.canonicalName ?? "C2NativeError";
    this.nativeDetails = details;
    this.code = details.code;
    this.canonicalName = details.canonicalName;
    this.transportPhase = details.transportPhase;
    this.category = details.category;
    this.details = details.details ?? {};
    this.localProtocolVersion = details.localProtocolVersion;
    this.peerProtocolVersion = details.peerProtocolVersion;
    this.ioOperation = details.ioOperation;
    this.ioKind = details.ioKind;
    this.rawOsError = details.rawOsError;
  }
}

function projectNativeError(error: unknown): Error {
  if (error instanceof C2NativeError) return error;
  if (error instanceof Error) {
    const details = (error as Error & { nativeDetails?: NativeErrorDetails }).nativeDetails;
    if (details !== undefined && typeof details.message === "string") return new C2NativeError(details, error);
    return error;
  }
  return new C2MemFfiBindingError(String(error));
}

async function nativePromise<T>(invoke: () => Promise<T>): Promise<T> {
  try { return await invoke(); }
  catch (error) { throw projectNativeError(error); }
}

function nativeValue<T>(get: () => T): T {
  try { return get(); }
  catch (error) { throw projectNativeError(error); }
}

class ProjectedCall implements NativeCall {
  constructor(private readonly native: NativeCall) {}
  chargeInput(byteLength: bigint | number): Promise<void> { return nativePromise(() => this.native.chargeInput(byteLength)); }
  dispatch(bytes: Uint8Array): Promise<void> { return nativePromise(() => this.native.dispatch(bytes)); }
  next(): Promise<NativeBuffer | null> { return nativePromise(() => this.native.next()); }
  cancel(): void { nativeValue(() => this.native.cancel()); }
  async observe(timeoutMs: number | null): Promise<NativeStreamReceipt> {
    const receipt = await nativePromise(() => this.native.observe(timeoutMs));
    return { completed: receipt.completed, error: receipt.error === null ? null : projectNativeError(receipt.error), completeItems: receipt.completeItems };
  }
  checkDeadline(): void { nativeValue(() => this.native.checkDeadline()); }
  close(): void { nativeValue(() => this.native.close()); }
}

class ProjectedEndpoint implements NativeEndpointContext {
  constructor(private readonly native: NativeEndpointContext) {}
  prepare(routeName: string, contract: object, expectedRouteToken?: { readonly routeUid: string }): Promise<NativePreparedRoute> {
    return nativePromise(() => this.native.prepare(routeName, contract, expectedRouteToken));
  }
  beginCall(routeName: string, method: string, serverStream: boolean, timeoutMs: number | null): NativeCall {
    return new ProjectedCall(nativeValue(() => this.native.beginCall(routeName, method, serverStream, timeoutMs)));
  }
  deactivate(): void { nativeValue(() => this.native.deactivate()); }
  close(): Promise<void> { return nativePromise(() => this.native.close()); }
}

interface ContextState { readonly native: NativeLocalEndpointContext; references: number; }
interface ContextLease { readonly state: ContextState; closed: boolean; }
const contextStates = new WeakMap<C2MemFfiLocalEndpointContext, ContextLease>();

function releaseContext(lease: ContextLease): void {
  if (lease.closed) return;
  lease.closed = true;
  if (--lease.state.references === 0) lease.state.native.close();
}

const contextFinalizer = new FinalizationRegistry<ContextLease>(releaseContext);

function leaseContext(state: ContextState): ContextLease {
  state.references++;
  return { state, closed: false };
}

function nativeContext(lease: ContextLease): NativeLocalEndpointContext {
  if (lease.closed) throw new C2NodeIpcConnectionError("C-Two endpoint context is closed.");
  return lease.state.native;
}

function validateCaptureOptions(options: C2MemFfiLocalEndpointCaptureOptions): void {
  if (typeof options !== "object" || options === null) throw new C2NodeIpcConnectionError("C-Two endpoint options must be an object.");
  if (options.ipcRoot !== undefined && (typeof options.ipcRoot !== "string" || options.ipcRoot.includes("\0"))) {
    throw new C2NodeIpcConnectionError("C-Two ipcRoot must be a string without NUL characters.");
  }
}

function validateAddress(address: string): void {
  if (typeof address !== "string" || address.includes("\0")) {
    throw new C2NodeIpcConnectionError("C-Two IPC address must be a string without NUL characters.");
  }
}

function validateSymbols(symbols: C2MemFfiNodeNativeSymbols): void {
  if (typeof symbols !== "object" || symbols === null || typeof symbols.c2_mem_ffi_abi_version !== "function") {
    throw new C2MemFfiBindingError("C-Two native addon must provide c2_mem_ffi_abi_version().");
  }
  const version = symbols.c2_mem_ffi_abi_version();
  if (version !== C2_MEM_FFI_ABI_VERSION) {
    throw new C2MemFfiBindingError(`C-Two native addon reports ABI version ${version}; expected version ${C2_MEM_FFI_ABI_VERSION}.`);
  }
  if (typeof symbols.captureEndpointContext !== "function" || typeof symbols.createClientDomain !== "function") {
    throw new C2MemFfiBindingError("C-Two native addon must provide captureEndpointContext() and createClientDomain().");
  }
}

export function captureLocalIpcEndpointContext(
  options: C2MemFfiNodeNativeLoadOptions & C2MemFfiLocalEndpointCaptureOptions = {},
): C2MemFfiLocalEndpointContext {
  validateCaptureOptions(options);
  return captureLocalIpcEndpointContextFromSymbols(loadBundledC2MemFfiNodeNativeSymbols(options).symbols, options);
}

export function captureLocalIpcEndpointContextFromSymbols(
  symbols: C2MemFfiNodeNativeSymbols,
  options: C2MemFfiLocalEndpointCaptureOptions = {},
): C2MemFfiLocalEndpointContext {
  validateCaptureOptions(options);
  validateSymbols(symbols);
  const native = nativeValue(() => symbols.captureEndpointContext(options.ipcRoot ?? null));
  if (native === null || typeof native !== "object" || typeof native.endpointName !== "function"
    || typeof native.namespaceId !== "function" || typeof native.close !== "function") {
    if (native !== null && typeof native === "object" && typeof native.close === "function") native.close();
    throw new C2MemFfiBindingError("C-Two native endpoint context capture returned an invalid owner.");
  }
  const lease = leaseContext({ native, references: 0 });
  const context: C2MemFfiLocalEndpointContext = Object.freeze({
    [endpointContextBrand]: true as const,
    endpointName(address: string): string {
      validateAddress(address);
      return nativeValue(() => nativeContext(lease).endpointName(address));
    },
    namespaceId(): string { return nativeValue(() => nativeContext(lease).namespaceId()); },
    close(): void { contextFinalizer.unregister(lease); releaseContext(lease); },
  });
  contextStates.set(context, lease);
  contextFinalizer.register(context, lease, lease);
  return context;
}

function retainedContext(context: C2MemFfiLocalEndpointContext): ContextLease {
  const source = contextStates.get(context);
  if (source === undefined) throw new C2NodeIpcConnectionError("C-Two endpointContext must be created by captureLocalIpcEndpointContext().");
  nativeContext(source);
  return leaseContext(source.state);
}

export function resolveBundledC2MemFfiNodeNativeLibraryPath(): string {
  const name = bundledLibraryName();
  return createRequire(import.meta.url).resolve(`./native/${name}`);
}

const C2_MEM_FFI_NODE_ADDON_PATH = "./native/c2_mem_ffi_node.node";
const symbolsCache = new Map<string, C2MemFfiNodeNativeSymbols>();

export function loadC2MemFfiNodeNativeSymbols(
  libraryPath: string,
  options: C2MemFfiNodeNativeLoadOptions = {},
): { readonly symbols: C2MemFfiNodeNativeSymbols } {
  if (typeof libraryPath !== "string" || libraryPath.length === 0) throw new C2MemFfiBindingError("c2-mem-ffi native libraryPath must be a non-empty string.");
  const addonPath = options.addonPath ?? C2_MEM_FFI_NODE_ADDON_PATH;
  if (typeof addonPath !== "string" || addonPath.length === 0) throw new C2MemFfiBindingError("c2-mem-ffi native addonPath must be a non-empty string.");
  const key = `${addonPath}\0${libraryPath}`;
  let symbols = symbolsCache.get(key);
  if (symbols === undefined) {
    const addon = createRequire(import.meta.url)(addonPath) as C2MemFfiNodeNativeAddon;
    if (addon === null || typeof addon !== "object" || typeof addon.load !== "function") {
      throw new C2MemFfiBindingError("c2-mem-ffi Node native addon must export load(libraryPath).");
    }
    symbols = addon.load(libraryPath);
    validateSymbols(symbols);
    symbolsCache.set(key, symbols);
  }
  return { symbols };
}

export function loadBundledC2MemFfiNodeNativeSymbols(
  options: C2MemFfiNodeNativeLoadOptions = {},
): { readonly symbols: C2MemFfiNodeNativeSymbols } {
  return loadC2MemFfiNodeNativeSymbols(resolveBundledC2MemFfiNodeNativeLibraryPath(), options);
}

function bundledLibraryName(): string {
  if (process.platform === "darwin") return "libc2_mem_ffi.dylib";
  if (process.platform === "linux") return "libc2_mem_ffi.so";
  if (process.platform === "win32") return "c2_mem_ffi.dll";
  throw new C2MemFfiBindingError(`c2-mem-ffi bundled Node native library is not available for ${process.platform}.`);
}

export function resolveLocalIpcEndpoint(address: string, options: C2MemFfiLocalEndpointOptions = {}): string {
  validateAddress(address);
  validateCaptureOptions(options);
  if (options.ipcRoot !== undefined && options.endpointContext !== undefined) {
    throw new C2NodeIpcConnectionError("C-Two ipcRoot and endpointContext options are mutually exclusive.");
  }
  if (options.endpointContext !== undefined) return options.endpointContext.endpointName(address);
  const symbols = options.nativeSymbols ?? loadBundledC2MemFfiNodeNativeSymbols(options).symbols;
  const context = captureLocalIpcEndpointContextFromSymbols(symbols, options);
  try { return context.endpointName(address); }
  finally { context.close(); }
}

interface ConnectorOwnership { domain: NativeClientDomain | undefined; lease: ContextLease | undefined; }
function releaseConnector(ownership: ConnectorOwnership): void {
  try { ownership.domain?.close(); }
  finally {
    ownership.domain = undefined;
    if (ownership.lease !== undefined) releaseContext(ownership.lease);
    ownership.lease = undefined;
  }
}
const connectorFinalizer = new FinalizationRegistry<ConnectorOwnership>(releaseConnector);

function validateConnectTimeout(value: number | undefined): void {
  if (value !== undefined && (!Number.isSafeInteger(value) || value <= 0)) {
    throw new C2NodeIpcConnectionError("C-Two connectTimeoutMs must be a positive safe integer.");
  }
}

/** One native client domain is shared by every endpoint and reconnect. */
export function createNodeIpcConnector(options: C2NodeIpcConnectorOptions = {}): C2NodeIpcConnector {
  validateCaptureOptions(options);
  validateConnectTimeout(options.connectTimeoutMs);
  const permitted = new Set([
    "addonPath", "ipcRoot", "endpointContext", "nativeSymbols", "connectTimeoutMs",
    "requestShmThreshold", "requestChunkSize", "expectedServerIdentity",
  ]);
  for (const key of Object.keys(options)) {
    if (!permitted.has(key)) throw new C2NodeIpcConnectionError(`C-Two Node IPC connector has unknown option ${key}.`);
  }
  if (options.ipcRoot !== undefined && options.endpointContext !== undefined) {
    throw new C2NodeIpcConnectionError("C-Two ipcRoot and endpointContext options are mutually exclusive.");
  }
  const symbols = options.nativeSymbols ?? loadBundledC2MemFfiNodeNativeSymbols(options).symbols;
  validateSymbols(symbols);
  const endpointOptions: NativeEndpointOptions = {
    ...(options.expectedServerIdentity === undefined ? {} : { expectedServerIdentity: options.expectedServerIdentity }),
  };
  const domainOptions: NativeClientDomainOptions = {
    ...(options.requestShmThreshold === undefined ? {} : { requestShmThreshold: options.requestShmThreshold }),
    ...(options.requestChunkSize === undefined ? {} : { requestChunkSize: options.requestChunkSize }),
  };
  const ownership: ConnectorOwnership = { lease: undefined, domain: undefined };
  let attempted = false;
  let failure: unknown;
  const lifetime = { closed: false };
  const requireOpen = (): void => {
    if (lifetime.closed) throw new C2NodeIpcConnectionError("C-Two Node IPC connector is closed.");
  };
  const freeze = (): NativeClientDomain => {
    requireOpen();
    if (!attempted) {
      attempted = true;
      try {
        if (options.endpointContext !== undefined) ownership.lease = retainedContext(options.endpointContext);
        else {
          const temporary = captureLocalIpcEndpointContextFromSymbols(symbols, options);
          try { ownership.lease = retainedContext(temporary); }
          finally { temporary.close(); }
        }
        ownership.domain = nativeValue(() => symbols.createClientDomain(nativeContext(ownership.lease!), domainOptions));
        if (ownership.domain === null || typeof ownership.domain !== "object" || typeof ownership.domain.createEndpoint !== "function"
          || typeof ownership.domain.stats !== "function" || typeof ownership.domain.close !== "function") {
          if (ownership.domain !== null && typeof ownership.domain === "object" && typeof ownership.domain.close === "function") ownership.domain.close();
          ownership.domain = undefined;
          throw new C2MemFfiBindingError("C-Two native client domain returned an invalid owner.");
        }
      } catch (error) {
        failure = projectNativeError(error);
        if (ownership.lease !== undefined) { releaseContext(ownership.lease); ownership.lease = undefined; }
      }
    }
    if (failure !== undefined) throw failure;
    return ownership.domain!;
  };
  const resolveEndpoint = (address: string): string => {
    requireOpen();
    if (failure !== undefined) throw failure;
    validateAddress(address);
    if (ownership.lease !== undefined) return nativeValue(() => nativeContext(ownership.lease!).endpointName(address));
    if (options.endpointContext !== undefined) return options.endpointContext.endpointName(address);
    const context = captureLocalIpcEndpointContextFromSymbols(symbols, options);
    try { return context.endpointName(address); }
    finally { context.close(); }
  };
  const connect = async (address: string, timeoutMs?: number, nativeOptions: NativeEndpointOptions = {}): Promise<NativeEndpointContext> => {
    validateAddress(address);
    if (typeof nativeOptions !== "object" || nativeOptions === null) {
      throw new C2NodeIpcConnectionError("C-Two native endpoint options must be an object.");
    }
    for (const key of Object.keys(nativeOptions)) {
      if (key !== "connectTimeoutMs" && key !== "expectedServerIdentity") {
        throw new C2NodeIpcConnectionError(`C-Two native endpoint has unknown option ${key}.`);
      }
    }
    validateConnectTimeout(timeoutMs);
    validateConnectTimeout(nativeOptions.connectTimeoutMs);
    const clientDomain = freeze();
    const definedOverrides = Object.fromEntries(Object.entries(nativeOptions).filter(([, value]) => value !== undefined)) as NativeEndpointOptions;
    const mergedOptions: NativeEndpointOptions = {
      ...endpointOptions,
      ...(options.connectTimeoutMs === undefined ? {} : { connectTimeoutMs: options.connectTimeoutMs }),
      ...definedOverrides,
      ...(timeoutMs === undefined ? {} : { connectTimeoutMs: timeoutMs }),
    };
    const endpoint = nativeValue(() => clientDomain.createEndpoint(address, mergedOptions));
    if (endpoint === null || typeof endpoint !== "object" || typeof endpoint.prepare !== "function"
      || typeof endpoint.beginCall !== "function" || typeof endpoint.deactivate !== "function" || typeof endpoint.close !== "function") {
      throw new C2MemFfiBindingError("C-Two native endpoint returned an invalid owner.");
    }
    return new ProjectedEndpoint(endpoint);
  };
  const connector = Object.freeze(Object.assign(connect, {
    resolveEndpoint,
    stats(): object { return nativeValue(() => freeze().stats()); },
    close(): void {
      if (lifetime.closed) return;
      lifetime.closed = true;
      connectorFinalizer.unregister(ownership);
      nativeValue(() => releaseConnector(ownership));
    },
  }));
  connectorFinalizer.register(lifetime, ownership, ownership);
  return connector;
}
