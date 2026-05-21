use c2_codegen::{TypeScriptOptions, generate_typescript_client};
use serde_json::Value;

fn opaque_primitive_contract_json() -> String {
    r#"{
      "schema": "c-two.contract.v1",
      "crm": {
        "namespace": "test.codegen",
        "name": "Portable",
        "version": "0.1.0"
      },
      "fingerprints": {
        "abi_hash": "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef",
        "signature_hash": "abcdef0123456789abcdef0123456789abcdef0123456789abcdef0123456789"
      },
      "methods": [
        {
          "access": "read",
          "buffer": "view",
          "name": "echo",
          "parameters": [
            {
              "name": "value",
              "kind": "POSITIONAL_OR_KEYWORD",
              "default": {"kind": "missing"},
              "type": {"kind": "primitive", "name": "str"}
            },
            {
              "name": "suffix",
              "kind": "POSITIONAL_OR_KEYWORD",
              "default": {"kind": "json_scalar", "value": null},
              "type": {
                "kind": "union",
                "items": [
                  {"kind": "none"},
                  {"kind": "primitive", "name": "str"}
                ]
              }
            }
          ],
          "return": {"kind": "primitive", "name": "str"},
          "wire": {
            "input": {
              "kind": "codec_ref",
              "id": "org.example.text-args",
              "version": "1",
              "schema": "example.text-args.v1",
              "portable": true
            },
            "output": {
              "kind": "codec_ref",
              "id": "org.example.text-args",
              "version": "1",
              "schema": "example.text-args.v1",
              "portable": true
            }
          }
        }
      ]
    }"#
    .to_string()
}

fn external_codec_contract_json() -> String {
    r#"{
      "schema": "c-two.contract.v1",
      "crm": {
        "namespace": "test.codegen",
        "name": "Portable",
        "version": "0.1.0"
      },
      "fingerprints": {
        "abi_hash": "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef",
        "signature_hash": "abcdef0123456789abcdef0123456789abcdef0123456789abcdef0123456789"
      },
      "methods": [
        {
          "access": "write",
          "buffer": "view",
          "name": "load_payload",
          "parameters": [],
          "return": {
            "kind": "codec",
            "codec": {
              "kind": "codec_ref",
              "id": "org.example.payload",
              "version": "1",
              "schema_sha256": "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef",
              "portable": true
            }
          },
          "wire": {
            "input": null,
            "output": {
              "kind": "codec_ref",
              "id": "org.example.payload",
              "version": "1",
              "schema_sha256": "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef",
              "portable": true
            }
          }
        }
      ]
    }"#
    .to_string()
}

fn fastdb_call_db_contract_json() -> String {
    r#"{
      "schema": "c-two.contract.v1",
      "crm": {
        "namespace": "test.codegen",
        "name": "FastdbPortable",
        "version": "0.1.0"
      },
      "fingerprints": {
        "abi_hash": "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef",
        "signature_hash": "abcdef0123456789abcdef0123456789abcdef0123456789abcdef0123456789"
      },
      "methods": [
        {
          "access": "write",
          "buffer": "view",
          "name": "query",
          "parameters": [],
          "return": {
            "kind": "codec",
            "codec": {
              "kind": "codec_ref",
              "id": "org.fastdb.call-db",
              "version": "1",
              "schema": "fastdb.call-db.schema.v1",
              "schema_sha256": "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa",
              "capabilities": ["bytes", "buffer-view"],
              "portable": true
            }
          },
          "wire": {
            "input": null,
            "output": {
              "kind": "codec_ref",
              "id": "org.fastdb.call-db",
              "version": "1",
              "schema": "fastdb.call-db.schema.v1",
              "schema_sha256": "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa",
              "capabilities": ["bytes", "buffer-view"],
              "portable": true
            }
          }
        }
      ]
    }"#
    .to_string()
}

fn fastdb_raw_feature_contract_json() -> String {
    r#"{
      "schema": "c-two.contract.v1",
      "crm": {
        "namespace": "test.codegen",
        "name": "FastdbFeature",
        "version": "0.1.0"
      },
      "fingerprints": {
        "abi_hash": "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef",
        "signature_hash": "abcdef0123456789abcdef0123456789abcdef0123456789abcdef0123456789"
      },
      "methods": [
        {
          "access": "write",
          "buffer": "view",
          "name": "load",
          "parameters": [],
          "return": {
            "kind": "codec",
            "codec": {
              "kind": "codec_ref",
              "id": "org.fastdb.columnar",
              "version": "1",
              "schema": "fastdb.schema.v1",
              "schema_sha256": "bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb",
              "capabilities": ["bytes", "buffer-view"],
              "portable": true
            }
          },
          "wire": {
            "input": null,
            "output": {
              "kind": "codec_ref",
              "id": "org.fastdb.columnar",
              "version": "1",
              "schema": "fastdb.schema.v1",
              "schema_sha256": "bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb",
              "capabilities": ["bytes", "buffer-view"],
              "portable": true
            }
          }
        }
      ]
    }"#
    .to_string()
}

