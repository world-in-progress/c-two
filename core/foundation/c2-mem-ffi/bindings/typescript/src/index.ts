import { createRequire } from "node:module";
import { createConnection, Socket } from "node:net";
import { closeSync } from "node:fs";

export const C2_MEM_FFI_STATUS_OK = 0;
export const C2_MEM_FFI_STATUS_NULL_POINTER = 1;
export const C2_MEM_FFI_STATUS_INVALID_ARGUMENT = 2;
export const C2_MEM_FFI_STATUS_POOL_ERROR = 3;
export const C2_MEM_FFI_STATUS_INSUFFICIENT_BUFFER = 4;
export const C2_MEM_FFI_STATUS_IO_ERROR = 5;

export const C2_MEM_FFI_MAX_SHM_PREFIX_BYTES = 255;
export const C2_MEM_FFI_MAX_IPC_SHM_SEGMENTS = 16;
export const C2_MEM_FFI_ABI_VERSION = 3;

export type C2MemFfiStatus =
  | typeof C2_MEM_FFI_STATUS_OK
  | typeof C2_MEM_FFI_STATUS_NULL_POINTER
  | typeof C2_MEM_FFI_STATUS_INVALID_ARGUMENT
  | typeof C2_MEM_FFI_STATUS_POOL_ERROR
  | typeof C2_MEM_FFI_STATUS_INSUFFICIENT_BUFFER
  | typeof C2_MEM_FFI_STATUS_IO_ERROR;

type MaybePromise<T> = T | Promise<T>;

export interface C2MemFfiCallResult<T = void> {
  readonly status: C2MemFfiStatus;
  readonly value?: T;
}

export interface C2MemFfiPoolConfig {
  readonly prefix: string;
  /**
   * Request pools create backings with this exact data capacity. Response
   * pools only open peer backings, so for them this is a bootstrap capacity
   * floor: c2-mem derives the real geometry from the mapped segment at
   * prefix/index/generation and validates capacity, byte range, and
   * generation. Response readers must not treat this value as a claim about
   * the server's segment size.
   */
  readonly segmentSize: number;
  readonly maxSegments: number;
  readonly minBlockSize: number;
}

export interface C2MemFfiShmSegment {
  readonly name: string;
  readonly size: number;
}

export interface C2MemFfiRequestBlock {
  readonly segmentIndex: number;
  readonly generation: number;
  readonly offset: number;
  readonly byteLength: number;
  readonly dedicated: boolean;
}

export interface C2MemFfiResponseBlock extends C2MemFfiRequestBlock {
  readonly prefix?: string;
  readonly segments?: readonly C2MemFfiShmSegment[];
}

export interface C2MemFfiNativeResponsePool {
  read(block: C2MemFfiResponseBlock, destination: Uint8Array): MaybePromise<void>;
  release(block: C2MemFfiResponseBlock): MaybePromise<void>;
  close?(): MaybePromise<void>;
}

export interface C2MemFfiNativeRequestPool {
  readonly prefix: string;
  readonly segments: readonly C2MemFfiShmSegment[];
  write(payload: Uint8Array): MaybePromise<C2MemFfiRequestBlock>;
  release(block: C2MemFfiRequestBlock): MaybePromise<void>;
  forgetConsumed(block: C2MemFfiRequestBlock): MaybePromise<void>;
  close?(): MaybePromise<void>;
}

export interface C2MemFfiNativeResponseBackend {
  readResponse(block: C2MemFfiResponseBlock, destination: Uint8Array): MaybePromise<void>;
  releaseResponse(block: C2MemFfiResponseBlock): MaybePromise<void>;
  close(): Promise<void>;
}

export interface C2MemFfiNativeRequestBackend {
  readonly prefix: string;
  readonly segments: readonly C2MemFfiShmSegment[];
  writeRequest(payload: Uint8Array): MaybePromise<C2MemFfiRequestBlock>;
  releaseRequest(block: C2MemFfiRequestBlock): MaybePromise<void>;
  markRequestConsumed(block: C2MemFfiRequestBlock): MaybePromise<void>;
  close(): Promise<void>;
}

export interface C2MemFfiAbiSymbols {
  c2_mem_ffi_abi_version(): MaybePromise<number>;
}

export interface C2MemFfiRequestPoolSymbols<Handle = unknown> extends C2MemFfiAbiSymbols {
  c2_mem_ffi_request_pool_new(
    prefix: string,
    segmentSize: number,
    maxSegments: number,
    minBlockSize: number,
  ): MaybePromise<C2MemFfiCallResult<Handle>>;
  c2_mem_ffi_request_pool_destroy(pool: Handle): MaybePromise<void>;
  c2_mem_ffi_request_pool_prefix(pool: Handle): MaybePromise<C2MemFfiCallResult<string>>;
  c2_mem_ffi_request_pool_segment_count(pool: Handle): MaybePromise<C2MemFfiCallResult<number>>;
  c2_mem_ffi_request_pool_segment_name(pool: Handle, segmentIndex: number): MaybePromise<C2MemFfiCallResult<string>>;
  c2_mem_ffi_request_pool_segment_data_size(pool: Handle, segmentIndex: number): MaybePromise<C2MemFfiCallResult<number>>;
  c2_mem_ffi_request_pool_write(pool: Handle, payload: Uint8Array): MaybePromise<C2MemFfiCallResult<C2MemFfiRequestBlock>>;
  c2_mem_ffi_request_pool_release(pool: Handle, block: C2MemFfiRequestBlock): MaybePromise<C2MemFfiCallResult>;
  c2_mem_ffi_request_pool_forget_consumed(pool: Handle, block: C2MemFfiRequestBlock): MaybePromise<C2MemFfiCallResult>;
}

export interface C2MemFfiResponsePoolSymbols<Handle = unknown> extends C2MemFfiAbiSymbols {
  c2_mem_ffi_response_pool_new(
    prefix: string,
    segmentSize: number,
    maxSegments: number,
    minBlockSize: number,
  ): MaybePromise<C2MemFfiCallResult<Handle>>;
  c2_mem_ffi_response_pool_destroy(pool: Handle): MaybePromise<void>;
  c2_mem_ffi_response_pool_read(pool: Handle, block: C2MemFfiResponseBlock, destination: Uint8Array): MaybePromise<C2MemFfiCallResult>;
  c2_mem_ffi_response_pool_release(pool: Handle, block: C2MemFfiResponseBlock): MaybePromise<C2MemFfiCallResult>;
}

/** Native addon owner; the Rust context pointer is never exposed to JavaScript. */
export interface C2MemFfiNativeUnixSocket {
  takeFd(): number;
  close(): void;
}

export interface C2MemFfiNativeLocalEndpointContext {
  endpointName(address: string): C2MemFfiCallResult<string>;
  namespaceId(): C2MemFfiCallResult<string>;
  connectUnix?(address: string): Promise<C2MemFfiCallResult<C2MemFfiNativeUnixSocket>>;
  close(): void;
}

export interface C2MemFfiLocalEndpointSymbols {
  c2_mem_ffi_abi_version(): number;
  /**
   * Resolve the automatic platform endpoint through the Rust configuration owner.
   */
  c2_mem_ffi_local_endpoint(address: string): C2MemFfiCallResult<string>;
  /** Optional capability: older ABI 3 libraries may omit this entire group. */
  c2_mem_ffi_local_endpoint_context_capture?(unixRoot: string | null): C2MemFfiCallResult<C2MemFfiNativeLocalEndpointContext>;
}

