# @c-two/c2-mem-ffi

TypeScript facade and Node/POSIX loader for adapting `c2-mem-ffi` pool bindings into C-Two generated native-buddy request and response backends.

This package includes a no-extra-npm-dependency Node-API addon source that uses POSIX `dlopen` / `dlsym` to load a caller-supplied `c2_mem_ffi` shared library and translate native pool pointers, C string out-parameters, block structs, status codes, and `Uint8Array` buffers into the JavaScript-callable symbol table consumed by `createC2MemFfiRequestPoolFromSymbols(...)` and `createC2MemFfiResponsePoolFromSymbols(...)`. It also exposes `createNodeIpcConnect(...)` for adapting Node Unix-domain sockets to generated `createIpcEncodedTransport(...)`. The package is restricted to Linux and macOS because the current runtime path depends on POSIX SHM and Unix-domain sockets. Bun, Deno, prebuilt distribution, broader real-IPC coverage, and publishable runtime packaging are still separate follow-up work.

This package also owns the structural composition into generated `createNodePosixNativeBuddyRequestShmWriter(...)` and `createNodePosixNativeBuddyResponseShmReader(...)` compatible backend shapes, including adapters over lower-level pool objects and adapters over JavaScript-callable C ABI symbol tables. Response-pool release is valid for both read and unread non-dedicated response blocks so generated transports can free real response SHM after allocator failures that happen before copying.

Use `loadBundledC2MemFfiNodeNativeSymbols()` when the packaged `dist/native/libc2_mem_ffi.*` artifact should be used, or `resolveBundledC2MemFfiNodeNativeLibraryPath()` when a caller needs the concrete path for diagnostics. Use `loadC2MemFfiNodeNativeSymbols(libraryPath)` only when a caller intentionally supplies an external Rust `c2_mem_ffi` shared library path.

```bash
npm run build
npm run build:rust
npm run build:node-addon
npm run test:node-addon
npm run pack:check
npm test
```

`npm run pack:check` builds the local Node/POSIX runtime artifacts, verifies that `npm pack --dry-run` contains only the runtime package surface, and installs the packed tarball into a clean temporary ESM consumer project. The consumer typechecks against the package `exports.types` surface, loads the bundled native library through the package API, exercises request and response pool write/read/release paths, and verifies closed native handles cannot be reused or destroyed twice. The tarball surface is limited to `dist/index.js`, `dist/index.d.ts`, `dist/native/c2_mem_ffi_node.node`, the platform `libc2_mem_ffi` shared library, `README.md`, and `package.json`. The package remains marked `private` until the cross-platform publishing story is decided; this check is a local packaging guardrail, not a registry publish claim.

The clean-consumer typecheck uses `tsc` by default. Set `TSC_JS=/path/to/typescript/bin/tsc` to run a specific TypeScript CLI through the current Node executable, or `TSC=/path/to/tsc` for a direct executable override. `npm run build`, `npm run typecheck`, `npm run test:node-addon`, and `npm run pack:check` run through npm's `$NODE` and then use `process.execPath` internally, so the package guardrails do not depend on recursive `npm`, ambient `node`, or ambient `tsc` lookup when npm exposes `npm_execpath` and a usable TypeScript CLI can be found from the package, npm global prefix, or `TSC_JS`. The local Rust build entrypoints resolve Cargo from `CARGO`, `CARGO_HOME`, `$HOME/.cargo/bin`, or common package-manager paths before falling back to ambient `cargo`, so `npm run build:rust` and the Node addon build share the same restricted-PATH behavior.

The facade is deliberately FastDB-neutral. FastDB remains a codec and provider-owned allocator layer that can compose with generated C-Two transports after a C-Two runtime binding supplies the IPC connection and SHM reader/writer hooks.
