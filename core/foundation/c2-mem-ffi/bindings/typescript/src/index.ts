import { createRequire } from "node:module";
import { createConnection, Socket } from "node:net";
import { closeSync } from "node:fs";

interface C2NodeTimer { unref(): void; }
const { setTimeout, clearTimeout, setImmediate } = createRequire(import.meta.url)("node:timers") as {
  setTimeout(callback: () => void, delayMillis: number): C2NodeTimer;
  clearTimeout(timer: C2NodeTimer): void;
  setImmediate(callback: () => void): void;
};

export const C2_MEM_FFI_STATUS_OK = 0;
export const C2_MEM_FFI_STATUS_NULL_POINTER = 1;
export const C2_MEM_FFI_STATUS_INVALID_ARGUMENT = 2;
export const C2_MEM_FFI_STATUS_POOL_ERROR = 3;
export const C2_MEM_FFI_STATUS_INSUFFICIENT_BUFFER = 4;
export const C2_MEM_FFI_STATUS_IO_ERROR = 5;
export const C2_MEM_FFI_STATUS_PERMISSION_DENIED = 6;
export const C2_MEM_FFI_STATUS_IDENTITY_MISMATCH = 7;

export const C2_MEM_FFI_MAX_SHM_PREFIX_BYTES = 255;
export const C2_MEM_FFI_ABI_VERSION = 7;

export type C2MemFfiStatus =
  | typeof C2_MEM_FFI_STATUS_OK
  | typeof C2_MEM_FFI_STATUS_NULL_POINTER
  | typeof C2_MEM_FFI_STATUS_INVALID_ARGUMENT
  | typeof C2_MEM_FFI_STATUS_POOL_ERROR
  | typeof C2_MEM_FFI_STATUS_INSUFFICIENT_BUFFER
  | typeof C2_MEM_FFI_STATUS_IO_ERROR
  | typeof C2_MEM_FFI_STATUS_PERMISSION_DENIED
  | typeof C2_MEM_FFI_STATUS_IDENTITY_MISMATCH;

type MaybePromise<T> = T | Promise<T>;

export interface C2MemFfiCallResult<T = void> {
  readonly status: C2MemFfiStatus;
  readonly value?: T;
}

export interface C2MemFfiBlock {
  readonly segmentIndex: number;
  readonly generation: number;
  readonly offset: number;
  readonly byteLength: number;
  readonly loanId: bigint;
  readonly dedicated: boolean;
}
export interface C2MemFfiPreparedRequest { readonly block: C2MemFfiBlock; }
export interface C2MemFfiWrite {
  readonly prefix: Uint8Array;
  start(): void;
  complete(success: boolean): void;
  discard(): void;
}
export interface C2MemFfiMemoryContext {
  readonly prefix: string;
  finishHandshake(prefix: string | null): void;
  prepareRequest(payload: Uint8Array): C2MemFfiPreparedRequest;
  discardRequest(request: C2MemFfiPreparedRequest): void;
  admitResponse(block: C2MemFfiBlock): unknown;
  consumeResponse(response: unknown, destination: Uint8Array): void;
  discardResponse(response: unknown): void;
  beginWrite(kind: 0 | 1 | 2, request?: C2MemFfiPreparedRequest): C2MemFfiWrite | undefined;
  receiveRelease(frame: Uint8Array): void;
  gc(): number;
  deactivate(): void;
  peerEnd(): void;
  tryClose(): boolean;
  close(): void;
}
export interface C2MemFfiNativeMemoryContext {
  prefix(): C2MemFfiCallResult<string>;
  finishHandshake(prefix: string | null): C2MemFfiCallResult;
  prepareRequest(payload: Uint8Array): C2MemFfiCallResult<C2MemFfiPreparedRequest>;
  discardRequest(request: C2MemFfiPreparedRequest): C2MemFfiCallResult;
  admitResponse(block: C2MemFfiBlock): C2MemFfiCallResult<unknown>;
  consumeResponse(response: unknown, destination: Uint8Array): C2MemFfiCallResult<number>;
  discardResponse(response: unknown): C2MemFfiCallResult;
  beginWrite(kind: number, request?: C2MemFfiPreparedRequest): C2MemFfiCallResult<{ readonly prefix: Uint8Array }>;
  startWrite(write: unknown): C2MemFfiCallResult;
  completeWrite(write: unknown, success: boolean): C2MemFfiCallResult;
  discardWrite(write: unknown): C2MemFfiCallResult;
  receiveRelease(frame: Uint8Array): C2MemFfiCallResult;
  gc(): C2MemFfiCallResult<number>;
  deactivate(): C2MemFfiCallResult;
  peerEnd(): C2MemFfiCallResult;
  tryClose(): C2MemFfiCallResult<boolean>;
  close(): C2MemFfiCallResult;
}