export interface C2MemFfiNodeNativeExtraSymbols<Handle = unknown> extends C2MemFfiLocalEndpointSymbols {
  c2_mem_ffi_request_pool_read_local(pool: Handle, block: C2MemFfiRequestBlock, destination: Uint8Array): MaybePromise<C2MemFfiCallResult<number>>;
}

export type C2MemFfiNodeNativeRawSymbols<Handle = unknown> =
  C2MemFfiRequestPoolSymbols<Handle>
  & C2MemFfiResponsePoolSymbols<Handle>
  & C2MemFfiNodeNativeExtraSymbols<Handle>;

export interface C2MemFfiNodeNativeSymbols<Handle = unknown> {
  readonly requestSymbols: C2MemFfiRequestPoolSymbols<Handle>;
  readonly responseSymbols: C2MemFfiResponsePoolSymbols<Handle>;
  readonly symbols: C2MemFfiNodeNativeRawSymbols<Handle>;
}

export interface C2MemFfiNodeNativeLoadOptions {
  readonly addonPath?: string;
}

export interface C2MemFfiLocalEndpointCaptureOptions {
  /** Unix IPC root override; native configuration rejects this on Windows. */
  readonly ipcRoot?: string;
}

const endpointContextBrand: unique symbol = Symbol("C-Two local endpoint context");

/**
 * A reusable native configuration snapshot. Each connector retains its own
 * reference on first connect; closing this handle does not invalidate those
 * references. Close unused handles explicitly. Native ownership ends when the
 * last reference closes, with garbage collection as a fallback.
 */
export interface C2MemFfiLocalEndpointContext {
  readonly [endpointContextBrand]: true;
  endpointName(address: string): string;
  namespaceId(): string;
  close(): void;
}

export interface C2MemFfiLocalEndpointOptions extends C2MemFfiNodeNativeLoadOptions, C2MemFfiLocalEndpointCaptureOptions {
  readonly endpointContext?: C2MemFfiLocalEndpointContext;
}

export interface C2MemFfiResponsePoolFactory {
  createResponsePool(config: C2MemFfiPoolConfig): Promise<C2MemFfiNativeResponsePool>;
}

export interface C2MemFfiNodeRuntime {
  readonly connect: C2NodeIpcConnector;
  readonly resolveEndpoint: (address: string) => string;
  readonly responsePoolFactory: C2MemFfiResponsePoolFactory;
  /** Release this runtime's endpoint snapshot; existing sockets remain usable. */
  close(): void;
}

export interface C2NodeIpcConnection {
  write(data: Uint8Array): MaybePromise<void>;
  readExactly(byteLength: number): MaybePromise<Uint8Array>;
  close?(): MaybePromise<void>;
}

export type C2NodeIpcConnect = (address: string) => MaybePromise<C2NodeIpcConnection>;

/** First connect attempt freezes native configuration, including failed attempts. */
export type C2NodeIpcConnector = C2NodeIpcConnect & {
  /** Before first connect this is a pure, unfrozen native query. */
  readonly resolveEndpoint: (address: string) => string;
  /** Release the factory snapshot, without closing sockets it has created. */
  readonly close: () => void;
};

export interface C2NodeIpcSocket {
  readonly destroyed: boolean;
  write(buffer: Uint8Array, callback?: (error?: Error | null) => void): boolean;
  end(): C2NodeIpcSocket;
  destroy(error?: Error): C2NodeIpcSocket;
  on(event: "data", listener: (chunk: Uint8Array) => void): C2NodeIpcSocket;
  on(event: "error", listener: (error: Error) => void): C2NodeIpcSocket;
  on(event: "end" | "close", listener: () => void): C2NodeIpcSocket;
  once(event: "connect", listener: () => void): C2NodeIpcSocket;
  once(event: "error", listener: (error: Error) => void): C2NodeIpcSocket;
  once(event: "close", listener: () => void): C2NodeIpcSocket;
  off(event: "connect", listener: () => void): C2NodeIpcSocket;
  off(event: "error", listener: (error: Error) => void): C2NodeIpcSocket;
  off(event: "close", listener: () => void): C2NodeIpcSocket;
}

export interface C2NodeIpcConnectOptions extends C2MemFfiLocalEndpointOptions {
  /** Optional already-loaded native endpoint symbols; never implements path derivation in TypeScript. */
  readonly nativeSymbols?: C2MemFfiLocalEndpointSymbols;
  readonly resolveEndpoint?: (address: string) => string;
  readonly createConnection?: (socketPath: string) => C2NodeIpcSocket;
}

interface C2MemFfiNodeNativeAddon<Handle = unknown> {
  load(libraryPath: string): C2MemFfiNodeNativeRawSymbols<Handle>;
}

const nodeNativeSymbolsCache = new Map<string, C2MemFfiNodeNativeRawSymbols<unknown>>();
const C2_MEM_FFI_NODE_ADDON_PATH = "./native/c2_mem_ffi_node.node";

export class C2MemFfiBindingError extends Error {
  constructor(message: string) {
    super(message);
    this.name = "C2MemFfiBindingError";
    Object.setPrototypeOf(this, C2MemFfiBindingError.prototype);
  }
}

export class C2NodeIpcConnectionError extends Error {
  constructor(message: string) {
    super(message);
    this.name = "C2NodeIpcConnectionError";
    Object.setPrototypeOf(this, C2NodeIpcConnectionError.prototype);
  }
}

export class C2MemFfiEndpointContextUnsupportedError extends C2MemFfiBindingError {
  readonly code = "C2_ENDPOINT_CONTEXT_UNSUPPORTED";

  constructor() {
    super("C-Two native library does not support endpoint contexts or ipcRoot. ABI version 3 does not guarantee this capability.");
    this.name = "C2MemFfiEndpointContextUnsupportedError";
    Object.setPrototypeOf(this, C2MemFfiEndpointContextUnsupportedError.prototype);
  }
}

interface LocalEndpointContextState {
  readonly native: C2MemFfiNativeLocalEndpointContext;
  references: number;
}

interface LocalEndpointContextOwner {
  readonly state: LocalEndpointContextState;
  closed: boolean;
}

interface LocalEndpointContextLease {
  readonly owner: LocalEndpointContextOwner;
}

function releaseEndpointContextOwner(owner: LocalEndpointContextOwner): void {
  if (owner.closed) return;
  owner.closed = true;
  if (--owner.state.references === 0) owner.state.native.close();
}

// The target shell is kept alive by every accessor using this reference. The
// held owner never references that shell, so detached accessors are safe too.
const endpointContextFinalizer = new FinalizationRegistry<LocalEndpointContextOwner>(releaseEndpointContextOwner);
const localEndpointContexts = new WeakMap<C2MemFfiLocalEndpointContext, LocalEndpointContextLease>();

function leaseEndpointContext(state: LocalEndpointContextState): LocalEndpointContextLease {
  const owner: LocalEndpointContextOwner = { state, closed: false };
  const lease = { owner };
  state.references += 1;
  endpointContextFinalizer.register(lease, owner, owner);
  return lease;
}

function closeEndpointContextLease(lease: LocalEndpointContextLease): void {
  endpointContextFinalizer.unregister(lease.owner);
  releaseEndpointContextOwner(lease.owner);
}

function nativeEndpointContext(lease: LocalEndpointContextLease): C2MemFfiNativeLocalEndpointContext {
  if (lease.owner.closed) throw new C2NodeIpcConnectionError("C-Two endpoint context is closed.");
  return lease.owner.state.native;
}