fn name_collision_contract_json() -> String {
    r#"{
      "schema": "c-two.contract.v1",
      "crm": {
        "namespace": "test.codegen",
        "name": "Portable",
        "version": "0.1.0"
      },
      "fingerprints": {
        "abi_hash": "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef",
        "signature_hash": "abcdef0123456789abcdef0123456789abcdef0123456789abcdef0123456789"
      },
      "methods": [
        {
          "access": "write",
          "buffer": "view",
          "name": "foo-bar",
          "parameters": [],
          "return": {"kind": "none"},
          "wire": {"input": null, "output": null}
        },
        {
          "access": "write",
          "buffer": "view",
          "name": "foo_bar",
          "parameters": [],
          "return": {"kind": "none"},
          "wire": {"input": null, "output": null}
        }
      ]
    }"#
    .to_string()
}

fn parameter_collision_contract_json() -> String {
    r#"{
      "schema": "c-two.contract.v1",
      "crm": {
        "namespace": "test.codegen",
        "name": "Portable",
        "version": "0.1.0"
      },
      "fingerprints": {
        "abi_hash": "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef",
        "signature_hash": "abcdef0123456789abcdef0123456789abcdef0123456789abcdef0123456789"
      },
      "methods": [
        {
          "access": "write",
          "buffer": "view",
          "name": "echo",
          "parameters": [
            {
              "name": "value-id",
              "kind": "POSITIONAL_OR_KEYWORD",
              "default": {"kind": "missing"},
              "type": {"kind": "primitive", "name": "str"}
            },
            {
              "name": "value_id",
              "kind": "POSITIONAL_OR_KEYWORD",
              "default": {"kind": "missing"},
              "type": {"kind": "primitive", "name": "str"}
            }
          ],
          "return": {"kind": "none"},
          "wire": {
            "input": {
              "kind": "codec_ref",
              "id": "org.example.text-args",
              "version": "1",
              "schema": "example.text-args.v1",
              "portable": true
            },
            "output": null
          }
        }
      ]
    }"#
    .to_string()
}

#[test]
fn generates_typescript_client_for_opaque_primitive_contract() {
    let output = generate_typescript_client(
        opaque_primitive_contract_json().as_bytes(),
        TypeScriptOptions::default(),
    )
    .unwrap();

    assert!(output.contains("export const PORTABLE_CONTRACT"));
    assert!(output.contains("export class PortableClient"));
    assert!(output.contains("async echo(value: string, suffix?: null | string): Promise<string>"));
    assert!(output.contains("compactTrailingUndefined([value, suffix])"));
    assert!(
        output.contains(
            "export const PORTABLE_CODEC_REQUIREMENTS: readonly C2CodecRequirement[] = ["
        )
    );
    assert!(output.contains("id: \"org.example.text-args\""));
    assert!(output.contains("schema: \"example.text-args.v1\""));
    assert!(output.contains("export function createCodecStubs(): C2CodecRegistry"));
}

#[test]
fn generates_opaque_types_and_requirements_for_external_codecs() {
    let output = generate_typescript_client(
        external_codec_contract_json().as_bytes(),
        TypeScriptOptions::default(),
    )
    .unwrap();

    assert!(output.contains("Promise<C2CodecValue<\"org.example.payload\">>"));
    assert!(
        output.contains(
            "export const PORTABLE_CODEC_REQUIREMENTS: readonly C2CodecRequirement[] = ["
        )
    );
    assert!(output.contains("id: \"org.example.payload\""));
    assert!(output.contains(
        "schemaSha256: \"0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef\""
    ));
    assert!(output.contains("export interface C2CodecImplementation"));
    assert!(
        output.contains(
            "export function codecRequirementKey(requirement: C2CodecRequirement): string"
        )
    );
    assert!(output.contains("export function createCodecStubs(): C2CodecRegistry"));
    assert!(output.contains("registry[codecRequirementKey(requirement)]"));
    assert!(output.contains("if (requirement.capabilities.includes(\"buffer-view\")) {"));
    assert!(!output.contains(
        "decode(_data: Uint8Array): unknown {\n        throw missingCodecImplementation(requirement);\n      },\n      fromBuffer(_data: Uint8Array): unknown {"
    ));
    assert!(output.contains("Codec ${requirement.id} requires application runtime implementation"));
}