/** Native addon owner; the Rust context pointer is never exposed to JavaScript. */
export interface C2MemFfiNativeUnixSocket {
  takeMemory(): C2MemFfiNativeMemoryContext;
  takeFd(): number;
  close(): void;
}

/**
 * Native addon owner of one verified Windows pipe. `takeFd()` transfers the
 * Node-CRT descriptor to `new Socket({ fd })`; the pinned server process
 * handle stays with this owner until the socket-backed connection closes or
 * the owner is closed or collected.
 */
export interface C2MemFfiNativeWindowsSocket {
  takeMemory(): C2MemFfiNativeMemoryContext;
  takeFd(): number;
  close(): void;
}

export interface C2MemFfiNativeLocalEndpointContext {
  endpointName(address: string): C2MemFfiCallResult<string>;
  namespaceId(): C2MemFfiCallResult<string>;
  connectUnix?(address: string): Promise<C2MemFfiCallResult<C2MemFfiNativeUnixSocket>>;
  connectWindows?(address: string): Promise<C2MemFfiCallResult<C2MemFfiNativeWindowsSocket>>;
  close(): void;
}

export interface C2MemFfiLocalEndpointSymbols {
  c2_mem_ffi_abi_version(): number;
  /**
   * Resolve the automatic platform endpoint through the Rust configuration owner.
   */
  c2_mem_ffi_local_endpoint(address: string): C2MemFfiCallResult<string>;
  c2_mem_ffi_local_endpoint_context_capture(unixRoot: string | null): C2MemFfiCallResult<C2MemFfiNativeLocalEndpointContext>;
  /**
   * Module-level verified Windows connect. The addon re-validates the native
   * context type tag of the receiver argument, so forged context objects with
   * arbitrary connect functions cannot supply an unverified socket.
   */
  c2_mem_ffi_endpoint_context_connect_windows?(
    context: C2MemFfiNativeLocalEndpointContext,
    address: string,
  ): Promise<C2MemFfiCallResult<C2MemFfiNativeWindowsSocket>>;
}