/** Capture code/environment/.env/default configuration once without creating directories. */
export function captureLocalIpcEndpointContext(
  options: C2MemFfiNodeNativeLoadOptions & C2MemFfiLocalEndpointCaptureOptions = {},
): C2MemFfiLocalEndpointContext {
  requireEndpointOptions(options);
  const { symbols } = loadBundledC2MemFfiNodeNativeSymbols(options);
  return captureLocalIpcEndpointContextFromSymbols(symbols, options);
}

/** Capture through a loaded library; ABI 3 and endpoint-context capability are checked separately. */
export function captureLocalIpcEndpointContextFromSymbols(
  symbols: C2MemFfiLocalEndpointSymbols,
  options: C2MemFfiLocalEndpointCaptureOptions = {},
): C2MemFfiLocalEndpointContext {
  requireEndpointOptions(options);
  requireEndpointSymbols(symbols);
  if (symbols.c2_mem_ffi_local_endpoint_context_capture === undefined) {
    throw new C2MemFfiEndpointContextUnsupportedError();
  }
  const result = symbols.c2_mem_ffi_local_endpoint_context_capture(options.ipcRoot ?? null);
  const native = result?.value;
  if (result?.status !== C2_MEM_FFI_STATUS_OK || typeof native !== "object" || native === null
    || typeof native.endpointName !== "function" || typeof native.namespaceId !== "function" || typeof native.close !== "function") {
    if (typeof native === "object" && native !== null && typeof native.close === "function") {
      native.close();
    }
    throw new C2NodeIpcConnectionError(`C-Two native endpoint context capture failed (status ${result?.status}).`);
  }
  const lease = leaseEndpointContext({ native, references: 0 });
  const context: C2MemFfiLocalEndpointContext = Object.freeze({
    [endpointContextBrand]: true as const,
    endpointName(address: string): string {
      const native = nativeEndpointContext(lease);
      requireLogicalIpcAddress(address);
      return requireNativeEndpointString(native.endpointName(address), "endpoint resolution");
    },
    namespaceId(): string {
      const native = nativeEndpointContext(lease);
      return requireNativeEndpointString(native.namespaceId(), "endpoint namespace query");
    },
    close(): void {
      closeEndpointContextLease(lease);
    },
  });
  localEndpointContexts.set(context, lease);
  return context;
}

function requireEndpointSymbols(symbols: C2MemFfiLocalEndpointSymbols): void {
  if (typeof symbols !== "object" || symbols === null || typeof symbols.c2_mem_ffi_abi_version !== "function") {
    throw new C2MemFfiBindingError("C-Two native endpoint symbols must provide c2_mem_ffi_abi_version().");
  }
  const abiVersion = symbols.c2_mem_ffi_abi_version();
  if (abiVersion !== C2_MEM_FFI_ABI_VERSION) {
    throw new C2MemFfiBindingError(`C-Two native endpoint symbols report ABI version ${abiVersion}; expected version ${C2_MEM_FFI_ABI_VERSION}.`);
  }
  if (typeof symbols.c2_mem_ffi_local_endpoint !== "function") {
    throw new C2MemFfiBindingError("C-Two native endpoint symbols must provide c2_mem_ffi_local_endpoint().");
  }
  if (symbols.c2_mem_ffi_local_endpoint_context_capture !== undefined && typeof symbols.c2_mem_ffi_local_endpoint_context_capture !== "function") {
    throw new C2MemFfiBindingError("C-Two native endpoint context capture symbol must be a function when provided.");
  }
}

function requireEndpointOptions(options: C2MemFfiLocalEndpointCaptureOptions & { readonly endpointContext?: C2MemFfiLocalEndpointContext }): void {
  if (typeof options !== "object" || options === null) {
    throw new C2NodeIpcConnectionError("C-Two endpoint options must be an object.");
  }
  if (options.ipcRoot !== undefined && (typeof options.ipcRoot !== "string" || options.ipcRoot.includes("\0"))) {
    throw new C2NodeIpcConnectionError("C-Two ipcRoot must be a string without NUL characters.");
  }
  if (options.ipcRoot !== undefined && options.endpointContext !== undefined) {
    throw new C2NodeIpcConnectionError("C-Two ipcRoot and endpointContext options are mutually exclusive.");
  }
  if (options.endpointContext !== undefined && !localEndpointContexts.has(options.endpointContext)) {
    throw new C2NodeIpcConnectionError("C-Two endpointContext must be created by captureLocalIpcEndpointContext().");
  }
}

function requireLogicalIpcAddress(address: string): void {
  if (typeof address !== "string" || address.includes("\0")) {
    throw new C2NodeIpcConnectionError("C-Two IPC address must be a string without NUL characters.");
  }
}

function requireNativeEndpointString(result: C2MemFfiCallResult<string>, operation: string): string {
  if (result?.status !== C2_MEM_FFI_STATUS_OK || typeof result.value !== "string" || result.value.length === 0 || result.value.includes("\0")) {
    throw new C2NodeIpcConnectionError(`C-Two native ${operation} failed (status ${result?.status}).`);
  }
  return result.value;
}

function retainEndpointContext(context: C2MemFfiLocalEndpointContext): LocalEndpointContextLease {
  const lease = localEndpointContexts.get(context);
  if (lease === undefined) {
    throw new C2NodeIpcConnectionError("C-Two endpointContext must be created by captureLocalIpcEndpointContext().");
  }
  nativeEndpointContext(lease);
  return leaseEndpointContext(lease.owner.state);
}

export function resolveBundledC2MemFfiNodeNativeLibraryPath(): string {
  const libraryName = bundledC2MemFfiNodeNativeLibraryName();
  const require = createRequire(import.meta.url);
  return require.resolve(`./native/${libraryName}`);
}

export function loadBundledC2MemFfiNodeNativeSymbols<Handle = unknown>(
  options: C2MemFfiNodeNativeLoadOptions = {},
): C2MemFfiNodeNativeSymbols<Handle> {
  return loadC2MemFfiNodeNativeSymbols(resolveBundledC2MemFfiNodeNativeLibraryPath(), options);
}

export function createBundledC2MemFfiNodeRuntime(
  options: C2MemFfiLocalEndpointOptions = {},
): C2MemFfiNodeRuntime {
  const connect = createNodeIpcConnect(options);
  const loadOptions: C2MemFfiNodeNativeLoadOptions = options.addonPath === undefined ? {} : { addonPath: options.addonPath };
  options = loadOptions;
  return Object.freeze({
    connect,
    resolveEndpoint: connect.resolveEndpoint,
    close: connect.close,
    responsePoolFactory: Object.freeze({
      async createResponsePool(
        config: C2MemFfiPoolConfig,
      ): Promise<C2MemFfiNativeResponsePool> {
        const { responseSymbols } = loadBundledC2MemFfiNodeNativeSymbols(loadOptions);
        return await createC2MemFfiResponsePoolFromSymbols(
          responseSymbols,
          config,
        );
      },
    }),
  });
}

/**
 * Resolve a logical `ipc://` address to its OS endpoint.
 *
 * The native resolver owns the platform backend and logical address validation.
 */
export function resolveLocalIpcEndpoint(
  address: string,
  options: C2MemFfiLocalEndpointOptions = {},
): string {
  requireLogicalIpcAddress(address);
  requireEndpointOptions(options);
  if (options.endpointContext !== undefined) {
    return options.endpointContext.endpointName(address);
  }
  if (options.ipcRoot !== undefined) {
    const context = captureLocalIpcEndpointContext(options);
    try {
      return context.endpointName(address);
    } finally {
      context.close();
    }
  }
  const { symbols } = loadBundledC2MemFfiNodeNativeSymbols(options);
  requireEndpointSymbols(symbols);
  return requireNativeEndpointString(symbols.c2_mem_ffi_local_endpoint(address), "endpoint resolution");
}