#[test]
fn strict_codec_mode_allows_fastdb_call_db_codecs() {
    let output = generate_typescript_client(
        fastdb_call_db_contract_json().as_bytes(),
        TypeScriptOptions {
            strict_codecs: true,
        },
    )
    .unwrap();

    assert!(output.contains("export interface C2EncodedClientTransport"));
    assert!(
        output.contains(
            "export interface C2HttpRelayEncodedTransport<Payload extends C2ResponsePayload = C2ByteArray> extends C2EncodedClientTransport"
        )
    );
    assert!(output.contains(
        "export interface C2RelayAwareHttpEncodedTransport<Payload extends C2ResponsePayload = C2ByteArray> extends C2EncodedClientTransport"
    ));
    assert!(output.contains("export interface C2ProviderOwnedResponsePayload"));
    assert!(output.contains(
        "export type C2ResponsePayload = C2ByteArray | ArrayBufferLike | C2ProviderOwnedResponsePayload"
    ));
    assert!(output.contains("export interface C2AllocatedResponsePayload"));
    assert!(output.contains("export type C2ResponsePayloadAllocator"));
    assert!(
        output.contains(
            "export type C2ResponsePayloadUnknownLengthStrategy = \"reject\" | \"buffer\";"
        )
    );
    assert!(
        output.contains("readonly responsePayloadAllocator?: C2ResponsePayloadAllocator<Payload>;")
    );
    assert!(output.contains(
        "readonly responsePayloadUnknownLengthStrategy?: C2ResponsePayloadUnknownLengthStrategy;"
    ));
    assert!(output.contains("readonly responsePayloadUnknownLengthMaxBytes?: number;"));
    assert!(output.contains("export interface C2HeldResult<T>"));
    assert!(output.contains("readonly buffer: C2ResponsePayload"));
    assert!(output.contains("export interface C2HeldClientTransport extends C2ClientTransport"));
    assert!(output.contains("export const FASTDB_PORTABLE_METHOD_WIRE"));
    assert!(output.contains("export function createFastdbPortableClientCodecTransport"));
    assert!(
        output.contains(
            "abiHash: \"0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef\""
        )
    );
    assert!(output.contains(
        "signatureHash: \"abcdef0123456789abcdef0123456789abcdef0123456789abcdef0123456789\""
    ));
    assert!(output.contains("export function createHttpRelayEncodedTransport"));
    assert!(output.contains("export function createRelayAwareHttpEncodedTransport"));
    assert!(output.contains("export class C2HttpTransportError"));
    assert!(output.contains("export interface C2IpcConnection"));
    assert!(output.contains("export interface C2NodeIpcSocket"));
    assert!(output.contains("export interface C2NodeIpcNet"));
    assert!(output.contains("export function createNodeIpcConnect"));
    assert!(output.contains("export interface C2IpcShmSegment"));
    assert!(output.contains("export interface C2IpcResponseShmBlock"));
    assert!(output.contains("export interface C2IpcResponseShmReader"));
    assert!(output.contains("export interface C2IpcRequestShmBlock"));
    assert!(output.contains("export interface C2IpcRequestShmWriter"));
    assert!(output.contains("markConsumed?(block: C2IpcRequestShmBlock): void | Promise<void>;"));
    assert!(output.contains("export interface C2NodePosixShmFileHandle"));
    assert!(output.contains("export interface C2NodePosixShmFileSystem"));
    assert!(output.contains("export interface C2NodePosixNativeBuddyResponseShmBackend"));
    assert!(output.contains("export interface C2NodePosixNativeBuddyRequestShmBackend"));
    assert!(output.contains("export interface C2MemFfiResponsePoolBinding"));
    assert!(output.contains("export interface C2MemFfiResponsePoolFactory"));
    assert!(output.contains("export interface C2MemFfiRequestPoolBinding"));
    assert!(output.contains("export function createNodePosixDedicatedResponseShmReader"));
    assert!(output.contains("export function createNodePosixDedicatedRequestShmWriter"));
    assert!(output.contains("export function createNodePosixNativeBuddyResponseShmReader"));
    assert!(output.contains("export function createNodePosixNativeBuddyRequestShmWriter"));
    assert!(output.contains("export function createC2MemFfiNativeBuddyResponseShmReader"));
    assert!(output.contains("export function createC2MemFfiNativeBuddyRequestShmWriter"));
    assert!(output.contains("  read(\n    block: C2IpcResponseShmBlock,"));
    assert!(output.contains("  release(block: C2IpcResponseShmBlock): void | Promise<void>;"));
    assert!(!output.contains("export type C2IpcResponseShmReader = ("));
    assert!(output.contains("export function createIpcEncodedTransport"));
    assert!(
        output.contains(
            "export interface C2IpcTransportOptions<Payload extends C2ResponsePayload = C2ByteArray> {\n  readonly connect: C2IpcConnect;\n  readonly responsePayloadAllocator?: C2ResponsePayloadAllocator<Payload>;\n  readonly responseShmReader?: C2IpcResponseShmReader;\n  readonly requestShmWriter?: C2IpcRequestShmWriter;\n  readonly requestShmThreshold?: number;\n  readonly requestChunkSize?: number;\n}"
        )
    );
    assert!(output.contains("readonly requestShmWriter?: C2IpcRequestShmWriter;"));
    assert!(output.contains("readonly requestShmThreshold?: number;"));
    assert!(output.contains("readonly requestChunkSize?: number;"));
    assert!(output.contains("requestShmWriter markConsumed must be a function when provided"));
    assert!(
        output.contains("export interface C2IpcEncodedTransport<Payload extends C2ResponsePayload = C2ByteArray> extends C2EncodedClientTransport")
    );
    assert!(
        output.contains("readonly responsePayloadAllocator?: C2ResponsePayloadAllocator<Payload>;")
    );
    assert!(output.contains("C-Two IPC responsePayloadAllocator"));
    assert!(output.contains("const C2_IPC_CAP_CHUNKED = 1 << 2;"));
    assert!(output.contains("const C2_IPC_DEFAULT_REQUEST_SHM_THRESHOLD = 4096;"));
    assert!(output.contains("const C2_IPC_DEFAULT_REQUEST_CHUNK_SIZE = 131_072;"));
    assert!(output.contains("const C2_IPC_REQUEST_CHUNK_HEADER_BYTES = 4;"));
    assert!(output.contains("const C2_IPC_BUDDY_PAYLOAD_BYTES = 11;"));
    assert!(output.contains("const C2_IPC_REPLY_CHUNK_META_BYTES = 16;"));
    assert!(output.contains("function normalizeIpcRequestShmWriter"));
    assert!(output.contains("function normalizeNodeIpcNet"));
    assert!(output.contains("function wrapNodeIpcSocket"));
    assert!(output.contains("function waitForNodeIpcConnect"));
    assert!(output.contains("function normalizeIpcRequestShmThreshold"));
    assert!(output.contains("function encodeIpcBuddyPayload"));
    assert!(output.contains("function writeIpcBuddyCallRequestFrame"));
    assert!(output.contains("function markIpcRequestShmBlockConsumed"));
    assert!(output.contains("function releaseIpcRequestShmBlock"));
    assert!(output.contains("function releasableIpcRequestShmBlock"));
    assert!(output.contains("function posixShmNameToPath"));
    assert!(output.contains("function readNodePosixShmFully"));
    assert!(output.contains("function writeNodePosixShmFully"));
    assert!(output.contains("function dedicatedShmSegmentName"));
    assert!(output.contains("function normalizeNodePosixNativeBuddyResponseBackend"));
    assert!(output.contains("function normalizeNodePosixNativeBuddyRequestBackend"));
    assert!(output.contains("function normalizeC2MemFfiResponsePoolFactory"));
    assert!(output.contains("function normalizeC2MemFfiRequestPool"));
    assert!(output.contains("function resolveC2MemFfiResponseSegmentSize"));
    assert!(output.contains("function ensureC2MemFfiFacadeOpen"));
    assert!(output.contains(
        "return await releaseIpcShmResponseBlockAfterAllocatorFailure(reader, block, error);"
    ));
    assert!(output.contains(
        "C-Two IPC responsePayloadAllocator failed and responseShmReader release failed"
    ));
    assert!(output.contains("C-Two c2-mem-ffi ${label} is closed."));
    assert!(
        output.contains("native buddy SHM reader post-read validation failed and release failed")
    );
    assert!(output.contains("native buddy response SHM reader only supports non-dedicated"));
    assert!(output.contains("native buddy request SHM backend must return non-dedicated"));
    assert!(output.contains("backend.markRequestConsumed must be a function when provided"));
    assert!(output.contains("requestShmWriter markConsumed failed"));
    assert!(output.contains("C2_IPC_DEDICATED_SHM_HEADER_BYTES"));
    assert!(output.contains("C2_IPC_MAX_SHM_PREFIX_BYTES"));
    assert!(output.contains("requestShmWriter prefix cannot exceed"));
    assert!(output.contains("requestShmWriter dedicated block offset must be 0"));
    assert!(output.contains("dedicated SHM response offset must be 0"));
    assert!(output.contains("native/FFI c2-mem-compatible backend"));
    assert!(
        output.contains("Node/POSIX dedicated SHM reader only supports dedicated C-Two SHM blocks")
    );
    assert!(output.contains("dedicated request SHM block is unknown or already released"));
    assert!(output.contains("C-Two IPC requestShmWriter"));
    assert!(output.contains("function normalizeIpcRequestChunkSize"));
    assert!(output.contains("function writeIpcCallRequestFrames"));
    assert!(output.contains("function writeIpcChunkedCallRequestFrames"));
    assert!(output.contains("function encodeIpcRequestChunkHeader"));
    assert!(output.contains("function closeIpcConnectionAfterChunkedRequestWriteFailure"));
    assert!(output.contains("C2_IPC_REQUEST_CHUNK_HEADER_BYTES - C2_MAX_WIRE_TEXT_BYTES - 2"));
    assert!(output.contains("C-Two IPC server does not support chunked request calls."));
    assert!(output.contains("C-Two IPC request chunk count"));
    assert!(output.contains("function decodeIpcBuddyPayload"));
    assert!(output.contains("function readIpcShmSuccessPayload"));
    assert!(output.contains("function readIpcChunkedSuccessPayload"));
    assert!(output.contains("function decodeIpcReplyChunkMeta"));
    assert!(output.contains("export class C2IpcTransportError"));
    assert!(output.contains("export class C2IpcRouteNotFoundError"));
    assert!(output.contains("C-Two IPC transport"));
    assert!(output.contains("const C2_IPC_FLAG_CALL_V2 = 1 << 7;"));
    assert!(output.contains("if (payloadLen > C2_MAX_RESPONSE_PAYLOAD_BYTES)"));
    assert!(output.contains(
        "C-Two IPC frame payload length ${payloadLen} must be no greater than ${C2_MAX_RESPONSE_PAYLOAD_BYTES}."
    ));
    assert!(output.contains("C-Two IPC responseShmReader is required for SHM replies."));
    assert!(output.contains("C-Two IPC responseShmReader read must be a function."));
    assert!(output.contains("C-Two IPC responseShmReader release must be a function."));
    assert!(output.contains("await releaseIpcShmResponseBlock(reader, block);"));
    assert!(output.contains("C-Two IPC responseShmReader release failed"));
    assert!(output.contains("C-Two IPC SHM response range"));
    assert!(output.contains("C-Two IPC chunked response final chunk did not complete total_size."));
    assert!(output.contains("C-Two IPC chunked response final chunk must not be empty."));
    assert!(output.contains("C-Two IPC chunked response chunk layout cannot fit total_size."));
    assert!(!output.contains(
        "C-Two IPC SHM and chunked responses are not supported by this generated inline transport yet."
    ));
    assert!(output.contains("function encodeIpcCallControl"));
    assert!(output.contains("function decodeServerIpcHandshake"));
    assert!(output.contains("return { kind: \"success\", payload: payload.subarray(1) };"));
    assert!(output.contains("Object.setPrototypeOf(this, C2HttpTransportError.prototype);"));
    assert!(output.contains("C-Two HTTP relay call fetch failed"));
    assert!(output.contains("C-Two HTTP relay call body read failed"));
    assert!(output.contains("C-Two relay resolve payload shape invalid"));
    assert!(
        output.contains(
            "C-Two relay maxAttempts must be a non-negative safe integer no greater than"
        )
    );
    assert!(output.contains("readonly callTimeoutMs?: number"));
    assert!(output.contains("readonly resolveTimeoutMs?: number"));
    assert!(output.contains("async function fetchWithTimeout"));
    assert!(output.contains("Number.isSafeInteger(value)"));
    assert!(output.contains("const C2_MAX_RESPONSE_PAYLOAD_BYTES = 2_147_483_647;"));
    assert!(output.contains("2_147_483_647"));
    assert!(output.contains("C-Two HTTP relay call"));
    assert!(output.contains("C-Two relay resolve"));
    assert!(output.contains("timed out after"));
    assert!(output.contains("error instanceof C2HttpTransportError"));
    assert!(output.contains("/_resolve/${encodePathSegment(routeName)}?"));
    assert!(output.contains("crm_ns=${encodeQueryValue(contract.namespace)}"));
    assert!(output.contains("route.relayUrl"));
    assert!(output.contains("readonly routeCacheTtlMs?: number"));
    assert!(output.contains("const routeCache = new Map<string, C2RelayRouteCacheEntry>()"));
    assert!(output.contains("relayRouteCacheKey(routeName, routeContract)"));
    assert!(output.contains("C-Two route contract identity must use schema c-two.contract.v1."));
    assert!(output.contains("requireRouteContractTextField(contract.namespace, \"namespace\")"));
    assert!(output.contains("C-Two route contract identity is missing non-empty ${field}."));
    assert!(output.contains("C2_MAX_WIRE_TEXT_BYTES"));
    assert!(output.contains("utf8ByteLength(value) > C2_MAX_WIRE_TEXT_BYTES"));
    assert!(output.contains("C-Two route contract ${field} cannot exceed"));
    assert!(output.contains("C-Two route contract ${field} cannot contain control characters."));
    assert!(
        output.contains("C-Two route contract ${field} cannot contain path or tag separators.")
    );
    assert!(output.contains("requireRouteNamePathValue(routeName)"));
    assert!(output.contains("requireMethodPathValue(method)"));
    assert!(output.contains("C-Two route ${field} must be a non-empty string."));
    assert!(output.contains("C-Two route ${field} cannot contain leading or trailing whitespace."));
    assert!(output.contains("C-Two route ${field} cannot contain control characters."));
    assert!(output.contains("C-Two route ${field} cannot contain path or tag separators."));
    assert!(output.contains("if (field === \"routeName\" ? value.includes(\"\\\\\") : value.includes(\"/\") || value.includes(\"\\\\\"))"));
    assert!(output.contains("resolveRelayRoutesCached("));
    assert!(output.contains("invalidateRelayRouteCache(routeCache, cacheKey)"));
    assert!(output.contains(
        "function invalidateRelayRouteCache(cache: Map<string, C2RelayRouteCacheEntry>, cacheKey: string): void {\n  cache.delete(cacheKey);\n}"
    ));
    assert!(output.contains(
        "orderRelayRoutes(routes, currentRelayByCacheKey.get(cacheKey), excludedRoutes)"
    ));
    assert!(output.contains("readonly maxPayloadSize: number"));
    assert!(output.contains("maxPayloadSize: numberField(item, \"max_payload_size\", index)"));
    assert!(output.contains("if (requestPayload.byteLength > route.maxPayloadSize)"));
    assert!(output.contains("requireUint8Array(payload, \"payload\")"));
    assert!(
        output.contains("function requireUint8Array(value: unknown, label: string): Uint8Array")
    );
    assert!(output.contains("requireResponsePayload(await wireTransport.call(routeName, contract, method, payload), \"encoded transport response\")"));
    assert!(output.contains(
        "function requireResponsePayload(value: unknown, label: string): C2ResponsePayload"
    ));
    assert!(output.contains(
        "`${label} must be a Uint8Array, ArrayBuffer, or provider-owned response payload.`"
    ));
    assert!(output.contains("`${label} must be a Uint8Array.`"));
    assert!(output.contains("Codec ${codec.id} encode result"));
    assert!(output.contains("encoded transport response"));
    assert!(output.contains("function requireCodecImplementation("));
    assert!(output.contains("Codec ${requirement.id} implementation encode must be a function."));
    assert!(output.contains("Codec ${requirement.id} implementation decode must be a function."));
    assert!(
        output.contains("Codec ${requirement.id} implementation fromBuffer must be a function.")
    );
    assert!(output.contains("headers[\"x-c2-expected-abi-hash\"] = routeContract.abiHash;"));
    assert!(output.contains("function normalizeHttpHeaders("));
    assert!(output.contains("function normalizeHttpRelayTransportOptions"));
    assert!(output.contains("C2_RESERVED_HTTP_HEADER_NAMES"));
    assert!(output.contains("function requestHeadersForRouteContract("));
    assert!(output.contains("headers[\"Content-Type\"] = \"application/octet-stream\";"));
    assert!(output.contains("cannot override reserved C-Two HTTP header"));
    assert!(output.contains("C2_HTTP_HEADER_NAME_RE"));
    assert!(output.contains("Object.create(null) as Record<string, string>"));
    assert!(output.contains("contains invalid HTTP header name"));
    assert!(output.contains("contains invalid HTTP header value"));
    assert!(output.contains("function normalizeFetch("));
    assert!(output.contains("function normalizeResponsePayloadAllocator"));
    assert!(output.contains("function normalizeResponsePayloadUnknownLengthStrategy"));
    assert!(output.contains("function normalizeUnknownLengthBufferMaxBytes"));
    assert!(output.contains("responsePayloadUnknownLengthMaxBytes,"));
    assert!(output.contains("responsePayloadUnknownLengthStrategy,"));
    assert!(output.contains("responsePayloadUnknownLengthMaxBytes,"));
    assert!(
        output.contains(
            "function responseContentLength(response: C2FetchResponse): number | undefined"
        )
    );
    assert!(
        output.contains(
            "async function readHttpArrayBufferResponsePayload(response: C2FetchResponse): Promise<Uint8Array>"
        )
    );
    assert!(
        output.contains("return await readHttpArrayBufferResponsePayload(response) as Payload;")
    );
    assert!(output.contains("errorPayload = await readHttpArrayBufferResponsePayload(response);"));
    assert!(output.contains(
        "C-Two HTTP relay response Content-Length must be no greater than ${C2_MAX_RESPONSE_PAYLOAD_BYTES}."
    ));
    assert!(
        output
            .contains("C-Two HTTP relay response body byte length does not match Content-Length.")
    );
    assert!(output.contains(
        "async function readHttpTextResponseBody(response: C2FetchResponse): Promise<string>"
    ));
    assert!(output.contains("body = await readHttpTextResponseBody(response);"));
    assert!(
        output.contains("function readAllocatedResponsePayload<Payload extends C2ResponsePayload>")
    );
    assert!(output.contains(
        "function readUnknownLengthAllocatedResponsePayload<Payload extends C2ResponsePayload>"
    ));
    assert!(output.contains("chunks.push(chunk.value.slice());"));
    assert!(output.contains("nextByteLength > maxBytes"));
    assert!(output.contains("exceeded responsePayloadUnknownLengthMaxBytes (${maxBytes})"));
    assert!(output.contains("let reader: C2ReadableStreamReader | undefined;"));
    assert!(output.contains("reader = body.getReader();"));
    assert!(output.contains("tryReleaseAllocatedResponsePayload(allocated);"));
    assert!(output.contains("reader?.releaseLock?.();"));
    assert!(output.contains("function releaseAllocatedResponsePayload"));
    assert!(output.contains("function tryReleaseAllocatedResponsePayload"));
    assert!(output.contains("function releaseInvalidAllocatedResponsePayload"));
    assert!(output.contains("function tryReleaseInvalidAllocatedResponsePayload"));
    assert!(output.contains("releaseHeldValue(value.payload);"));
    assert!(
        output.contains("Content-Length is required when responsePayloadAllocator is configured")
    );
    assert!(output.contains("must be \"reject\" or \"buffer\""));
    assert!(output.contains("unknown-length response stream byte length must be a safe integer"));
    assert!(output.contains("typeof candidate !== \"function\""));
    assert!(output.contains("`${label} must be a string.`"));
    assert!(output.contains("`${label} cannot include whitespace.`"));
    assert!(output.contains("parsed = new URL(normalized)"));
    assert!(output.contains("must be an absolute HTTP(S) URL"));
    assert!(output.contains("`${label} cannot include credentials.`"));
    assert!(output.contains("cannot include query or fragment components"));
    assert!(output.contains("`C-Two relay resolve route ${index} relay_url`"));
    assert!(output.contains("encodePathSegment(routeName)"));
    assert!(output.contains("async query(): Promise<unknown>"));
    assert!(output.contains("async hold_query(): Promise<C2HeldResult<unknown>>"));
    assert!(output.contains("return requireHeldTransport(this.transport).hold<unknown>"));
    assert!(output.contains("requireCodecImplementation(codecs, wire.output)"));
    assert!(
        output.contains(
            "if (codec.fromBuffer && wire.output.capabilities.includes(\"buffer-view\"))"
        )
    );
    assert!(output.contains(
        "return createHeldResult(value as T, true, retainableResponsePayload(response));"
    ));
    assert!(output.contains(
        "return createHeldResult(value as T, false, retainableResponsePayload(response), true);"
    ));
    assert!(output.contains("tryReleaseResponsePayload(response);"));
    assert!(
        output.contains(
            "function retainableResponsePayload(value: C2ResponsePayload): C2ResponsePayload {\n  return value;\n}"
        )
    );
    assert!(output.contains("C2HeldResult released: value no longer accessible."));
    assert!(output.contains("C2HeldResult released: buffer no longer accessible."));
    assert!(output.contains("private readonly releaseBuffer = false"));
    assert!(output.contains("if (this.releaseBuffer && buffer !== value) {"));
    assert!(output.contains(
        "function tryReleaseResponsePayload(value: C2ResponsePayload | undefined): void"
    ));
    assert!(output.contains("id: \"org.fastdb.call-db\""));
    assert!(output.contains("schema: \"fastdb.call-db.schema.v1\""));
    assert!(output.contains("capabilities: [\"bytes\", \"buffer-view\"]"));
    assert!(output.contains("[...requirement.capabilities].sort().join(\",\")"));
}

