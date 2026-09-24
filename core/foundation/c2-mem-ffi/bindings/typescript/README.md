# @c-two/c2-mem-ffi

TypeScript facade and Node-API loader for the Rust `c2-mem-ffi` memory pool. The native library owns backing names, pool incarnations, buddy generations, allocation and release. JavaScript passes the native block coordinates through unchanged; it does not implement a second allocator or construct OS object names.

The source build supports Linux and macOS shared libraries and Windows `c2_mem_ffi.dll`. The loader uses `dlopen` on Unix and Unicode `LoadLibraryExW` on Windows. Windows addon builds use the pinned development dependency `node-gyp` with Python and Visual Studio C++ build tools. A native run on each target is required to establish runtime support; the package contains the artifacts built for the current platform and architecture.

`createNodeIpcConnect()` accepts logical `ipc://server-id` addresses. Rust resolves each address to a Unix socket or a Windows Named Pipe in the current logon scope. `resolveLocalIpcEndpoint(address)` exposes the same native resolver for diagnostics. Neither helper treats a pipe endpoint as a filesystem path.

Use `createBundledC2MemFfiNodeRuntime()` for generated clients. It exposes `connect`, `resolveEndpoint` and a response pool factory accepted by generated `createC2MemFfiNativeResponseShmReader(...)`. Native loading is deferred until an IPC operation, so creating a transport configuration for HTTP does not open local shared memory. Initialize request and response IPC adapters only for local transport paths.

`loadBundledC2MemFfiNodeNativeSymbols()` loads the packaged native library. `resolveBundledC2MemFfiNodeNativeLibraryPath()` returns its path. `loadC2MemFfiNodeNativeSymbols(libraryPath)` accepts an explicit external library built with ABI version 2. ABI 2 blocks carry `segmentIndex`, `generation`, `offset`, `byteLength` and `dedicated`. Buddy generations are positive; dedicated coordinates use generation zero. The native pools support both buddy and dedicated mappings. Dedicated release acknowledges reader completion before the owner reclaims the mapping.

The request pool input `prefix` is a logical label. Rust appends a fresh process and pool incarnation; send the returned `pool.prefix` in the handshake. Response pools receive that complete peer prefix. Segment names in handshake metadata are informational; Rust opens each backing from the peer prefix, slot and generation. Response release accepts both read and unread buddy or dedicated blocks so allocation failures can still release the peer allocation.

```bash
npm ci
npm test
npm run pack:check
```

`npm test` builds the TypeScript, Rust library and Node addon before running native tests. `npm run pack:check` verifies the package file inventory and installs the exact local tarball into a separate ESM consumer. That consumer checks types, bundled library loading, pool write/read/release and closed-handle rejection. The archive contains `dist/index.js`, `dist/index.d.ts`, the `.node` addon, the target native library, this README and `package.json`. This is a local package check, not a registry publication.

The scripts launch child JavaScript through `process.execPath`. TypeScript can be overridden using `TSC_JS` or `TSC`; Cargo can be overridden using `CARGO` or resolved through `CARGO_HOME`, the user's `.cargo/bin`, and then `PATH`. Windows builds require the target MSVC tools to be installed; `node-gyp` selects the matching Node headers and import library.

This package exposes C-Two memory ownership and byte transport only. Portable FastDB adapters are composed separately from Core-generated artifacts and currently open owned payload copies. Holding a payload is a lifetime contract, not a claim of direct response-SHM construction or zero-copy decoding.