export function createNodeIpcConnect(options: C2NodeIpcConnectOptions = {}): C2NodeIpcConnector {
  if (typeof options !== "object" || options === null) {
    throw new C2NodeIpcConnectionError("C-Two Node IPC connect options must be an object.");
  }
  requireEndpointOptions(options);
  if (options.resolveEndpoint !== undefined && (options.ipcRoot !== undefined || options.endpointContext !== undefined || options.nativeSymbols !== undefined)) {
    throw new C2NodeIpcConnectionError("C-Two custom resolveEndpoint cannot be combined with ipcRoot, endpointContext, or nativeSymbols.");
  }
  if (options.endpointContext !== undefined && options.nativeSymbols !== undefined) {
    throw new C2NodeIpcConnectionError("C-Two endpointContext and nativeSymbols options are mutually exclusive.");
  }
  const openSocket = options.createConnection ?? createConnection;
  if (options.resolveEndpoint !== undefined && typeof options.resolveEndpoint !== "function") {
    throw new C2NodeIpcConnectionError("C-Two Node IPC resolveEndpoint option must be a function.");
  }
  if (typeof openSocket !== "function") {
    throw new C2NodeIpcConnectionError("C-Two Node IPC createConnection option must be a function.");
  }
  options = { ...options };
  let snapshot: LocalEndpointContextLease | undefined;
  let legacySymbols: C2MemFfiLocalEndpointSymbols | undefined;
  let attempted = false;
  let captureFailed = false;
  let captureError: unknown;
  let closed = false;
  const requireOpen = (): void => {
    if (closed) throw new C2NodeIpcConnectionError("C-Two Node IPC connector is closed.");
  };
  const freeze = (): void => {
    requireOpen();
    if (!attempted) {
      attempted = true;
      try {
        if (options.endpointContext !== undefined) {
          snapshot = retainEndpointContext(options.endpointContext);
          // Retain the snapshot reference, not the caller's disposable handle.
          const { endpointContext, ...remainingOptions } = options;
          options = remainingOptions;
        } else if (options.resolveEndpoint === undefined) {
          const symbols = options.nativeSymbols ?? loadBundledC2MemFfiNodeNativeSymbols(options).symbols;
          requireEndpointSymbols(symbols);
          if (symbols.c2_mem_ffi_local_endpoint_context_capture !== undefined || options.ipcRoot !== undefined) {
            const context = captureLocalIpcEndpointContextFromSymbols(symbols, options);
            try {
              snapshot = retainEndpointContext(context);
            } finally {
              context.close();
            }
          } else {
            // Older ABI3 libraries still support their existing default query.
            // They cannot provide a complete captured configuration snapshot.
            legacySymbols = symbols;
          }
        }
      } catch (error) {
        captureFailed = true;
        captureError = error;
      }
    }
    if (captureFailed) throw captureError;
  };
  const resolveEndpoint = (address: string): string => {
    requireOpen();
    if (captureFailed) throw captureError;
    if (options.resolveEndpoint !== undefined) return requireNodeIpcSocketPath(options.resolveEndpoint(address));
    requireLogicalIpcAddress(address);
    if (snapshot !== undefined) {
      return requireNativeEndpointString(nativeEndpointContext(snapshot).endpointName(address), "endpoint resolution");
    }
    if (options.endpointContext !== undefined) return options.endpointContext.endpointName(address);
    const symbols = legacySymbols ?? options.nativeSymbols ?? loadBundledC2MemFfiNodeNativeSymbols(options).symbols;
    requireEndpointSymbols(symbols);
    if (options.ipcRoot !== undefined) {
      const context = captureLocalIpcEndpointContextFromSymbols(symbols, options);
      try {
        return context.endpointName(address);
      } finally {
        context.close();
      }
    }
    return requireNativeEndpointString(symbols.c2_mem_ffi_local_endpoint(address), "endpoint resolution");
  };
  const connect = async (address: string): Promise<C2NodeIpcConnection> => {
    let socket: C2NodeIpcSocket | undefined;
    try {
      freeze();
      const normalizedPath = requireNodeIpcSocketPath(resolveEndpoint(address));
      if (process.platform !== "win32" && options.createConnection === undefined && options.resolveEndpoint === undefined) {
        if (snapshot === undefined || typeof nativeEndpointContext(snapshot).connectUnix !== "function") {
          throw new C2NodeIpcConnectionError("Native library lacks directory-relative Unix connections.");
        }
        const active = leaseEndpointContext(snapshot.owner.state);
        let owner: C2MemFfiNativeUnixSocket | undefined;
        try {
          const result = await nativeEndpointContext(active).connectUnix!(address);
          owner = result?.value;
          if (result?.status !== C2_MEM_FFI_STATUS_OK || owner === undefined
            || typeof owner.takeFd !== "function" || typeof owner.close !== "function") {
            throw new C2NodeIpcConnectionError(`C-Two Node IPC connect failed (${c2MemFfiStatusName(result?.status)}).`);
          }
          requireOpen();
          const fd = owner.takeFd();
          try {
            socket = new Socket({ fd, readable: true, writable: true });
          } catch (error) {
            closeSync(fd);
            throw error;
          }
          return new NodeIpcConnection(socket);
        } finally {
          owner?.close();
          closeEndpointContextLease(active);
        }
      }
      const pendingSocket = openSocket(normalizedPath);
      socket = pendingSocket;
      return await new Promise<C2NodeIpcConnection>((resolve, reject) => {
        let settled = false;
        const cleanup = (): void => {
          pendingSocket.off("connect", onConnect);
          pendingSocket.off("error", onError);
          pendingSocket.off("close", onClose);
        };
        const onConnect = (): void => {
          if (settled) return;
          try {
            const connection = new NodeIpcConnection(pendingSocket);
            settled = true;
            cleanup();
            resolve(connection);
          } catch (error) {
            settled = true;
            cleanup();
            reject(error);
          }
        };
        const onError = (error: Error): void => {
          if (settled) return;
          settled = true;
          cleanup();
          reject(nodeIpcError("C-Two Node IPC connect failed", error));
        };
        const onClose = (): void => {
          if (settled) return;
          settled = true;
          cleanup();
          reject(new C2NodeIpcConnectionError(`C-Two Node IPC socket closed before connect completed for ${normalizedPath}.`));
        };
        pendingSocket.once("connect", onConnect);
        pendingSocket.once("error", onError);
        pendingSocket.once("close", onClose);
      });
    } catch (error) {
      socket?.destroy();
      if (error instanceof C2MemFfiBindingError) throw error;
      throw nodeIpcError("C-Two Node IPC connect failed", error);
    }
  };
  return Object.freeze(Object.assign(connect, {
    resolveEndpoint,
    close(): void {
      if (closed) return;
      closed = true;
      if (snapshot !== undefined) closeEndpointContextLease(snapshot);
      snapshot = undefined;
      legacySymbols = undefined;
      captureError = undefined;
      options = {};
    },
  }));
}