#[test]
fn strict_codec_mode_rejects_external_codecs() {
    let err = generate_typescript_client(
        external_codec_contract_json().as_bytes(),
        TypeScriptOptions {
            strict_codecs: true,
        },
    )
    .unwrap_err();

    assert!(
        err.to_string()
            .contains("unsupported codec org.example.payload")
    );
}

#[test]
fn strict_codec_mode_rejects_opaque_primitive_wire_refs() {
    let err = generate_typescript_client(
        opaque_primitive_contract_json().as_bytes(),
        TypeScriptOptions {
            strict_codecs: true,
        },
    )
    .unwrap_err();

    assert!(
        err.to_string()
            .contains("unsupported codec org.example.text-args")
    );
}

#[test]
fn strict_codec_mode_rejects_raw_fastdb_feature_wire_refs() {
    let err = generate_typescript_client(
        fastdb_raw_feature_contract_json().as_bytes(),
        TypeScriptOptions {
            strict_codecs: true,
        },
    )
    .unwrap_err();

    assert!(
        err.to_string()
            .contains("unsupported codec org.fastdb.columnar")
    );
}

#[test]
fn strict_codec_mode_rejects_malformed_fastdb_call_db_refs() {
    let missing_schema = fastdb_call_db_contract_json().replace(
        "\n              \"schema\": \"fastdb.call-db.schema.v1\",",
        "",
    );
    let err = generate_typescript_client(
        missing_schema.as_bytes(),
        TypeScriptOptions {
            strict_codecs: true,
        },
    )
    .unwrap_err();
    assert!(
        err.to_string()
            .contains("fastdb call-db codec refs must use schema fastdb.call-db.schema.v1")
    );

    let wrong_schema =
        fastdb_call_db_contract_json().replace("fastdb.call-db.schema.v1", "fastdb.schema.v1");
    let err = generate_typescript_client(
        wrong_schema.as_bytes(),
        TypeScriptOptions {
            strict_codecs: true,
        },
    )
    .unwrap_err();
    assert!(
        err.to_string()
            .contains("fastdb call-db codec refs must use schema fastdb.call-db.schema.v1")
    );

    let missing_hash = fastdb_call_db_contract_json().replace(
        "\n              \"schema_sha256\": \"aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa\",",
        "",
    );
    let err = generate_typescript_client(
        missing_hash.as_bytes(),
        TypeScriptOptions {
            strict_codecs: true,
        },
    )
    .unwrap_err();
    assert!(
        err.to_string()
            .contains("fastdb call-db codec refs must include schema_sha256")
    );

    let missing_bytes_capability =
        fastdb_call_db_contract_json().replace("\"bytes\", \"buffer-view\"", "\"buffer-view\"");
    let err = generate_typescript_client(
        missing_bytes_capability.as_bytes(),
        TypeScriptOptions {
            strict_codecs: true,
        },
    )
    .unwrap_err();
    assert!(
        err.to_string()
            .contains("fastdb call-db codec refs must include bytes capability")
    );
}