export interface C2MemFfiNodeNativeSymbols {
  readonly symbols: C2MemFfiLocalEndpointSymbols;
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

export interface C2MemFfiNodeRuntime {
  readonly connect: C2NodeIpcConnector;
  readonly resolveEndpoint: (address: string) => string;
  close(): void;
}

export interface C2NodeIpcConnection {
  readonly memory?: C2MemFfiMemoryContext | undefined;
  write(data: Uint8Array): MaybePromise<void>;
  readExactly(byteLength: number): MaybePromise<Uint8Array>;
  close(): MaybePromise<void>;
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
  pause(): C2NodeIpcSocket;
  resume(): C2NodeIpcSocket;
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

interface C2MemFfiNodeNativeAddon {
  load(libraryPath: string): C2MemFfiLocalEndpointSymbols;
}

const nodeNativeSymbolsCache = new Map<string, C2MemFfiLocalEndpointSymbols>();
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

/** Capture through a loaded ABI 7 library. */
export function captureLocalIpcEndpointContextFromSymbols(
  symbols: C2MemFfiLocalEndpointSymbols,
  options: C2MemFfiLocalEndpointCaptureOptions = {},
): C2MemFfiLocalEndpointContext {
  requireEndpointOptions(options);
  requireEndpointSymbols(symbols);
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
  if (typeof symbols.c2_mem_ffi_local_endpoint_context_capture !== "function") {
    throw new C2MemFfiBindingError("C-Two native endpoint symbols must provide c2_mem_ffi_local_endpoint_context_capture().");
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

export function loadBundledC2MemFfiNodeNativeSymbols(
  options: C2MemFfiNodeNativeLoadOptions = {},
): C2MemFfiNodeNativeSymbols {
  return loadC2MemFfiNodeNativeSymbols(resolveBundledC2MemFfiNodeNativeLibraryPath(), options);
}

export function createBundledC2MemFfiNodeRuntime(
  options: C2MemFfiLocalEndpointOptions = {},
): C2MemFfiNodeRuntime {
  const connect = createNodeIpcConnect(options);
  return Object.freeze({ connect, resolveEndpoint: connect.resolveEndpoint, close: connect.close });
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
  if (process.platform === "win32"
    && (options.createConnection !== undefined || options.resolveEndpoint !== undefined || options.nativeSymbols !== undefined)) {
    // The official Windows connector only opens sockets through the verified
    // native entry. This refusal happens at construction, before any factory
    // call or wire byte; Unix keeps its existing injection surface.
    throw new C2NodeIpcConnectionError("C-Two Windows Node IPC requires the trusted native connector; createConnection, resolveEndpoint, and nativeSymbols are not accepted on Windows.");
  }
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
  let verifiedWindowsConnect: C2MemFfiLocalEndpointSymbols["c2_mem_ffi_endpoint_context_connect_windows"] | undefined;
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
          const context = captureLocalIpcEndpointContextFromSymbols(symbols, options);
          try {
            snapshot = retainEndpointContext(context);
          } finally {
            context.close();
          }
        }
        if (process.platform === "win32") {
          // Every Windows socket goes through the addon's module-level verified
          // connect, which re-validates the native context type tag of the
          // snapshot receiver before any pipe handle is produced.
          const symbols = loadBundledC2MemFfiNodeNativeSymbols(options).symbols;
          requireEndpointSymbols(symbols);
          const connect = symbols.c2_mem_ffi_endpoint_context_connect_windows;
          if (typeof connect !== "function") {
            throw new C2NodeIpcConnectionError("C-Two native endpoint symbols lack the verified Windows connect entry.");
          }
          verifiedWindowsConnect = connect;
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
    const symbols = options.nativeSymbols ?? loadBundledC2MemFfiNodeNativeSymbols(options).symbols;
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
      if (process.platform === "win32") {
        requireLogicalIpcAddress(address);
        if (snapshot === undefined) {
          throw new C2NodeIpcConnectionError("C-Two Windows Node IPC connector lacks a frozen native endpoint snapshot.");
        }
        const verifiedConnect = verifiedWindowsConnect;
        if (verifiedConnect === undefined) {
          throw new C2NodeIpcConnectionError("C-Two Windows Node IPC connector lacks the verified native connect entry.");
        }
        const active = leaseEndpointContext(snapshot.owner.state);
        let owner: C2MemFfiNativeWindowsSocket | undefined;
        try {
          const native = nativeEndpointContext(active);
          const result = await verifiedConnect(native, address);
          owner = result?.value;
          if (result?.status !== C2_MEM_FFI_STATUS_OK || owner === undefined
            || typeof owner.takeFd !== "function" || typeof owner.close !== "function") {
            throw new C2NodeIpcConnectionError(`C-Two Node IPC connect failed (${c2MemFfiStatusName(result?.status)}).`);
          }
          requireOpen();
          const memory = memoryFromNative(owner.takeMemory());
          const fd = owner.takeFd();
          try {
            socket = new Socket({ fd, readable: true, writable: true });
          } catch (error) {
            closeSync(fd);
            memory.close();
            throw error;
          }
          const pinnedOwner = owner;
          socket.once("close", () => pinnedOwner.close());
          owner = undefined;
          return new NodeIpcConnection(socket, memory);
        } finally {
          owner?.close();
          closeEndpointContextLease(active);
        }
      }
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
          const memory = memoryFromNative(owner.takeMemory());
          const fd = owner.takeFd();
          try {
            socket = new Socket({ fd, readable: true, writable: true });
          } catch (error) {
            closeSync(fd);
            memory.close();
            throw error;
          }
          return new NodeIpcConnection(socket, memory);
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
      verifiedWindowsConnect = undefined;
      captureError = undefined;
      options = {};
    },
  }));
}