export function loadC2MemFfiNodeNativeSymbols<Handle = unknown>(
  libraryPath: string,
  options: C2MemFfiNodeNativeLoadOptions = {},
): C2MemFfiNodeNativeSymbols<Handle> {
  if (typeof libraryPath !== "string" || libraryPath.length === 0) {
    throw new C2MemFfiBindingError("c2-mem-ffi native libraryPath must be a non-empty string.");
  }
  const addonPath = options.addonPath ?? C2_MEM_FFI_NODE_ADDON_PATH;
  if (typeof addonPath !== "string" || addonPath.length === 0) {
    throw new C2MemFfiBindingError("c2-mem-ffi native addonPath must be a non-empty string.");
  }
  const cacheKey = `${addonPath}\0${libraryPath}`;
  const cached = nodeNativeSymbolsCache.get(cacheKey);
  if (cached !== undefined) {
    const symbols = cached as C2MemFfiNodeNativeRawSymbols<Handle>;
    return {
      requestSymbols: symbols,
      responseSymbols: symbols,
      symbols,
    };
  }
  const require = createRequire(import.meta.url);
  const addon = require(addonPath) as C2MemFfiNodeNativeAddon<Handle>;
  if (typeof addon !== "object" || addon === null || typeof addon.load !== "function") {
    throw new C2MemFfiBindingError("c2-mem-ffi Node native addon must export load(libraryPath).");
  }
  const symbols = addon.load(libraryPath);
  nodeNativeSymbolsCache.set(cacheKey, symbols as C2MemFfiNodeNativeRawSymbols<unknown>);
  return {
    requestSymbols: symbols,
    responseSymbols: symbols,
    symbols,
  };
}

function bundledC2MemFfiNodeNativeLibraryName(): string {
  if (process.platform === "darwin") {
    return "libc2_mem_ffi.dylib";
  }
  if (process.platform === "linux") {
    return "libc2_mem_ffi.so";
  }
  if (process.platform === "win32") {
    return "c2_mem_ffi.dll";
  }
  throw new C2MemFfiBindingError(`c2-mem-ffi bundled Node native library is not available for ${process.platform}.`);
}

export async function createC2MemFfiRequestPoolFromSymbols<Handle>(
  symbols: C2MemFfiRequestPoolSymbols<Handle>,
  config: C2MemFfiPoolConfig,
): Promise<C2MemFfiNativeRequestPool> {
  const normalizedConfig = normalizePoolConfig(config, "c2-mem-ffi request pool config");
  await requireAbiVersion(symbols, "c2-mem-ffi request pool symbols");
  const handle = await requireOkValue(
    await symbols.c2_mem_ffi_request_pool_new(
      normalizedConfig.prefix,
      normalizedConfig.segmentSize,
      normalizedConfig.maxSegments,
      normalizedConfig.minBlockSize,
    ),
    "c2_mem_ffi_request_pool_new",
  );
  let closed = false;
  try {
    const prefix = await requireOkValue(
      await symbols.c2_mem_ffi_request_pool_prefix(handle),
      "c2_mem_ffi_request_pool_prefix",
    );
    const segmentCount = await requireOkValue(
      await symbols.c2_mem_ffi_request_pool_segment_count(handle),
      "c2_mem_ffi_request_pool_segment_count",
    );
    if (segmentCount !== normalizedConfig.maxSegments) {
      throw new C2MemFfiBindingError(
        `c2-mem-ffi request pool reported ${segmentCount} segments, expected ${normalizedConfig.maxSegments}.`,
      );
    }
    const segments: C2MemFfiShmSegment[] = [];
    for (let index = 0; index < segmentCount; index += 1) {
      const name = await requireOkValue(
        await symbols.c2_mem_ffi_request_pool_segment_name(handle, index),
        `c2_mem_ffi_request_pool_segment_name(${index})`,
      );
      const size = await requireOkValue(
        await symbols.c2_mem_ffi_request_pool_segment_data_size(handle, index),
        `c2_mem_ffi_request_pool_segment_data_size(${index})`,
      );
      segments.push({ name, size });
    }
    const pool = normalizeRequestPool({
      prefix,
      segments,
      async write(payload: Uint8Array): Promise<C2MemFfiRequestBlock> {
        ensurePoolOpen(closed, "request");
        const block = await requireOkValue(
          await symbols.c2_mem_ffi_request_pool_write(handle, requireUint8Array(payload, "payload")),
          "c2_mem_ffi_request_pool_write",
        );
        try {
          return requireOwnedRequestBlock(block, payload.byteLength, segments, "write");
        } catch (error) {
          await releaseInvalidRequestSymbolBlock(symbols, handle, block, error);
          throw error;
        }
      },
      async release(block: C2MemFfiRequestBlock): Promise<void> {
        ensurePoolOpen(closed, "request");
        await requireOk(
          await symbols.c2_mem_ffi_request_pool_release(
            handle,
            requireOwnedRequestBlock(block, undefined, segments, "release"),
          ),
          "c2_mem_ffi_request_pool_release",
        );
      },
      async forgetConsumed(block: C2MemFfiRequestBlock): Promise<void> {
        ensurePoolOpen(closed, "request");
        await requireOk(
          await symbols.c2_mem_ffi_request_pool_forget_consumed(
            handle,
            requireOwnedRequestBlock(block, undefined, segments, "forget consumed"),
          ),
          "c2_mem_ffi_request_pool_forget_consumed",
        );
      },
      async close(): Promise<void> {
        if (closed) {
          return;
        }
        closed = true;
        await symbols.c2_mem_ffi_request_pool_destroy(handle);
      },
    }, "c2-mem-ffi request pool symbols");
    return pool;
  } catch (error) {
    if (!closed) {
      closed = true;
      await symbols.c2_mem_ffi_request_pool_destroy(handle);
    }
    throw error;
  }
}

/**
 * Open a peer response pool over an owner's advertised SHM prefix.
 *
 * The pool maps nothing until a block references a backing. `config.segmentSize`
 * is only the minimum capacity accepted for buddy backings opened lazily by
 * prefix/index/generation; actual geometry and validation stay in c2-mem.
 */
export async function createC2MemFfiResponsePoolFromSymbols<Handle>(
  symbols: C2MemFfiResponsePoolSymbols<Handle>,
  config: C2MemFfiPoolConfig,
): Promise<C2MemFfiNativeResponsePool> {
  const normalizedConfig = normalizePoolConfig(config, "c2-mem-ffi response pool config");
  await requireAbiVersion(symbols, "c2-mem-ffi response pool symbols");
  const handle = await requireOkValue(
    await symbols.c2_mem_ffi_response_pool_new(
      normalizedConfig.prefix,
      normalizedConfig.segmentSize,
      normalizedConfig.maxSegments,
      normalizedConfig.minBlockSize,
    ),
    "c2_mem_ffi_response_pool_new",
  );
  let closed = false;
  return {
    async read(block: C2MemFfiResponseBlock, destination: Uint8Array): Promise<void> {
      ensurePoolOpen(closed, "response");
      const responseBlock = requireResponseBlock(block, "read");
      requireUint8Array(destination, "destination");
      if (destination.byteLength !== responseBlock.byteLength) {
        throw new C2MemFfiBindingError(`c2-mem-ffi response destination length ${destination.byteLength} does not match block byteLength ${responseBlock.byteLength}.`);
      }
      await requireOk(
        await symbols.c2_mem_ffi_response_pool_read(handle, responseBlock, destination),
        "c2_mem_ffi_response_pool_read",
      );
    },
    async release(block: C2MemFfiResponseBlock): Promise<void> {
      ensurePoolOpen(closed, "response");
      await requireOk(
        await symbols.c2_mem_ffi_response_pool_release(handle, requireResponseBlock(block, "release")),
        "c2_mem_ffi_response_pool_release",
      );
    },
    async close(): Promise<void> {
      if (closed) {
        return;
      }
      closed = true;
      await symbols.c2_mem_ffi_response_pool_destroy(handle);
    },
  };
}

