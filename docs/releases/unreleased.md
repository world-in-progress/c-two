# Unreleased

## Python manual assembly API retirement

Remove `c_two.mem.ChunkAssembler`, `c_two.mem.MemHandle` and their `_native` exports. These were the manual Python entry points used by the retired Python transport loops. They have no compatibility aliases or replacement Python manual-assembly API. Code that directly constructed an assembler and accessed its finished handle must move to normal RPC automatic chunking or the retained Rust assembler API.

The Rust `ChunkRegistry`, `ChunkAssembler`, `ReassemblyBacking` and `MemHandle` remain unchanged. Current IPC chunking, memory-pool APIs, snapshots, budget accounting and release order are preserved; C/FFI ABI 5, IPC 12, SHM 3 and owner control 1 are unchanged.

## Inline response ownership

Core now moves an IPC inline response's existing byte vector into the held response owner. Ordinary Python IPC calls and Rust held calls avoid one payload allocation and copy; Rust owned calls retain their existing move path. SHM and chunked responses keep their original transport leases, pool identity, budget accounting and release order. Python snapshots and FastDB receive adapters retain their existing behavior. Public APIs, ABI and protocols are unchanged.

## Python endpoint credentials

`cc.EndpointCredential` now exposes the existing frozen native type directly. `cc.inspect_endpoint` passes the observed Rust credential to Python without encoding JSON, parsing it again or allocating a Python wrapper. `cc.reap_endpoint(address, credential)` accepts that same value. Metadata properties, `from_json(document=...)`, `to_json()` and the public repr are preserved.

Remove uses of the old `EndpointCredential(native)` wrapper constructor, subclassing, `credential._native`, `copy.copy(credential)` and `_native.PyEndpointCredential`. Share the immutable value directly within a process. The low-level `_native.inspect_endpoint_endpoint` present result now contains a credential object instead of a JSON string. There are no compatibility aliases. Python method descriptors and non-string argument diagnostics now come from the native type.

For persistence or cross-process transfer, call `to_json()` explicitly and restore with `cc.EndpointCredential.from_json(document=...)`. These boundaries retain strict schema/platform/field validation, canonical JSON and the 32 KiB limit. Inspection reports the observed native object without also promising that JSON export will succeed; a subsequent explicit export can report its codec error.

Rust public APIs, C/FFI ABI 5, IPC 12, SHM 3, owner control 1, business frames and credential JSON formats are unchanged. Exact identity checks, reaping authority and lifecycle behavior are unchanged. This entry describes the current source and is not a published release announcement.