export function loadC2MemFfiNodeNativeSymbols(
  libraryPath: string,
  options: C2MemFfiNodeNativeLoadOptions = {},
): C2MemFfiNodeNativeSymbols {
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
    const symbols = cached as C2MemFfiLocalEndpointSymbols;
    return {
      symbols,
    };
  }
  const require = createRequire(import.meta.url);
  const addon = require(addonPath) as C2MemFfiNodeNativeAddon;
  if (typeof addon !== "object" || addon === null || typeof addon.load !== "function") {
    throw new C2MemFfiBindingError("c2-mem-ffi Node native addon must export load(libraryPath).");
  }
  const symbols = addon.load(libraryPath);
  nodeNativeSymbolsCache.set(cacheKey, symbols as C2MemFfiLocalEndpointSymbols);
  return {
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

function memoryFromNative(native: C2MemFfiNativeMemoryContext): C2MemFfiMemoryContext {
  let prefix: string;
  try { prefix = requireOkValue(native.prefix(), "memory prefix"); }
  catch (error) { native.close(); throw error; }
  return {
    prefix,
    finishHandshake(prefix) { requireOk(native.finishHandshake(prefix), "finish handshake"); },
    prepareRequest(payload) { return requireOkValue(native.prepareRequest(payload), "prepare request"); },
    discardRequest(request) { requireOk(native.discardRequest(request), "discard request"); },
    admitResponse(block) { return requireOkValue(native.admitResponse(block), "admit response"); },
    consumeResponse(response, dst) { requireOk(native.consumeResponse(response, dst), "consume response"); },
    discardResponse(response) { requireOk(native.discardResponse(response), "discard response"); },
    beginWrite(kind, request) {
      const result = request === undefined ? native.beginWrite(kind) : native.beginWrite(kind, request);
      requireOk(result, "begin write");
      const write = result.value;
      if (write === undefined) return undefined;
      return {
        prefix: write.prefix,
        start() { requireOk(native.startWrite(write), "start write"); },
        complete(success) { requireOk(native.completeWrite(write, success), "complete write"); },
        discard() { requireOk(native.discardWrite(write), "discard write"); },
      };
    },
    receiveRelease(frame) { requireOk(native.receiveRelease(frame), "receive release"); },
    gc() { return requireOkValue(native.gc(), "memory GC"); },
    deactivate() { requireOk(native.deactivate(), "deactivate"); },
    peerEnd() { requireOk(native.peerEnd(), "peer end"); },
    tryClose() { return requireOkValue(native.tryClose(), "try close"); },
    close() { requireOk(native.close(), "close memory context"); },
  };
}

class NodeIpcConnection implements C2NodeIpcConnection {
  private readonly chunks: Uint8Array[] = [];
  private bufferedBytes = 0;
  private readonly waiters: Array<() => void> = [];
  private terminalError: C2NodeIpcConnectionError | undefined;
  private closed = false;
  private peerReadEnded = false;
  private peerEndReported = false;
  private peerEndScheduled = false;
  private gcTimer: C2NodeTimer | undefined;
  constructor(private readonly socket: C2NodeIpcSocket, readonly memory?: C2MemFfiMemoryContext) {
    socket.pause();
    socket.on("data", (chunk: Uint8Array): void => {
      if (chunk.byteLength === 0) {
        return;
      }
      this.chunks.push(chunk);
      this.bufferedBytes += chunk.byteLength;
      socket.pause();
      this.wake();
    });
    socket.on("error", (error: Error): void => {
      const failure = error as Error & { code?: string; syscall?: string };
      if (!this.closed && process.platform !== "win32" && failure.code === "ECONNRESET" && failure.syscall === "read") this.markPeerEnd();
      this.fail(nodeIpcError("C-Two Node IPC socket error", error));
    });
    socket.on("end", (): void => {
      if (!this.closed) this.markPeerEnd();
      this.fail(new C2NodeIpcConnectionError("C-Two Node IPC socket ended."));
    });
    socket.on("close", (): void => {
      this.fail(new C2NodeIpcConnectionError("C-Two Node IPC socket closed."));
    });
    if (this.memory !== undefined) this.scheduleGc();
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
        // A short terminal read cannot contain the complete frame requested
        // by the caller. Its remaining bytes will not settle a release.
        this.reportPeerEnd();
        throw this.terminalError;
      }
      await new Promise<void>((resolve) => {
        this.waiters.push(resolve);
        this.socket.resume();
      });
    }
    const bytes = this.take(requested);
    this.schedulePeerEnd();
    return bytes;
  }

  close(): void {
    if (this.closed) {
      return;
    }
    this.closed = true;
    if (this.gcTimer !== undefined) clearTimeout(this.gcTimer);
    this.gcTimer = undefined;
    this.fail(new C2NodeIpcConnectionError("C-Two Node IPC connection closed."));
    // Native remote reads retain a duplicate OS reference. SHUT_WR on this
    // socket would expose EOF to the peer before the last such read ends.
    if (this.memory === undefined) this.socket.end();
    this.socket.destroy();
    if (this.peerReadEnded) this.reportPeerEnd();
    this.memory?.deactivate();
    this.memory?.close();
  }

  private markPeerEnd(): void {
    this.peerReadEnded = true;
    this.schedulePeerEnd();
  }

  private scheduleGc(): void {
    if (this.closed || this.memory === undefined) return;
    const delayMillis = this.memory.gc();
    this.gcTimer = setTimeout(() => {
      this.gcTimer = undefined;
      if (!this.closed) this.scheduleGc();
    }, delayMillis);
    this.gcTimer.unref();
  }

  private schedulePeerEnd(): void {
    if (!this.peerReadEnded || this.peerEndReported || this.peerEndScheduled || this.bufferedBytes !== 0) return;
    this.peerEndScheduled = true;
    // The consumer parses the final buffered frame and delivers its release
    // record in microtasks following readExactly(). Settle remaining loans only
    // after that parser has run.
    setImmediate(() => {
      this.peerEndScheduled = false;
      if (this.bufferedBytes === 0) this.reportPeerEnd();
    });
  }

  private reportPeerEnd(): void {
    if (!this.peerReadEnded || this.peerEndReported) return;
    this.peerEndReported = true;
    this.memory?.peerEnd();
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

function requireUint8Array(value: Uint8Array, label: string): Uint8Array {
  if (!(value instanceof Uint8Array)) throw new C2MemFfiBindingError(`${label} must be a Uint8Array.`);
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
    case C2_MEM_FFI_STATUS_PERMISSION_DENIED:
      return "PERMISSION_DENIED";
    case C2_MEM_FFI_STATUS_IDENTITY_MISMATCH:
      return "IDENTITY_MISMATCH";
    default:
      return `UNKNOWN(${String(status)})`;
  }
}