export function createC2MemFfiNativeResponseBackend(
  pool: C2MemFfiNativeResponsePool,
): C2MemFfiNativeResponseBackend {
  const normalized = normalizeResponsePool(pool, "c2-mem-ffi native response pool");
  let closed = false;
  return {
    async readResponse(block: C2MemFfiResponseBlock, destination: Uint8Array): Promise<void> {
      ensureOpen(closed, "response");
      const normalizedBlock = requireResponseBlock(block, "read");
      requireUint8Array(destination, "destination");
      if (destination.byteLength !== normalizedBlock.byteLength) {
        throw new C2MemFfiBindingError(`c2-mem-ffi response destination length ${destination.byteLength} does not match block byteLength ${normalizedBlock.byteLength}.`);
      }
      await normalized.read(normalizedBlock, destination);
    },
    async releaseResponse(block: C2MemFfiResponseBlock): Promise<void> {
      ensureOpen(closed, "response");
      await normalized.release(requireResponseBlock(block, "release"));
    },
    async close(): Promise<void> {
      if (closed) {
        return;
      }
      closed = true;
      await normalized.close?.();
    },
  };
}

export function createC2MemFfiNativeRequestBackend(
  pool: C2MemFfiNativeRequestPool,
): C2MemFfiNativeRequestBackend {
  const normalized = normalizeRequestPool(pool, "c2-mem-ffi native request pool");
  let closed = false;
  return {
    prefix: normalized.prefix,
    segments: normalized.segments,
    async writeRequest(payload: Uint8Array): Promise<C2MemFfiRequestBlock> {
      ensureOpen(closed, "request");
      const block = await normalized.write(requireUint8Array(payload, "payload"));
      try {
        return requireOwnedRequestBlock(block, payload.byteLength, normalized.segments, "write");
      } catch (error) {
        await releaseInvalidRequestBlock(normalized, block, error);
        throw error;
      }
    },
    releaseRequest(block: C2MemFfiRequestBlock): void | Promise<void> {
      ensureOpen(closed, "request");
      return normalized.release(requireOwnedRequestBlock(block, undefined, normalized.segments, "release"));
    },
    markRequestConsumed(block: C2MemFfiRequestBlock): void | Promise<void> {
      ensureOpen(closed, "request");
      return normalized.forgetConsumed(requireOwnedRequestBlock(block, undefined, normalized.segments, "mark consumed"));
    },
    async close(): Promise<void> {
      if (closed) {
        return;
      }
      closed = true;
      await normalized.close?.();
    },
  };
}

class NodeIpcConnection implements C2NodeIpcConnection {
  private readonly chunks: Uint8Array[] = [];
  private bufferedBytes = 0;
  private readonly waiters: Array<() => void> = [];
  private terminalError: C2NodeIpcConnectionError | undefined;
  private closed = false;

  constructor(private readonly socket: C2NodeIpcSocket) {
    socket.on("data", (chunk: Uint8Array): void => {
      if (chunk.byteLength === 0) {
        return;
      }
      this.chunks.push(chunk);
      this.bufferedBytes += chunk.byteLength;
      this.wake();
    });
    socket.on("error", (error: Error): void => {
      this.fail(nodeIpcError("C-Two Node IPC socket error", error));
    });
    socket.on("end", (): void => {
      this.fail(new C2NodeIpcConnectionError("C-Two Node IPC socket ended."));
    });
    socket.on("close", (): void => {
      this.fail(new C2NodeIpcConnectionError("C-Two Node IPC socket closed."));
    });
  }

  async write(data: Uint8Array): Promise<void> {
    const bytes = requireNodeIpcBytes(data, "C-Two Node IPC write data");
    if (this.terminalError !== undefined) {
      throw this.terminalError;
    }
    if (this.closed || this.socket.destroyed) {
      throw new C2NodeIpcConnectionError("C-Two Node IPC connection is closed.");
    }
    if (bytes.byteLength === 0) {
      return;
    }
    await new Promise<void>((resolve, reject) => {
      let settled = false;
      const cleanup = (): void => {
        this.socket.off("error", onError);
        this.socket.off("close", onClose);
      };
      const finish = (error?: unknown): void => {
        if (settled) {
          return;
        }
        settled = true;
        cleanup();
        if (error === undefined) {
          resolve();
        } else {
          reject(nodeIpcError("C-Two Node IPC write failed", error));
        }
      };
      const onError = (error: Error): void => finish(error);
      const onClose = (): void => finish(this.terminalError ?? new C2NodeIpcConnectionError("C-Two Node IPC socket closed before write completed."));
      this.socket.once("error", onError);
      this.socket.once("close", onClose);
      try {
        this.socket.write(bytes, (error) => finish(error ?? undefined));
      } catch (error) {
        finish(error);
      }
    });
  }

  async readExactly(byteLength: number): Promise<Uint8Array> {
    const requested = requireNodeIpcByteLength(byteLength, "C-Two Node IPC read byteLength");
    if (this.closed) {
      throw new C2NodeIpcConnectionError("C-Two Node IPC connection is closed.");
    }
    while (this.bufferedBytes < requested) {
      if (this.terminalError !== undefined) {
        throw this.terminalError;
      }
      await new Promise<void>((resolve) => {
        this.waiters.push(resolve);
      });
    }
    return this.take(requested);
  }

  close(): void {
    if (this.closed) {
      return;
    }
    this.closed = true;
    this.fail(new C2NodeIpcConnectionError("C-Two Node IPC connection closed."));
    this.socket.end();
    this.socket.destroy();
  }

  private take(byteLength: number): Uint8Array {
    if (byteLength === 0) {
      return new Uint8Array();
    }
    const output = new Uint8Array(byteLength);
    let outputOffset = 0;
    while (outputOffset < byteLength) {
      const chunk = this.chunks[0];
      if (chunk === undefined) {
        throw new C2NodeIpcConnectionError("C-Two Node IPC internal buffer underflow.");
      }
      const takeLength = Math.min(chunk.byteLength, byteLength - outputOffset);
      output.set(chunk.subarray(0, takeLength), outputOffset);
      outputOffset += takeLength;
      if (takeLength === chunk.byteLength) {
        this.chunks.shift();
      } else {
        this.chunks[0] = chunk.subarray(takeLength);
      }
    }
    this.bufferedBytes -= byteLength;
    return output;
  }

  private fail(error: C2NodeIpcConnectionError): void {
    this.terminalError ??= error;
    this.wake();
  }

  private wake(): void {
    const waiters = this.waiters.splice(0);
    for (const wake of waiters) {
      wake();
    }
  }
}

function normalizeResponsePool(pool: C2MemFfiNativeResponsePool, label: string): C2MemFfiNativeResponsePool {
  if (typeof pool !== "object" || pool === null) {
    throw new C2MemFfiBindingError(`${label} must be an object.`);
  }
  if (typeof pool.read !== "function") {
    throw new C2MemFfiBindingError(`${label}.read must be a function.`);
  }
  if (typeof pool.release !== "function") {
    throw new C2MemFfiBindingError(`${label}.release must be a function.`);
  }
  if (pool.close !== undefined && typeof pool.close !== "function") {
    throw new C2MemFfiBindingError(`${label}.close must be a function when provided.`);
  }
  return pool;
}