#[test]
fn rejects_non_string_codec_identity_fields_before_typescript_generation() {
    let mut descriptor: Value = serde_json::from_str(&opaque_primitive_contract_json()).unwrap();
    descriptor["methods"][0]["wire"]["input"]["media_type"] = serde_json::json!(42);

    let err = generate_typescript_client(
        descriptor.to_string().as_bytes(),
        TypeScriptOptions::default(),
    )
    .unwrap_err();

    assert!(
        err.to_string()
            .contains("$.methods[0].wire.input.media_type")
    );
    assert!(err.to_string().contains("expected string"));
}

#[test]
fn rejects_invalid_contract_descriptor() {
    let err = generate_typescript_client(br#"{"schema":"wrong"}"#, TypeScriptOptions::default())
        .unwrap_err();

    assert!(err.to_string().contains("contract descriptor invalid"));
}

#[test]
fn rejects_typescript_method_identifier_collisions() {
    let err = generate_typescript_client(
        name_collision_contract_json().as_bytes(),
        TypeScriptOptions::default(),
    )
    .unwrap_err();

    assert!(err.to_string().contains("method name collision"));
}

#[test]
fn rejects_typescript_parameter_identifier_collisions() {
    let err = generate_typescript_client(
        parameter_collision_contract_json().as_bytes(),
        TypeScriptOptions::default(),
    )
    .unwrap_err();

    assert!(err.to_string().contains("parameter name collision"));
}