function normalizeRequestPool(pool: C2MemFfiNativeRequestPool, label: string): C2MemFfiNativeRequestPool {
  if (typeof pool !== "object" || pool === null) {
    throw new C2MemFfiBindingError(`${label} must be an object.`);
  }
  const prefix = requirePrefix(pool.prefix, `${label}.prefix`);
  if (!Array.isArray(pool.segments)) {
    throw new C2MemFfiBindingError(`${label}.segments must be an array.`);
  }
  if (pool.segments.length === 0 || pool.segments.length > C2_MEM_FFI_MAX_IPC_SHM_SEGMENTS) {
    throw new C2MemFfiBindingError(`${label}.segments length must be between 1 and ${C2_MEM_FFI_MAX_IPC_SHM_SEGMENTS}.`);
  }
  const segments = Object.freeze(pool.segments.map((segment, index) => normalizeRequestSegment(segment, index, label)));
  if (typeof pool.write !== "function") {
    throw new C2MemFfiBindingError(`${label}.write must be a function.`);
  }
  if (typeof pool.release !== "function") {
    throw new C2MemFfiBindingError(`${label}.release must be a function.`);
  }
  if (typeof pool.forgetConsumed !== "function") {
    throw new C2MemFfiBindingError(`${label}.forgetConsumed must be a function.`);
  }
  if (pool.close !== undefined && typeof pool.close !== "function") {
    throw new C2MemFfiBindingError(`${label}.close must be a function when provided.`);
  }
  return {
    prefix,
    segments,
    write(payload: Uint8Array): C2MemFfiRequestBlock | Promise<C2MemFfiRequestBlock> {
      return pool.write(payload);
    },
    release(block: C2MemFfiRequestBlock): void | Promise<void> {
      return pool.release(block);
    },
    forgetConsumed(block: C2MemFfiRequestBlock): void | Promise<void> {
      return pool.forgetConsumed(block);
    },
    close(): void | Promise<void> {
      return pool.close?.();
    },
  };
}

function normalizeSegment(segment: C2MemFfiShmSegment, label: string): C2MemFfiShmSegment {
  if (typeof segment !== "object" || segment === null) {
    throw new C2MemFfiBindingError(`${label} must be an object.`);
  }
  requireShmIdentity(segment.name, `${label}.name`);
  if (!Number.isSafeInteger(segment.size) || segment.size <= 0 || segment.size > 0xffffffff) {
    throw new C2MemFfiBindingError(`${label}.size must be a positive u32 integer.`);
  }
  return Object.freeze({ name: segment.name, size: segment.size });
}

function normalizeRequestSegment(
  segment: C2MemFfiShmSegment,
  index: number,
  label: string,
): C2MemFfiShmSegment {
  const normalized = normalizeSegment(segment, `${label}.segments[${index}]`);
  return normalized;
}

function requirePrefix(value: string, label: string): string {
  requireShmIdentity(value, label);
  if (utf8ByteLength(value) > C2_MEM_FFI_MAX_SHM_PREFIX_BYTES) {
    throw new C2MemFfiBindingError(`${label} cannot exceed ${C2_MEM_FFI_MAX_SHM_PREFIX_BYTES} bytes.`);
  }
  return value;
}

function requireShmIdentity(value: string, label: string): void {
  if (typeof value !== "string" || value.length <= 1 || !value.startsWith("/")) {
    throw new C2MemFfiBindingError(`${label} must be a non-empty shared-memory identity name beginning with "/".`);
  }
  if (value.slice(1).includes("/")) {
    throw new C2MemFfiBindingError(`${label} must not contain "/" after the leading slash.`);
  }
  if (/[\u0000-\u001F\u007F-\u009F]/.test(value)) {
    throw new C2MemFfiBindingError(`${label} must not contain control characters.`);
  }
}

function requireUint8Array(value: Uint8Array, label: string): Uint8Array {
  if (!(value instanceof Uint8Array)) {
    throw new C2MemFfiBindingError(`${label} must be a Uint8Array.`);
  }
  return value;
}

function requireNodeIpcSocketPath(value: unknown): string {
  if (typeof value !== "string" || value.length === 0) {
    throw new C2NodeIpcConnectionError("C-Two Node IPC socket path must be a non-empty string.");
  }
  if (/[\u0000]/.test(value)) {
    throw new C2NodeIpcConnectionError("C-Two Node IPC socket path must not contain NUL bytes.");
  }
  return value;
}

function requireNodeIpcBytes(value: Uint8Array, label: string): Uint8Array {
  if (!(value instanceof Uint8Array)) {
    throw new C2NodeIpcConnectionError(`${label} must be a Uint8Array.`);
  }
  return value;
}

function requireNodeIpcByteLength(value: number, label: string): number {
  if (!Number.isSafeInteger(value) || value < 0) {
    throw new C2NodeIpcConnectionError(`${label} must be a non-negative safe integer.`);
  }
  return value;
}

function nodeIpcError(prefix: string, error: unknown): C2NodeIpcConnectionError {
  if (error instanceof C2NodeIpcConnectionError) {
    return error;
  }
  if (error instanceof Error) {
    return new C2NodeIpcConnectionError(`${prefix}: ${error.message}`);
  }
  return new C2NodeIpcConnectionError(`${prefix}: ${String(error)}`);
}

function requireResponseBlock(block: C2MemFfiResponseBlock, action: string): C2MemFfiResponseBlock {
  const normalized = requireBlockShape(block, `response block ${action}`);
  validateResponseBlockMetadata(block, normalized);
  return block;
}

function validateResponseBlockMetadata(
  block: C2MemFfiResponseBlock,
  normalized: { readonly segmentIndex: number; readonly offset: number; readonly byteLength: number },
): void {
  if (block.prefix !== undefined) {
    requirePrefix(block.prefix, "c2-mem-ffi response block prefix");
  }
  if (block.segments === undefined) {
    return;
  }
  if (!Array.isArray(block.segments)) {
    throw new C2MemFfiBindingError("c2-mem-ffi response block segments must be an array when provided.");
  }
  if (block.segments.length > C2_MEM_FFI_MAX_IPC_SHM_SEGMENTS) {
    throw new C2MemFfiBindingError(`c2-mem-ffi response block segments length must be no greater than ${C2_MEM_FFI_MAX_IPC_SHM_SEGMENTS}.`);
  }
  for (const [index, segment] of block.segments.entries()) {
    normalizeSegment(segment, `c2-mem-ffi response block segments[${index}]`);
  }
  if (block.dedicated) return;
  const segment = block.segments[normalized.segmentIndex];
  if (segment !== undefined && normalized.offset + normalized.byteLength > segment.size) {
    throw new C2MemFfiBindingError(`c2-mem-ffi response block range exceeds advertised segment ${normalized.segmentIndex} size ${segment.size}.`);
  }
}

function requireOwnedRequestBlock(
  block: C2MemFfiRequestBlock,
  expectedByteLength: number | undefined,
  segments: readonly C2MemFfiShmSegment[],
  action: string,
): C2MemFfiRequestBlock {
  const normalized = requireBlockShape(block, `request block ${action}`);
  if (expectedByteLength !== undefined && normalized.byteLength !== expectedByteLength) {
    throw new C2MemFfiBindingError(`c2-mem-ffi request block byteLength ${normalized.byteLength} does not match payload length ${expectedByteLength}.`);
  }
  if (normalized.dedicated) return block;
  const segment = segments[normalized.segmentIndex];
  if (segment === undefined) {
    throw new C2MemFfiBindingError(`c2-mem-ffi request block segmentIndex ${normalized.segmentIndex} is not advertised.`);
  }
  const end = normalized.offset + normalized.byteLength;
  if (end > segment.size) {
    throw new C2MemFfiBindingError(`c2-mem-ffi request block range ${normalized.offset}+${normalized.byteLength} exceeds advertised segment ${normalized.segmentIndex} size ${segment.size}.`);
  }
  return block;
}

function requireBlockShape(
  block: C2MemFfiRequestBlock,
  label: string,
): { readonly segmentIndex: number; readonly generation: number; readonly offset: number; readonly byteLength: number; readonly dedicated: boolean } {
  if (typeof block !== "object" || block === null) {
    throw new C2MemFfiBindingError(`c2-mem-ffi ${label} must be an object.`);
  }
  if (!Number.isSafeInteger(block.segmentIndex) || block.segmentIndex < 0 || block.segmentIndex > 0xffff) {
    throw new C2MemFfiBindingError(`c2-mem-ffi ${label} segmentIndex must be a u16 integer.`);
  }
  if (!Number.isSafeInteger(block.offset) || block.offset < 0 || block.offset > 0xffffffff) {
    throw new C2MemFfiBindingError(`c2-mem-ffi ${label} offset must be a u32 integer.`);
  }
  if (!Number.isSafeInteger(block.generation) || block.generation < 0 || block.generation > 0xffffffff || (!block.dedicated && block.generation === 0) || (block.dedicated && block.generation !== 0)) {
    throw new C2MemFfiBindingError(`c2-mem-ffi ${label} generation must be a positive u32 for buddy memory or 0 for dedicated memory.`);
  }
  if (!Number.isSafeInteger(block.byteLength) || block.byteLength <= 0 || block.byteLength > 0xffffffff) {
    throw new C2MemFfiBindingError(`c2-mem-ffi ${label} byteLength must be a positive u32 integer.`);
  }
  if (typeof block.dedicated !== "boolean") {
    throw new C2MemFfiBindingError(`c2-mem-ffi ${label} dedicated must be a boolean.`);
  }
  if (block.dedicated && block.offset !== 0) {
    throw new C2MemFfiBindingError(`c2-mem-ffi ${label} dedicated offset must be 0.`);
  }
  return {
    segmentIndex: block.segmentIndex,
    generation: block.generation,
    offset: block.offset,
    byteLength: block.byteLength,
    dedicated: block.dedicated,
  };
}

function ensureOpen(closed: boolean, label: string): void {
  if (closed) {
    throw new C2MemFfiBindingError(`c2-mem-ffi native ${label} backend is closed.`);
  }
}

function ensurePoolOpen(closed: boolean, label: string): void {
  if (closed) {
    throw new C2MemFfiBindingError(`c2-mem-ffi ${label} pool is closed.`);
  }
}

function normalizePoolConfig(config: C2MemFfiPoolConfig, label: string): C2MemFfiPoolConfig {
  if (typeof config !== "object" || config === null) {
    throw new C2MemFfiBindingError(`${label} must be an object.`);
  }
  const prefix = requirePrefix(config.prefix, `${label}.prefix`);
  if (!Number.isSafeInteger(config.segmentSize) || config.segmentSize <= 0 || config.segmentSize > 0xffffffff) {
    throw new C2MemFfiBindingError(`${label}.segmentSize must be a positive u32 integer.`);
  }
  if (!Number.isSafeInteger(config.maxSegments) || config.maxSegments < 1 || config.maxSegments > C2_MEM_FFI_MAX_IPC_SHM_SEGMENTS) {
    throw new C2MemFfiBindingError(`${label}.maxSegments must be between 1 and ${C2_MEM_FFI_MAX_IPC_SHM_SEGMENTS}.`);
  }
  if (!Number.isSafeInteger(config.minBlockSize) || config.minBlockSize <= 0 || config.minBlockSize > 0xffffffff) {
    throw new C2MemFfiBindingError(`${label}.minBlockSize must be a positive u32 integer.`);
  }
  return {
    prefix,
    segmentSize: config.segmentSize,
    maxSegments: config.maxSegments,
    minBlockSize: config.minBlockSize,
  };
}

async function requireAbiVersion(symbols: C2MemFfiAbiSymbols, label: string): Promise<void> {
  if (typeof symbols !== "object" || symbols === null) {
    throw new C2MemFfiBindingError(`${label} must be an object.`);
  }
  if (typeof symbols.c2_mem_ffi_abi_version !== "function") {
    throw new C2MemFfiBindingError(`${label}.c2_mem_ffi_abi_version must be a function.`);
  }
  const version = await symbols.c2_mem_ffi_abi_version();
  if (!Number.isSafeInteger(version) || version !== C2_MEM_FFI_ABI_VERSION) {
    throw new C2MemFfiBindingError(`c2-mem-ffi ABI version ${String(version)} is incompatible with expected version ${C2_MEM_FFI_ABI_VERSION}.`);
  }
}

function requireOk(result: C2MemFfiCallResult<unknown>, label: string): void {
  if (typeof result !== "object" || result === null) {
    throw new C2MemFfiBindingError(`${label} returned an invalid c2-mem-ffi status result.`);
  }
  if (result.status !== C2_MEM_FFI_STATUS_OK) {
    throw new C2MemFfiBindingError(`${label} failed with ${c2MemFfiStatusName(result.status)}.`);
  }
}

function requireOkValue<T>(result: C2MemFfiCallResult<T>, label: string): T {
  requireOk(result, label);
  if (result.value === undefined) {
    throw new C2MemFfiBindingError(`${label} returned OK without a value.`);
  }
  return result.value;
}

function c2MemFfiStatusName(status: unknown): string {
  switch (status) {
    case C2_MEM_FFI_STATUS_OK:
      return "OK";
    case C2_MEM_FFI_STATUS_NULL_POINTER:
      return "NULL_POINTER";
    case C2_MEM_FFI_STATUS_INVALID_ARGUMENT:
      return "INVALID_ARGUMENT";
    case C2_MEM_FFI_STATUS_POOL_ERROR:
      return "POOL_ERROR";
    case C2_MEM_FFI_STATUS_INSUFFICIENT_BUFFER:
      return "INSUFFICIENT_BUFFER";
    case C2_MEM_FFI_STATUS_IO_ERROR:
      return "IO_ERROR";
    default:
      return `UNKNOWN(${String(status)})`;
  }
}

async function releaseInvalidRequestBlock(
  pool: C2MemFfiNativeRequestPool,
  block: C2MemFfiRequestBlock,
  priorError: unknown,
): Promise<void> {
  try {
    if (typeof block === "object" && block !== null) {
      await pool.release(block);
    }
  } catch (releaseError) {
    throw new C2MemFfiBindingError(`c2-mem-ffi request block validation failed and release failed: validation=${String(priorError)} release=${String(releaseError)}`);
  }
}

async function releaseInvalidRequestSymbolBlock<Handle>(
  symbols: C2MemFfiRequestPoolSymbols<Handle>,
  handle: Handle,
  block: C2MemFfiRequestBlock,
  priorError: unknown,
): Promise<void> {
  try {
    if (typeof block === "object" && block !== null) {
      await requireOk(
        await symbols.c2_mem_ffi_request_pool_release(handle, block),
        "c2_mem_ffi_request_pool_release",
      );
    }
  } catch (releaseError) {
    throw new C2MemFfiBindingError(`c2-mem-ffi request block validation failed and C ABI release failed: validation=${String(priorError)} release=${String(releaseError)}`);
  }
}

function utf8ByteLength(value: string): number {
  let bytes = 0;
  for (let index = 0; index < value.length; index += 1) {
    const code = value.charCodeAt(index);
    if (code < 0x80) {
      bytes += 1;
    } else if (code < 0x800) {
      bytes += 2;
    } else if (code >= 0xd800 && code <= 0xdbff && index + 1 < value.length) {
      const next = value.charCodeAt(index + 1);
      if (next >= 0xdc00 && next <= 0xdfff) {
        bytes += 4;
        index += 1;
      } else {
        bytes += 3;
      }
    } else {
      bytes += 3;
    }
  }
  return bytes;
}
