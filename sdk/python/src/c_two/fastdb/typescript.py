"""C-Two-owned FastDB TypeScript helper generation."""

from __future__ import annotations

import argparse

import json
from pathlib import Path
from typing import Any

from fastdb4py.schema import SCHEMA_VERSION, schema_sha256

FASTDB_CODEC_IDS = {
    'org.fastdb.columnar': 'columnar.v1',
    'org.fastdb.object-graph': 'object_graph.v1',
}
CALL_DB_CODEC_ID = 'org.fastdb.call-db'
CALL_DB_SCHEMA_VERSION = 'fastdb.call-db.schema.v1'
CALL_DB_COLUMNAR_PROFILE = 'fastdb.call.columnar.v1'
CALL_DB_OBJECT_GRAPH_PROFILE = 'fastdb.call.object-graph.v1'
CALL_DB_ARRAY_VALUE_FIELD = 'value'

_SCALAR_SCHEMA = {
    'bool': ('BOOL', 'boolean'),
    'u8': ('U8', 'number'),
    'u16': ('U16', 'number'),
    'u32': ('U32', 'number'),
    'i32': ('I32', 'number'),
    'u8n': ('U8N', 'number'),
    'u16n': ('U16N', 'number'),
    'f32': ('F32', 'number'),
    'f64': ('F64', 'number'),
    'str': ('STR', 'string'),
    'wstr': ('WSTR', 'string'),
    'bytes': ('BYTES', 'Uint8Array'),
}

_NATIVE_LIST_ITEM_KINDS = {'bool', 'u8', 'u16', 'u32', 'i32', 'u8n', 'u16n', 'f32', 'f64'}
_CodecRequirementKey = tuple[str, str, str, str, str, tuple[str, ...]]


class CTwoCodegenError(Exception):
    pass


def generate_c_two_typescript_helpers(
    contract_descriptor: dict[str, Any] | str | bytes,
    schema_descriptors: list[dict[str, Any] | list[dict[str, Any]] | str | bytes],
) -> str:
    contract = _coerce_json(contract_descriptor, label='contract descriptor')
    _validate_c_two_contract_descriptor_shape(contract)
    descriptors = _coerce_schema_descriptors(schema_descriptors)
    feature_schemas, call_db_schemas = _split_schema_descriptors(descriptors)
    schemas_by_hash, schemas_by_identity = _index_schemas(feature_schemas)
    call_db_schemas_by_hash = _index_call_db_schemas(call_db_schemas)
    requirements = _collect_fastdb_requirements(contract)
    call_db_ref_sources = _collect_call_db_ref_sources(contract)
    matched_features: list[tuple[dict[str, Any], dict[str, Any]]] = []
    matched_calls: list[tuple[dict[str, Any], dict[str, Any]]] = []
    for codec_ref in requirements:
        digest = codec_ref.get('schema_sha256')
        if not digest:
            raise CTwoCodegenError(f'fastdb codec {codec_ref.get("id")!r} is missing schema_sha256.')
        if codec_ref.get('id') == CALL_DB_CODEC_ID:
            if codec_ref.get('schema') != CALL_DB_SCHEMA_VERSION:
                raise CTwoCodegenError(
                    f'fastdb call-db codec must reference schema {CALL_DB_SCHEMA_VERSION}.',
                )
            call_schema = call_db_schemas_by_hash.get(digest)
            if call_schema is None:
                raise CTwoCodegenError(
                    f'No fastdb.call-db.schema.v1 descriptor supplied for schema hash {digest}.',
                )
            _validate_call_db_schema(codec_ref, call_schema, schemas_by_hash)
            _validate_call_db_contract_binding(codec_ref, call_schema, call_db_ref_sources)
            matched_calls.append((codec_ref, call_schema))
            continue
        if codec_ref.get('schema') != SCHEMA_VERSION:
            raise CTwoCodegenError(
                f'fastdb codec {codec_ref.get("id")!r} must reference schema {SCHEMA_VERSION}.',
            )
        schema = schemas_by_hash.get(digest)
        if schema is None:
            raise CTwoCodegenError(f'No fastdb.schema.v1 descriptor supplied for schema hash {digest}.')
        _validate_codec_schema_profile(codec_ref, schema)
        matched_features.append((codec_ref, schema))

    needed_identities = _schema_identity_closure(
        [
            schema
            for _codec, schema in matched_features
        ] + [
            schema
            for _codec, call_schema in matched_calls
            for schema in _feature_schemas_for_call_db(call_schema, schemas_by_hash)
        ],
        schemas_by_identity,
    )
    emitted_schemas = [
        schema
        for identity, schema in schemas_by_identity.items()
        if identity in needed_identities
    ]
    return _render_typescript_helpers(
        contract,
        matched_features,
        matched_calls,
        emitted_schemas,
        schemas_by_identity,
        schemas_by_hash,
    )


def run_codegen_c_two_ts(
    contract_path: str,
    output_path: str,
    schema_paths: list[str],
) -> None:
    contract = json.loads(Path(contract_path).read_text())
    schemas = [json.loads(Path(path).read_text()) for path in schema_paths]
    output = generate_c_two_typescript_helpers(contract, schemas)
    Path(output_path).write_text(output)


def _coerce_json(value: dict[str, Any] | str | bytes, *, label: str) -> dict[str, Any]:
    if isinstance(value, dict):
        return value
    if isinstance(value, bytes):
        value = value.decode()
    if isinstance(value, str):
        parsed = json.loads(value)
        if isinstance(parsed, dict):
            return parsed
    raise CTwoCodegenError(f'{label} must be a JSON object.')


def _coerce_schema_descriptors(
    values: list[dict[str, Any] | list[dict[str, Any]] | str | bytes],
) -> list[dict[str, Any]]:
    descriptors: list[dict[str, Any]] = []
    for value in values:
        if isinstance(value, bytes):
            value = value.decode()
        if isinstance(value, str):
            value = json.loads(value)
        if isinstance(value, list):
            for item in value:
                if not isinstance(item, dict):
                    raise CTwoCodegenError('fastdb schema artifact bundle entries must be JSON objects.')
                descriptors.append(item)
            continue
        if isinstance(value, dict):
            descriptors.append(value)
            continue
        raise CTwoCodegenError('fastdb schema descriptor must be a JSON object or an artifact bundle array.')
    return descriptors


def _schema_hash(descriptor: dict[str, Any]) -> str:
    _schema_identity(descriptor)
    return schema_sha256(descriptor)


def _index_schemas(
    schemas: list[dict[str, Any]],
) -> tuple[dict[str, dict[str, Any]], dict[str, dict[str, Any]]]:
    by_hash: dict[str, dict[str, Any]] = {}
    by_identity: dict[str, dict[str, Any]] = {}
    identity_hashes: dict[str, str] = {}
    for descriptor in schemas:
        identity = _schema_identity(descriptor)
        digest = schema_sha256(descriptor)
        previous_hash = identity_hashes.get(identity)
        if previous_hash is not None:
            if previous_hash != digest:
                raise CTwoCodegenError(
                    'duplicate fastdb schema identity with different hashes: '
                    f'{identity} has {previous_hash} and {digest}.',
                )
            continue
        identity_hashes[identity] = digest
        by_hash[digest] = descriptor
        by_identity[identity] = descriptor
    return by_hash, by_identity


def _split_schema_descriptors(
    descriptors: list[dict[str, Any]],
) -> tuple[list[dict[str, Any]], list[dict[str, Any]]]:
    feature_schemas: list[dict[str, Any]] = []
    call_db_schemas: list[dict[str, Any]] = []
    for descriptor in descriptors:
        schema = descriptor.get('schema')
        if schema == SCHEMA_VERSION:
            feature_schemas.append(descriptor)
            continue
        if schema == CALL_DB_SCHEMA_VERSION:
            call_db_schemas.append(descriptor)
            continue
        raise CTwoCodegenError(
            f'Expected {SCHEMA_VERSION} or {CALL_DB_SCHEMA_VERSION}, got {schema!r}.',
        )
    return feature_schemas, call_db_schemas


def _index_call_db_schemas(
    schemas: list[dict[str, Any]],
) -> dict[str, dict[str, Any]]:
    by_hash: dict[str, dict[str, Any]] = {}
    for descriptor in schemas:
        digest = schema_sha256(descriptor)
        existing = by_hash.get(digest)
        if existing is not None and existing != descriptor:
            raise CTwoCodegenError(
                f'duplicate fastdb call-db schema hash with different descriptors: {digest}.',
            )
        by_hash[digest] = descriptor
    return by_hash


def _validate_c_two_contract_descriptor_shape(contract: dict[str, Any]) -> None:
    if contract.get('schema') != 'c-two.contract.v1':
        raise CTwoCodegenError('contract descriptor must use schema c-two.contract.v1.')
    crm = contract.get('crm')
    if not isinstance(crm, dict):
        raise CTwoCodegenError('c-two.contract.v1 descriptor must contain a crm object.')
    for key in ('namespace', 'name', 'version'):
        if not isinstance(crm.get(key), str) or not crm.get(key):
            raise CTwoCodegenError(f'c-two.contract.v1 crm.{key} must be a non-empty string.')
    fingerprints = contract.get('fingerprints')
    if not isinstance(fingerprints, dict):
        raise CTwoCodegenError('c-two.contract.v1 descriptor must contain route fingerprints.')
    _validate_contract_hash(fingerprints.get('abi_hash'), 'abi_hash')
    _validate_contract_hash(fingerprints.get('signature_hash'), 'signature_hash')
    if not isinstance(contract.get('methods'), list):
        raise CTwoCodegenError('c-two.contract.v1 descriptor methods must be a list.')


def _validate_contract_hash(value: Any, field: str) -> None:
    if not isinstance(value, str) or len(value) != 64 or any(ch not in '0123456789abcdef' for ch in value):
        raise CTwoCodegenError(f'c-two.contract.v1 fingerprints.{field} must be a 64-character lowercase hex hash.')


def _schema_identity(descriptor: dict[str, Any]) -> str:
    if descriptor.get('schema') != SCHEMA_VERSION:
        raise CTwoCodegenError(f'Expected {SCHEMA_VERSION}, got {descriptor.get("schema")!r}.')
    feature = descriptor.get('feature')
    if not isinstance(feature, dict):
        raise CTwoCodegenError('fastdb schema descriptor must contain a feature object.')
    identity = feature.get('identity')
    if not isinstance(identity, str) or not identity:
        raise CTwoCodegenError('fastdb schema feature.identity must be a non-empty string.')
    name = feature.get('name')
    if not isinstance(name, str) or not name:
        raise CTwoCodegenError('fastdb schema feature.name must be a non-empty string.')
    fields = descriptor.get('fields')
    if not isinstance(fields, list):
        raise CTwoCodegenError('fastdb schema descriptor fields must be a list.')
    return identity


def _collect_fastdb_requirements(contract: dict[str, Any]) -> list[dict[str, Any]]:
    requirements: dict[_CodecRequirementKey, dict[str, Any]] = {}
    schema_requirement_keys: dict[tuple[str, str], _CodecRequirementKey] = {}
    for method in contract.get('methods', []):
        wire = method.get('wire') or {}
        for position in ('input', 'output'):
            codec_ref = wire.get(position)
            if not isinstance(codec_ref, dict):
                continue
            codec_id = codec_ref.get('id')
            if codec_id in FASTDB_CODEC_IDS or codec_id == CALL_DB_CODEC_ID:
                key = _codec_requirement_key(codec_ref)
                schema_key = _codec_schema_identity_key(codec_ref)
                if schema_key is not None:
                    previous = schema_requirement_keys.get(schema_key)
                    if previous is not None and previous != key:
                        raise CTwoCodegenError(
                            'fastdb codec refs with the same id and schema hash have conflicting codec requirement keys: '
                            f'{schema_key[0]} {schema_key[1]} uses '
                            f'{_format_codec_requirement_key(previous)} and {_format_codec_requirement_key(key)}.',
                        )
                    schema_requirement_keys[schema_key] = key
                requirements[key] = codec_ref
    return list(requirements.values())


def _codec_requirement_key(codec_ref: dict[str, Any]) -> _CodecRequirementKey:
    return (
        _codec_key_string(codec_ref, 'id'),
        _codec_key_string(codec_ref, 'version'),
        _codec_key_string(codec_ref, 'schema'),
        _codec_key_string(codec_ref, 'schema_sha256'),
        _codec_key_string(codec_ref, 'media_type'),
        _codec_capability_key(codec_ref),
    )


def _codec_key_string(codec_ref: dict[str, Any], field: str) -> str:
    value = _codec_optional_string(codec_ref, field)
    return value or ''


def _codec_optional_string(codec_ref: dict[str, Any], field: str) -> str | None:
    value = codec_ref.get(field)
    if value is None:
        return None
    if not isinstance(value, str):
        raise CTwoCodegenError(f'fastdb codec requirement {field} must be a string.')
    return value


def _codec_schema_identity_key(codec_ref: dict[str, Any]) -> tuple[str, str] | None:
    codec_id = codec_ref.get('id')
    digest = codec_ref.get('schema_sha256')
    if not isinstance(codec_id, str) or not codec_id:
        return None
    if not isinstance(digest, str) or not digest:
        return None
    return (codec_id, digest)


def _codec_capability_key(codec_ref: dict[str, Any]) -> tuple[str, ...]:
    capabilities = codec_ref.get('capabilities', [])
    if capabilities is None:
        return ()
    if not isinstance(capabilities, list):
        raise CTwoCodegenError('fastdb codec requirement capabilities must be a list of strings.')
    normalized = []
    for capability in capabilities:
        if not isinstance(capability, str):
            raise CTwoCodegenError('fastdb codec requirement capabilities must be a list of strings.')
        normalized.append(capability)
    return tuple(sorted(normalized))


def _format_codec_requirement_key(key: _CodecRequirementKey) -> str:
    codec_id, version, schema, digest, media_type, capabilities = key
    capability_key = ','.join(capabilities)
    return f'({codec_id}|{version}|{schema}|{digest}|{media_type}|{capability_key})'


def _collect_call_db_ref_sources(
    contract: dict[str, Any],
) -> dict[_CodecRequirementKey, list[tuple[str, str]]]:
    sources: dict[_CodecRequirementKey, list[tuple[str, str]]] = {}
    for method in contract.get('methods', []):
        wire = method.get('wire') or {}
        for direction in ('input', 'output'):
            codec_ref = wire.get(direction)
            if not isinstance(codec_ref, dict) or codec_ref.get('id') != CALL_DB_CODEC_ID:
                continue
            method_name = method.get('name')
            if not isinstance(method_name, str) or not method_name:
                raise CTwoCodegenError(
                    'c-two.contract.v1 methods using fastdb call-db refs must include non-empty names.',
                )
            sources.setdefault(_codec_requirement_key(codec_ref), []).append((method_name, direction))
    return sources


def _render_typescript_helpers(
    contract: dict[str, Any],
    matched_features: list[tuple[dict[str, Any], dict[str, Any]]],
    matched_calls: list[tuple[dict[str, Any], dict[str, Any]]],
    schemas: list[dict[str, Any]],
    schemas_by_identity: dict[str, dict[str, Any]],
    schemas_by_hash: dict[str, dict[str, Any]],
) -> str:
    imports = _collect_imports(schemas, schemas_by_identity)
    type_imports: set[str] = set()
    if matched_features:
        type_imports.update({'FastdbDatabaseBytes', 'FastdbModule', 'FastdbOwnedBytes'})
        imports.update({'allocateFastdbOwnedBytes', 'decodeFastdbFeature', 'encodeFastdbFeature'})
    if matched_calls:
        type_imports.update({'FastdbCallDbView', 'FastdbDatabaseBytes', 'FastdbModule', 'FastdbOwnedBytes'})
        imports.update({'allocateFastdbOwnedBytes', 'decodeFastdbCallDb', 'encodeFastdbCallDb', 'viewFastdbCallDb'})
    lines: list[str] = ['// Auto-generated by c3 contract codegen typescript --fastdb-schema. Do not edit manually.']
    if imports:
        lines.append(f"import {{ {', '.join(sorted(imports))} }} from 'fastdb4ts';")
    if type_imports:
        lines.append(f"import type {{ {', '.join(sorted(type_imports))} }} from 'fastdb4ts';")
    lines.append('')
    if not matched_features and not matched_calls:
        lines.append('export const FASTDB_C2_CODEC_BINDINGS = [];')
        return '\n'.join(lines) + '\n'

    lines.extend([
        'export interface FastdbC2CodecRequirement {',
        '  readonly id: string;',
        '  readonly version: string;',
        '  readonly schema?: string;',
        '  readonly schemaSha256?: string;',
        '  readonly mediaType?: string;',
        '  readonly capabilities: readonly string[];',
        '}',
        '',
        'export interface FastdbC2CodecImplementation {',
        '  readonly id: string;',
        '  readonly requirement: FastdbC2CodecRequirement;',
        '  encode(value: unknown): Uint8Array;',
        '  decode(payload: FastdbDatabaseBytes): unknown;',
        '  fromBuffer?(payload: FastdbDatabaseBytes): unknown;',
        '}',
        '',
        'export type FastdbC2CodecRegistry = Readonly<Record<string, FastdbC2CodecImplementation>>;',
        '',
        'export function fastdbC2CodecRequirementKey(requirement: FastdbC2CodecRequirement): string {',
        '  return [',
        '    requirement.id,',
        '    requirement.version,',
        '    requirement.schema ?? "",',
        '    requirement.schemaSha256 ?? "",',
        '    requirement.mediaType ?? "",',
        '    [...requirement.capabilities].sort().join(","),',
        '  ].join("|");',
        '}',
        '',
        'export type FastdbC2TransportResponsePayload = FastdbDatabaseBytes | { readonly byteLength: number };',
        '',
        'export interface FastdbC2AllocatedResponsePayload {',
        '  readonly payload: FastdbDatabaseBytes;',
        '  readonly view: Uint8Array;',
        '  release(): void;',
        '}',
        '',
        'export type FastdbC2ResponsePayloadAllocator = (byteLength: number) => FastdbC2AllocatedResponsePayload;',
        '',
        'export function createFastdbC2ResponsePayloadAllocator(module: FastdbModule): FastdbC2ResponsePayloadAllocator {',
        '  return (byteLength: number): FastdbC2AllocatedResponsePayload => {',
        '    if (byteLength === 0) {',
        '      const payload = new Uint8Array();',
        '      return { payload, view: payload, release(): void {} };',
        '    }',
        '    const payload = allocateFastdbOwnedBytes(module, byteLength);',
        '    return { payload, view: payload.view, release(): void { payload.release(); } };',
        '  };',
        '}',
        '',
    ])

    if matched_features:
        lines.extend([
            'export interface FastdbC2CodecBinding<T extends Feature> {',
            '  readonly codecId: string;',
            '  readonly version: string;',
            '  readonly schema: string;',
            '  readonly profile: string;',
            '  readonly schemaSha256: string;',
            '  readonly mediaType?: string;',
            '  readonly capabilities: readonly string[];',
            '  readonly feature: { new (): T; readonly name: string; readonly schema: unknown; };',
            '  encode(value: T): Uint8Array;',
            '  decode(payload: FastdbDatabaseBytes): T;',
            '}',
            '',
        ])
    if matched_calls:
        lines.extend([
            'export type FastdbC2CallDbView = FastdbCallDbView;',
            '',
            'export interface FastdbC2CallDbScalarField {',
            '  readonly name: string;',
            '  readonly kind: string;',
            '  readonly parameter?: string;',
            '  readonly valuePosition: number;',
            '}',
            '',
            'export interface FastdbC2CallDbArrayItem {',
            '  readonly name: string;',
            '  readonly kind: string;',
            '}',
            '',
            'export interface FastdbC2CallDbTable {',
            '  readonly name: string;',
            '  readonly kind: "scalars" | "array" | "feature";',
            '  readonly cardinality: string;',
            '  readonly feature?: { new (): Feature; readonly name: string; readonly schema: unknown; };',
            '  readonly featureSchemaSha256?: string;',
            '  readonly featureDependencies?: readonly FastdbC2CallDbFeatureDependency[];',
            '  readonly parameter?: string;',
            '  readonly returnIndex?: number;',
            '  readonly valuePosition?: number;',
            '  readonly fields?: readonly FastdbC2CallDbScalarField[];',
            '  readonly item?: FastdbC2CallDbArrayItem;',
            '}',
            '',
            'export interface FastdbC2CallDbFeatureDependency {',
            '  readonly feature: { new (): Feature; readonly name: string; readonly schema: unknown; };',
            '  readonly featureSchemaSha256: string;',
            '}',
            '',
            'export interface FastdbC2CallDbBinding {',
            '  readonly codecId: string;',
            '  readonly version: string;',
            '  readonly schema: string;',
            '  readonly profile: string;',
            '  readonly schemaSha256: string;',
            '  readonly mediaType?: string;',
            '  readonly capabilities: readonly string[];',
            '  readonly method: string;',
            '  readonly direction: "input" | "output";',
            '  readonly tables: readonly FastdbC2CallDbTable[];',
            '  encode(value: unknown): Uint8Array;',
            '  decode(payload: FastdbDatabaseBytes): unknown;',
            '  view(payload: FastdbDatabaseBytes): FastdbC2CallDbView;',
            '  scalar(payload: FastdbDatabaseBytes, nameOrIndex: string | number): unknown;',
            '}',
            '',
        ])
    class_names: dict[str, tuple[str, str]] = {}
    for schema in schemas:
        original_name = schema['feature']['name']
        class_name = _identifier(original_name)
        identity = schema['feature']['identity']
        previous = class_names.setdefault(class_name, (original_name, identity))
        if previous[1] != identity:
            raise CTwoCodegenError(
                'feature name collision after TypeScript identifier sanitization: '
                f'{previous[0]!r} and {original_name!r} both map to {class_name!r}.',
            )
        lines.append(_render_schema_class(schema, schemas_by_identity))
        lines.append('')
    binding_names = []
    for codec_ref, schema in matched_features:
        binding_name = _binding_name(schema, codec_ref)
        binding_names.append(binding_name)
        lines.append(_render_codec_binding(binding_name, codec_ref, schema))
        lines.append('')
    for codec_ref, call_schema in matched_calls:
        binding_name = _call_db_binding_name(call_schema)
        binding_names.append(binding_name)
        lines.append(_render_call_db_binding(binding_name, codec_ref, call_schema, schemas_by_hash))
        lines.append('')
    typed_client = _render_typed_client_facade(contract, matched_calls, schemas_by_hash)
    if typed_client:
        lines.append(typed_client)
        lines.append('')
    lines.append(f'export const FASTDB_C2_CODEC_BINDINGS = [{", ".join(binding_names)}] as const;')
    lines.append('')
    lines.extend([
        'export function createFastdbC2CodecRegistry(): FastdbC2CodecRegistry {',
        '  const registry: Record<string, FastdbC2CodecImplementation> = {};',
        '  for (const binding of FASTDB_C2_CODEC_BINDINGS) {',
        '    const requirement: FastdbC2CodecRequirement = {',
        '      id: binding.codecId,',
        '      version: binding.version,',
        '      schema: binding.schema,',
        '      schemaSha256: binding.schemaSha256,',
        '      mediaType: binding.mediaType,',
        '      capabilities: binding.capabilities,',
        '    };',
        '    const runtime = binding as unknown as {',
        '      encode(value: unknown): Uint8Array;',
        '      decode(payload: FastdbDatabaseBytes): unknown;',
        '      view?: (payload: FastdbDatabaseBytes) => unknown;',
        '    };',
        '    const codec: FastdbC2CodecImplementation = {',
        '      id: binding.codecId,',
        '      requirement,',
        '      encode(value: unknown): Uint8Array {',
        '        return runtime.encode.call(binding, value);',
        '      },',
        '      decode(payload: FastdbDatabaseBytes): unknown {',
        '        return runtime.decode.call(binding, payload);',
        '      },',
        '    };',
        '    if (runtime.view && binding.capabilities.includes("buffer-view")) {',
        '      codec.fromBuffer = (payload: FastdbDatabaseBytes): unknown => runtime.view!.call(binding, payload);',
        '    }',
        '    registry[fastdbC2CodecRequirementKey(requirement)] = codec;',
        '  }',
        '  return registry;',
        '}',
    ])
    return '\n'.join(lines) + '\n'


def _schema_identity_closure(
    roots: list[dict[str, Any]],
    schemas_by_identity: dict[str, dict[str, Any]],
) -> set[str]:
    needed = {schema['feature']['identity'] for schema in roots}
    changed = True
    while changed:
        changed = False
        for identity in list(needed):
            schema = schemas_by_identity.get(identity)
            if schema is None:
                raise CTwoCodegenError(f'Missing fastdb schema descriptor for ref target {identity}.')
            for target in _iter_ref_targets(schema):
                target_schema = _ref_target_schema(
                    target,
                    schemas_by_identity,
                    context=f'feature {schema["feature"]["name"]!r}',
                )
                target_identity = target_schema['feature']['identity']
                if target_identity not in needed:
                    needed.add(target_identity)
                    changed = True
    return needed


def _iter_ref_targets(schema: dict[str, Any]):
    for field in schema.get('fields', []):
        if field.get('kind') == 'ref':
            yield field.get('target') or {}
        elif field.get('kind') == 'list':
            item = field.get('item') or {}
            if item.get('kind') == 'ref':
                yield item.get('target') or {}


def _validate_codec_schema_profile(codec_ref: dict[str, Any], schema: dict[str, Any]) -> None:
    codec_id = codec_ref.get('id')
    if codec_id == 'org.fastdb.columnar':
        for field in schema.get('fields', []):
            if field.get('kind') == 'ref':
                raise CTwoCodegenError(
                    f'columnar codec cannot represent ref field {field.get("name")!r}; use org.fastdb.object-graph.',
                )
            if field.get('kind') == 'list' and (field.get('item') or {}).get('kind') == 'ref':
                raise CTwoCodegenError(
                    f'columnar codec cannot represent list[ref] field {field.get("name")!r}; use org.fastdb.object-graph.',
                )
        _validate_typescript_columnar_runtime_schema(schema)
        return
    if codec_id == 'org.fastdb.object-graph':
        _validate_typescript_object_graph_runtime_schema(schema)
        return
    raise CTwoCodegenError(f'unsupported fastdb codec {codec_id!r}.')


def _validate_call_db_schema(
    codec_ref: dict[str, Any],
    call_schema: dict[str, Any],
    schemas_by_hash: dict[str, dict[str, Any]],
) -> None:
    if codec_ref.get('id') != CALL_DB_CODEC_ID:
        raise CTwoCodegenError(f'unsupported fastdb call-db codec {codec_ref.get("id")!r}.')
    if codec_ref.get('schema') != CALL_DB_SCHEMA_VERSION:
        raise CTwoCodegenError(
            f'fastdb call-db codec must reference schema {CALL_DB_SCHEMA_VERSION}.',
        )
    if call_schema.get('schema') != CALL_DB_SCHEMA_VERSION:
        raise CTwoCodegenError(
            f'Expected {CALL_DB_SCHEMA_VERSION}, got {call_schema.get("schema")!r}.',
        )
    if call_schema.get('profile') not in {
        CALL_DB_COLUMNAR_PROFILE,
        CALL_DB_OBJECT_GRAPH_PROFILE,
    }:
        raise CTwoCodegenError(
            f'unsupported fastdb call-db profile {call_schema.get("profile")!r}.',
        )
    _validate_call_db_crm_context(call_schema)
    if codec_ref.get('schema_sha256') != schema_sha256(call_schema):
        raise CTwoCodegenError(
            f'fastdb call-db schema hash mismatch for method {call_schema.get("method")!r}.',
        )
    tables = call_schema.get('tables')
    if not isinstance(tables, list):
        raise CTwoCodegenError('fastdb call-db schema tables must be a list.')
    object_graph_layer_names: dict[str, tuple[str, str]] = {}
    table_names: set[str] = set()
    value_positions: list[tuple[int, str]] = []
    def claim_object_graph_layer(layer_name: str, owner: str, kind: str) -> None:
        if call_schema.get('profile') != CALL_DB_OBJECT_GRAPH_PROFILE:
            return
        previous = object_graph_layer_names.get(layer_name)
        if previous is not None:
            previous_owner, previous_kind = previous
            if previous_kind == 'dependency' and kind == 'dependency' and previous_owner == owner:
                return
            raise CTwoCodegenError(
                f'fastdb object-graph call-db cannot encode both {previous_owner!r} and {owner!r} as layer {layer_name!r}.',
            )
        object_graph_layer_names[layer_name] = (owner, kind)

    for table in tables:
        if not isinstance(table, dict):
            raise CTwoCodegenError('fastdb call-db table entries must be objects.')
        kind = table.get('kind')
        table_name = _validate_call_db_table_name(table)
        if table_name in table_names:
            raise CTwoCodegenError(f'fastdb call-db duplicate table name {table_name!r}.')
        table_names.add(table_name)
        cardinality = table.get('cardinality')
        if kind == 'scalars' and cardinality != 'one':
            raise CTwoCodegenError(f'fastdb call-db scalar table {table_name!r} must have cardinality "one".')
        if kind == 'array' and cardinality != 'many':
            raise CTwoCodegenError(f'fastdb call-db array table {table_name!r} must have cardinality "many".')
        if kind == 'feature' and cardinality not in {'one', 'many'}:
            raise CTwoCodegenError(f'fastdb call-db feature table {table_name!r} must have cardinality "one" or "many".')
        if kind == 'feature':
            digest = table.get('feature_schema_sha256')
            if not isinstance(digest, str) or digest not in schemas_by_hash:
                raise CTwoCodegenError(
                    f'No fastdb.schema.v1 descriptor supplied for call-db feature schema hash {digest}.',
                )
            value_positions.append((
                _validate_call_db_value_position(
                    table.get('value_position'),
                    f'fastdb call-db feature table {table_name!r}',
                ),
                f'feature table {table_name!r}',
            ))
            feature_schema = schemas_by_hash[digest]
            feature_name = feature_schema.get('feature', {}).get('name')
            if not isinstance(feature_name, str) or not feature_name:
                raise CTwoCodegenError(f'fastdb call-db feature table {table_name!r} references a schema without a feature name.')
            if table.get('feature') != feature_schema.get('feature'):
                raise CTwoCodegenError(
                    f'fastdb call-db feature table {table_name!r} feature metadata does not match referenced schema.',
                )
            claim_object_graph_layer(feature_name, f'feature table {table_name}', 'feature-table')
            if call_schema.get('profile') == CALL_DB_COLUMNAR_PROFILE:
                _validate_typescript_columnar_runtime_schema(
                    feature_schema,
                    context=f'fastdb call-db feature table {table_name!r}',
                )
            else:
                _validate_typescript_object_graph_runtime_schema(
                    feature_schema,
                    context=f'fastdb object-graph call-db feature table {table_name!r}',
                )
            dependencies = table.get('feature_schema_dependencies', [])
            if not isinstance(dependencies, list):
                raise CTwoCodegenError('fastdb call-db feature dependencies must be a list.')
            if dependencies and call_schema.get('profile') != CALL_DB_OBJECT_GRAPH_PROFILE:
                raise CTwoCodegenError(
                    f'fastdb call-db feature table {table_name!r} dependencies are only valid for object-graph call-db.',
                )
            for dependency in dependencies:
                if not isinstance(dependency, dict):
                    raise CTwoCodegenError('fastdb call-db feature dependency entries must be objects.')
                dep_digest = dependency.get('feature_schema_sha256')
                if not isinstance(dep_digest, str) or dep_digest not in schemas_by_hash:
                    raise CTwoCodegenError(
                        f'No fastdb.schema.v1 descriptor supplied for call-db dependency schema hash {dep_digest}.',
                )
                if call_schema.get('profile') == CALL_DB_OBJECT_GRAPH_PROFILE:
                    dependency_schema = schemas_by_hash[dep_digest]
                    dependency_name = dependency_schema.get('feature', {}).get('name')
                    if not isinstance(dependency_name, str) or not dependency_name:
                        raise CTwoCodegenError(f'fastdb call-db dependency {dep_digest!r} references a schema without a feature name.')
                    if dependency.get('feature') != dependency_schema.get('feature'):
                        raise CTwoCodegenError(
                            f'fastdb call-db dependency metadata for {dep_digest!r} does not match referenced schema.',
                        )
                    claim_object_graph_layer(dependency_name, f'dependency {dependency_name}', 'dependency')
                    _validate_typescript_object_graph_runtime_schema(
                        dependency_schema,
                        context=f'fastdb object-graph call-db dependency {dep_digest!r}',
                    )
        elif kind == 'array':
            claim_object_graph_layer(table_name, f'table {table_name}', 'table')
            item = table.get('item')
            if not isinstance(item, dict) or not isinstance(item.get('kind'), str):
                raise CTwoCodegenError('fastdb call-db array table must include an item kind.')
            item_name = item.get('name')
            if item_name != CALL_DB_ARRAY_VALUE_FIELD:
                raise CTwoCodegenError(
                    f'fastdb call-db array table {table_name!r} item name must be {CALL_DB_ARRAY_VALUE_FIELD!r}.',
                )
            _validate_call_db_scalar_kind(
                item.get('kind'),
                f'fastdb call-db array table {table_name!r} item',
            )
            value_positions.append((
                _validate_call_db_value_position(
                    table.get('value_position'),
                    f'fastdb call-db array table {table_name!r}',
                ),
                f'array table {table_name!r}',
            ))
        elif kind == 'scalars':
            claim_object_graph_layer(table_name, f'table {table_name}', 'table')
            fields = table.get('fields')
            if not isinstance(fields, list):
                raise CTwoCodegenError('fastdb call-db scalar table must include fields.')
            bytes_fields = [
                field.get('name')
                for field in fields
                if isinstance(field, dict) and field.get('kind') == 'bytes'
            ]
            if call_schema.get('profile') == CALL_DB_OBJECT_GRAPH_PROFILE and len(bytes_fields) > 1:
                raise CTwoCodegenError(
                    f'fastdb object-graph call-db scalar table {table.get("name")!r} cannot generate a TypeScript runtime binding for multiple bytes fields {bytes_fields!r}.',
                )
            scalar_field_names: set[str] = set()
            for field in fields:
                if not isinstance(field, dict):
                    raise CTwoCodegenError('fastdb call-db scalar fields must include value_position.')
                field_name = field.get('name')
                if not isinstance(field_name, str) or not field_name:
                    raise CTwoCodegenError(f'fastdb call-db scalar table {table_name!r} fields must include names.')
                if field_name in scalar_field_names:
                    raise CTwoCodegenError(f'fastdb call-db duplicate scalar field name {field_name!r}.')
                scalar_field_names.add(field_name)
                value_positions.append((
                    _validate_call_db_value_position(
                        field.get('value_position'),
                        f'fastdb call-db scalar field {field_name!r}',
                    ),
                    f'scalar field {field_name!r}',
                ))
                _validate_call_db_scalar_kind(
                    field.get('kind'),
                    f'fastdb call-db scalar field {field_name!r}',
                )
        elif kind != 'scalars':
            raise CTwoCodegenError(f'Unsupported fastdb call-db table kind {kind!r}.')
    _validate_call_db_value_positions(
        value_positions,
        method=call_schema.get('method'),
        direction=call_schema.get('direction'),
    )


def _validate_call_db_crm_context(call_schema: dict[str, Any]) -> None:
    crm = call_schema.get('crm')
    if crm is None:
        return
    if not isinstance(crm, dict):
        raise CTwoCodegenError('fastdb call-db schema crm must be an object when present.')
    for field in ('namespace', 'name', 'version'):
        value = crm.get(field)
        if not isinstance(value, str) or not value:
            raise CTwoCodegenError(
                'fastdb call-db schema must include complete CRM context '
                '(namespace, name, version) when crm is present.',
            )


def _validate_call_db_value_position(position: object, context: str) -> int:
    if type(position) is not int or position < 0:
        raise CTwoCodegenError(f'{context} must include a non-negative integer value_position.')
    return position


def _validate_call_db_value_positions(
    positions: list[tuple[int, str]],
    *,
    method: object,
    direction: object,
) -> None:
    if not positions:
        return
    seen: dict[int, str] = {}
    duplicates: list[str] = []
    for position, owner in positions:
        previous = seen.get(position)
        if previous is not None:
            duplicates.append(f'{position} used by {previous} and {owner}')
            continue
        seen[position] = owner
    if duplicates:
        raise CTwoCodegenError(
            'fastdb call-db duplicate value_position for '
            f'method {method!r} direction {direction!r}: {", ".join(duplicates)}.',
        )
    actual = sorted(seen)
    expected = list(range(len(actual)))
    if actual != expected:
        raise CTwoCodegenError(
            'fastdb call-db value_position values must be contiguous from 0 for '
            f'method {method!r} direction {direction!r}; got {actual!r}.',
        )


def _validate_call_db_contract_binding(
    codec_ref: dict[str, Any],
    call_schema: dict[str, Any],
    call_db_ref_sources: dict[_CodecRequirementKey, list[tuple[str, str]]],
) -> None:
    schema_method = call_schema.get('method')
    schema_direction = call_schema.get('direction')
    if not isinstance(schema_method, str) or not schema_method:
        raise CTwoCodegenError('fastdb call-db schema must include a non-empty method name.')
    if schema_direction not in {'input', 'output'}:
        raise CTwoCodegenError('fastdb call-db schema direction must be "input" or "output".')
    sources = call_db_ref_sources.get(_codec_requirement_key(codec_ref), [])
    if not sources:
        raise CTwoCodegenError('fastdb call-db codec ref is not used by any contract wire direction.')
    mismatches = [
        (method_name, direction)
        for method_name, direction in sources
        if method_name != schema_method or direction != schema_direction
    ]
    if mismatches:
        usage = ', '.join(f'{method_name}/{direction}' for method_name, direction in mismatches)
        raise CTwoCodegenError(
            f'fastdb call-db schema {schema_method!r}/{schema_direction!r} '
            f'does not match contract wire usage {usage}.',
        )


def _validate_call_db_table_name(table: dict[str, Any]) -> str:
    table_name = table.get('name')
    if not isinstance(table_name, str) or not table_name:
        raise CTwoCodegenError('fastdb call-db table entries must include a non-empty name.')
    return table_name


def _validate_call_db_scalar_kind(kind: Any, context: str) -> None:
    if kind == 'list':
        raise CTwoCodegenError(
            f'{context} cannot generate a TypeScript runtime binding for list scalar metadata.',
        )
    if kind == 'ref':
        raise CTwoCodegenError(
            f'{context} cannot generate a TypeScript runtime binding for ref scalar metadata.',
        )
    if kind not in _SCALAR_SCHEMA:
        raise CTwoCodegenError(f'{context} uses unsupported fastdb scalar kind {kind!r}.')

def _validate_typescript_columnar_runtime_schema(
    schema: dict[str, Any],
    *,
    context: str = 'fastdb columnar codec',
) -> None:
    bytes_fields = [
        field.get('name')
        for field in schema.get('fields', [])
        if field.get('kind') == 'bytes'
    ]
    if len(bytes_fields) > 1:
        raise CTwoCodegenError(
            f'{context} cannot generate a TypeScript runtime binding for multiple bytes fields '
            f'{bytes_fields!r}; fastdb columnar bytes use the feature raw payload.',
        )
    for field in schema.get('fields', []):
        kind = field.get('kind')
        if kind == 'list':
            item = field.get('item') or {}
            item_kind = item.get('kind')
            if item_kind in _NATIVE_LIST_ITEM_KINDS:
                continue
            raise CTwoCodegenError(
                f'{context} cannot generate a TypeScript runtime binding for list field '
                f'{field.get("name")!r} with item kind {item_kind!r}.',
            )


def _validate_typescript_object_graph_runtime_schema(
    schema: dict[str, Any],
    *,
    context: str = 'fastdb object-graph codec',
) -> None:
    bytes_fields = [
        field.get('name')
        for field in schema.get('fields', [])
        if field.get('kind') == 'bytes'
    ]
    if len(bytes_fields) > 1:
        raise CTwoCodegenError(
            f'{context} cannot generate a TypeScript runtime binding for multiple bytes fields '
            f'{bytes_fields!r}; fastdb object-graph bytes use the feature raw payload.',
        )
    for field in schema.get('fields', []):
        if field.get('kind') != 'list':
            continue
        item = field.get('item') or {}
        item_kind = item.get('kind')
        if item_kind == 'ref' or item_kind in _NATIVE_LIST_ITEM_KINDS:
            continue
        raise CTwoCodegenError(
            f'{context} cannot generate a TypeScript runtime binding for list field '
            f'{field.get("name")!r} with item kind {item_kind!r}.',
        )


def _feature_schemas_for_call_db(
    call_schema: dict[str, Any],
    schemas_by_hash: dict[str, dict[str, Any]],
) -> list[dict[str, Any]]:
    features: list[dict[str, Any]] = []
    for table in call_schema.get('tables', []):
        if not isinstance(table, dict) or table.get('kind') != 'feature':
            continue
        digest = table.get('feature_schema_sha256')
        if isinstance(digest, str) and digest in schemas_by_hash:
            features.append(schemas_by_hash[digest])
        for dependency in table.get('feature_schema_dependencies', []):
            if not isinstance(dependency, dict):
                continue
            dep_digest = dependency.get('feature_schema_sha256')
            if isinstance(dep_digest, str) and dep_digest in schemas_by_hash:
                features.append(schemas_by_hash[dep_digest])
    return features


def _collect_imports(
    schemas: list[dict[str, Any]],
    schemas_by_identity: dict[str, dict[str, Any]],
) -> set[str]:
    imports = {'Feature', 'defineSchema'}
    for schema in schemas:
        for field in schema.get('fields', []):
            _schema_expr, _type_expr, field_imports = _field_ts(field, schemas_by_identity)
            imports.update(field_imports)
    return imports


def _render_schema_class(
    schema: dict[str, Any],
    schemas_by_identity: dict[str, dict[str, Any]],
) -> str:
    class_name = _identifier(schema['feature']['name'])
    body = [f'export class {class_name} extends Feature {{', '  static schema = defineSchema({']
    declares: list[str] = []
    field_names: dict[str, str] = {}
    for field in schema.get('fields', []):
        schema_expr, type_expr, _imports = _field_ts(field, schemas_by_identity)
        field_name = _identifier(field['name'])
        previous = field_names.setdefault(field_name, field['name'])
        if previous != field['name']:
            raise CTwoCodegenError(
                f'field name collision after TypeScript identifier sanitization in {class_name}: '
                f'{previous!r} and {field["name"]!r} both map to {field_name!r}.',
            )
        body.append(f'    {field_name}: {schema_expr},')
        declares.append(f'  declare {field_name}: {type_expr};')
    body.append('  });')
    body.extend(declares)
    body.append('}')
    return '\n'.join(body)


def _field_ts(
    field: dict[str, Any],
    schemas_by_identity: dict[str, dict[str, Any]],
) -> tuple[str, str, set[str]]:
    kind = field.get('kind')
    if kind in _SCALAR_SCHEMA:
        symbol, ts_type = _SCALAR_SCHEMA[kind]
        return symbol, ts_type, {symbol}
    if kind == 'ref':
        target = _target_class(field.get('target'), schemas_by_identity)
        return f'ref(() => {target})', f'{target} | null', {'ref'}
    if kind == 'list':
        item = field.get('item') or {}
        item_schema, item_type, imports = _list_item_ts(item, schemas_by_identity)
        imports.add('listOf')
        return f'listOf({item_schema})', f'{item_type}[]', imports
    raise CTwoCodegenError(f'Unsupported fastdb schema field kind {kind!r}.')


def _list_item_ts(
    item: dict[str, Any],
    schemas_by_identity: dict[str, dict[str, Any]],
) -> tuple[str, str, set[str]]:
    kind = item.get('kind')
    if kind in _SCALAR_SCHEMA:
        symbol, ts_type = _SCALAR_SCHEMA[kind]
        return symbol, ts_type, {symbol}
    if kind == 'ref':
        target = _target_class(item.get('target'), schemas_by_identity)
        return f'() => {target}', target, set()
    raise CTwoCodegenError(f'Unsupported fastdb schema list item kind {kind!r}.')


def _target_class(
    target: dict[str, Any] | None,
    schemas_by_identity: dict[str, dict[str, Any]],
) -> str:
    schema = _ref_target_schema(target, schemas_by_identity, context='field')
    return _identifier(schema['feature']['name'])


def _ref_target_schema(
    target: dict[str, Any] | None,
    schemas_by_identity: dict[str, dict[str, Any]],
    *,
    context: str,
) -> dict[str, Any]:
    if not isinstance(target, dict):
        raise CTwoCodegenError(f'fastdb ref target for {context} must be an object.')
    identity = target.get('identity')
    if not isinstance(identity, str) or not identity:
        raise CTwoCodegenError(f'fastdb ref target for {context} must include a non-empty identity.')
    name = target.get('name')
    if not isinstance(name, str) or not name:
        raise CTwoCodegenError(f'fastdb ref target for {context} must include a non-empty name.')
    schema = schemas_by_identity.get(identity)
    if schema is None:
        raise CTwoCodegenError(f'Missing fastdb schema descriptor for ref target {identity}.')
    feature = schema.get('feature')
    if not isinstance(feature, dict):
        raise CTwoCodegenError(f'fastdb ref target {identity!r} references a schema without feature metadata.')
    if feature.get('identity') != identity or feature.get('name') != name:
        raise CTwoCodegenError(
            f'fastdb ref target metadata for {context} does not match referenced schema identity {identity!r}.',
        )
    return schema


def _render_codec_binding(
    binding_name: str,
    codec_ref: dict[str, Any],
    schema: dict[str, Any],
) -> str:
    class_name = _identifier(schema['feature']['name'])
    codec_id = codec_ref['id']
    profile = FASTDB_CODEC_IDS[codec_id]
    digest = codec_ref['schema_sha256']
    media_type = _codec_optional_string(codec_ref, 'media_type')
    return '\n'.join([
        f'export const {binding_name}: FastdbC2CodecBinding<{class_name}> = {{',
        f'  codecId: {_ts_string(codec_id)},',
        f'  version: {_ts_string(str(codec_ref.get("version", "")))},',
        f'  schema: {_ts_string(codec_ref.get("schema", SCHEMA_VERSION))},',
        f'  profile: {_ts_string(profile)},',
        f'  schemaSha256: {_ts_string(digest)},',
        *(
            [f'  mediaType: {_ts_string(media_type)},']
            if media_type is not None
            else []
        ),
        f'  capabilities: [{_render_ts_string_array(codec_ref.get("capabilities", []))}],',
        f'  feature: {class_name},',
        f'  encode(value: {class_name}): Uint8Array {{',
        '    return encodeFastdbFeature(this, value);',
        '  },',
        f'  decode(payload: FastdbDatabaseBytes): {class_name} {{',
        '    return decodeFastdbFeature(this, payload);',
        '  },',
        '};',
    ])


def _render_call_db_binding(
    binding_name: str,
    codec_ref: dict[str, Any],
    call_schema: dict[str, Any],
    schemas_by_hash: dict[str, dict[str, Any]],
) -> str:
    digest = codec_ref['schema_sha256']
    media_type = _codec_optional_string(codec_ref, 'media_type')
    lines = [
        f'export const {binding_name}: FastdbC2CallDbBinding = {{',
        f'  codecId: {_ts_string(CALL_DB_CODEC_ID)},',
        f'  version: {_ts_string(str(codec_ref.get("version", "")))},',
        f'  schema: {_ts_string(codec_ref.get("schema", CALL_DB_SCHEMA_VERSION))},',
        f'  profile: {_ts_string(call_schema["profile"])},',
        f'  schemaSha256: {_ts_string(digest)},',
        *(
            [f'  mediaType: {_ts_string(media_type)},']
            if media_type is not None
            else []
        ),
        f'  capabilities: [{_render_ts_string_array(codec_ref.get("capabilities", []))}],',
        f'  method: {_ts_string(call_schema["method"])},',
        f'  direction: {_ts_string(call_schema["direction"])},',
        '  tables: [',
    ]
    for table in call_schema.get('tables', []):
        lines.extend(_render_call_db_table(table, schemas_by_hash))
    lines.extend([
        '  ],',
        '  encode(value: unknown): Uint8Array {',
        '    return encodeFastdbCallDb(this, value);',
        '  },',
        '  decode(payload: FastdbDatabaseBytes): unknown {',
        '    return decodeFastdbCallDb(this, payload);',
        '  },',
        '  view(payload: FastdbDatabaseBytes): FastdbC2CallDbView {',
        '    return viewFastdbCallDb(this, payload);',
        '  },',
        '  scalar(payload: FastdbDatabaseBytes, nameOrIndex: string | number): unknown {',
        '    const view = viewFastdbCallDb(this, payload);',
        '    try {',
        '      return view.scalar(nameOrIndex);',
        '    } finally {',
        '      view.close();',
        '    }',
        '  },',
        '};',
    ])
    return '\n'.join(lines)


def _render_typed_client_facade(
    contract: dict[str, Any],
    matched_calls: list[tuple[dict[str, Any], dict[str, Any]]],
    schemas_by_hash: dict[str, dict[str, Any]],
) -> str:
    if not matched_calls:
        return ''
    calls_by_method_direction: dict[tuple[str, str], tuple[dict[str, Any], dict[str, Any]]] = {}
    for codec_ref, call_schema in matched_calls:
        calls_by_method_direction[(call_schema['method'], call_schema['direction'])] = (codec_ref, call_schema)

    crm = contract['crm']
    client_prefix = _identifier(str(crm['name']))
    raw_client_name = f'{client_prefix}RawClient'
    typed_client_name = f'{client_prefix}TypedClient'
    factory_name = f'create{client_prefix}TypedClient'
    raw_methods: list[str] = []
    typed_methods: list[str] = []
    factory_methods: list[str] = []
    used_method_names: dict[str, str] = {}
    for method in contract.get('methods', []):
        method_name = method.get('name')
        if not isinstance(method_name, str) or not method_name:
            continue
        input_entry = calls_by_method_direction.get((method_name, 'input'))
        output_entry = calls_by_method_direction.get((method_name, 'output'))
        input_schema = input_entry[1] if input_entry is not None else None
        output_schema = output_entry[1] if output_entry is not None else None
        if input_entry is None and output_entry is None:
            continue
        method_ident = _identifier(method_name)
        previous = used_method_names.setdefault(method_ident, method_name)
        if previous != method_name:
            raise CTwoCodegenError(
                'method name collision after TypeScript identifier sanitization in typed client facade: '
                f'{previous!r} and {method_name!r} both map to {method_ident!r}.',
            )
        hold_method_ident = _identifier(f'hold_{method_ident}')
        previous = used_method_names.setdefault(hold_method_ident, method_name)
        if previous != method_name:
            raise CTwoCodegenError(
                'held method name collision after TypeScript identifier sanitization in typed client facade: '
                f'{previous!r} already maps to {hold_method_ident!r} required by {method_name!r}.',
            )
        params = _typed_client_params(method, input_schema, schemas_by_hash)
        raw_signature = ', '.join(f'{name}: unknown' for name, _type_expr in params)
        typed_signature = ', '.join(f'{name}: {type_expr}' for name, type_expr in params)
        arg_list = ', '.join(name for name, _type_expr in params)
        return_type = _typed_client_return_type(output_schema, schemas_by_hash)
        held_return_type = _typed_client_held_return_type(output_entry, return_type)
        raw_methods.append(f'  {method_ident}({raw_signature}): Promise<unknown>;')
        raw_methods.append(
            f'  {hold_method_ident}({raw_signature}): Promise<FastdbC2HeldResult<unknown, FastdbC2TransportResponsePayload>>;',
        )
        typed_methods.append(f'  {method_ident}({typed_signature}): Promise<{return_type}>;')
        typed_methods.append(
            f'  {hold_method_ident}({typed_signature}): Promise<FastdbC2HeldResult<{held_return_type}>>;',
        )
        factory_methods.extend([
            f'    async {method_ident}({typed_signature}): Promise<{return_type}> {{',
            f'      return (await client.{method_ident}({arg_list})) as {return_type};',
            '    },',
            f'    async {hold_method_ident}({typed_signature}): Promise<FastdbC2HeldResult<{held_return_type}>> {{',
            f'      return (await client.{hold_method_ident}({arg_list})) as FastdbC2HeldResult<{held_return_type}>;',
            '    },',
        ])
    if not typed_methods:
        return ''
    return '\n'.join([
        'export type FastdbC2InputSequence<T> = readonly T[] | (Iterable<T> & object) | (ArrayLike<T> & object);',
        '',
        'export interface FastdbC2HeldResult<T, TBuffer = FastdbC2TransportResponsePayload> {',
        '  readonly value: T;',
        '  readonly buffer: TBuffer;',
        '  release(): void;',
        '}',
        '',
        f'export interface {raw_client_name} {{',
        *raw_methods,
        '}',
        '',
        f'export interface {typed_client_name} {{',
        *typed_methods,
        '}',
        '',
        f'export function {factory_name}(client: {raw_client_name}): {typed_client_name} {{',
        '  return {',
        *factory_methods,
        '  };',
        '}',
    ])


def _typed_client_params(
    method: dict[str, Any],
    input_schema: dict[str, Any] | None,
    schemas_by_hash: dict[str, dict[str, Any]],
) -> list[tuple[str, str]]:
    if input_schema is None:
        params = method.get('parameters', [])
        if not isinstance(params, list):
            return []
        return _dedupe_typed_names([
            (str(param.get('name', 'arg')), 'unknown')
            for param in params
            if isinstance(param, dict)
        ])
    return _dedupe_typed_names([
        (name, type_expr)
        for _position, name, type_expr in _call_db_value_types(input_schema, schemas_by_hash, direction='input')
    ])


def _typed_client_return_type(
    output_schema: dict[str, Any] | None,
    schemas_by_hash: dict[str, dict[str, Any]],
) -> str:
    if output_schema is None:
        return 'void'
    values = _call_db_value_types(output_schema, schemas_by_hash, direction='output')
    if not values:
        return 'void'
    types = [type_expr for _position, _name, type_expr in values]
    if len(types) == 1:
        return types[0]
    return f'[{", ".join(types)}]'


def _typed_client_held_return_type(
    output_entry: tuple[dict[str, Any], dict[str, Any]] | None,
    materialized_return_type: str,
) -> str:
    if output_entry is None:
        return 'void'
    codec_ref, _call_schema = output_entry
    if _codec_ref_has_buffer_view(codec_ref):
        return 'FastdbC2CallDbView'
    return materialized_return_type


def _codec_ref_has_buffer_view(codec_ref: dict[str, Any]) -> bool:
    capabilities = codec_ref.get('capabilities', [])
    return isinstance(capabilities, list) and 'buffer-view' in capabilities


def _call_db_value_types(
    call_schema: dict[str, Any],
    schemas_by_hash: dict[str, dict[str, Any]],
    *,
    direction: str,
) -> list[tuple[int, str, str]]:
    values: list[tuple[int, str, str]] = []
    for table in call_schema.get('tables', []):
        if table.get('kind') == 'scalars':
            for field in table.get('fields', []):
                values.append((
                    int(field['value_position']),
                    str(field.get('parameter') or field['name']),
                    _scalar_ts_type(str(field['kind'])),
                ))
        elif table.get('kind') == 'array':
            item = table['item']
            item_type = _scalar_ts_type(str(item['kind']))
            type_expr = f'FastdbC2InputSequence<{item_type}>' if direction == 'input' else f'{item_type}[]'
            values.append((
                int(table['value_position']),
                str(table.get('parameter') or table['name']),
                type_expr,
            ))
        elif table.get('kind') == 'feature':
            digest = table['feature_schema_sha256']
            schema = schemas_by_hash[digest]
            class_name = _identifier(schema['feature']['name'])
            if table.get('cardinality') == 'many':
                type_expr = f'FastdbC2InputSequence<{class_name}>' if direction == 'input' else f'{class_name}[]'
            else:
                type_expr = class_name
            values.append((
                int(table['value_position']),
                str(table.get('parameter') or table['name']),
                type_expr,
            ))
    return sorted(values, key=lambda item: item[0])


def _scalar_ts_type(kind: str) -> str:
    try:
        return _SCALAR_SCHEMA[kind][1]
    except KeyError as exc:
        raise CTwoCodegenError(f'Unsupported fastdb scalar kind {kind!r}.') from exc


def _dedupe_typed_names(values: list[tuple[str, str]]) -> list[tuple[str, str]]:
    names: dict[str, str] = {}
    result: list[tuple[str, str]] = []
    for original, type_expr in values:
        name = _identifier(original)
        previous = names.setdefault(name, original)
        if previous != original:
            raise CTwoCodegenError(
                'typed client parameter name collision after TypeScript identifier sanitization: '
                f'{previous!r} and {original!r} both map to {name!r}.',
            )
        result.append((name, type_expr))
    return result


def _render_call_db_table(
    table: dict[str, Any],
    schemas_by_hash: dict[str, dict[str, Any]],
) -> list[str]:
    common = [
        f'      name: {_ts_string(table["name"])},',
        f'      kind: {_ts_string(table["kind"])},',
        f'      cardinality: {_ts_string(table["cardinality"])},',
    ]
    lines = ['    {', *common]
    if table.get('kind') == 'scalars':
        lines.append('      fields: [')
        for field in table.get('fields', []):
            field_parts = [
                f'name: {_ts_string(field["name"])}',
                f'kind: {_ts_string(field["kind"])}',
            ]
            if 'parameter' in field:
                field_parts.append(f'parameter: {_ts_string(field["parameter"])}')
            if 'value_position' in field:
                field_parts.append(f'valuePosition: {int(field["value_position"])}')
            lines.append(f'        {{ {", ".join(field_parts)} }},')
        lines.append('      ],')
    elif table.get('kind') == 'array':
        item = table['item']
        lines.append(
            f'      item: {{ name: {_ts_string(item["name"])}, kind: {_ts_string(item["kind"])} }},',
        )
        if 'parameter' in table:
            lines.append(f'      parameter: {_ts_string(table["parameter"])},')
        if 'return_index' in table:
            lines.append(f'      returnIndex: {int(table["return_index"])},')
        if 'value_position' in table:
            lines.append(f'      valuePosition: {int(table["value_position"])},')
    elif table.get('kind') == 'feature':
        digest = table['feature_schema_sha256']
        schema = schemas_by_hash[digest]
        lines.append(f'      feature: {_identifier(schema["feature"]["name"])},')
        lines.append(f'      featureSchemaSha256: {_ts_string(digest)},')
        dependencies = table.get('feature_schema_dependencies', [])
        if dependencies:
            lines.append('      featureDependencies: [')
            for dependency in dependencies:
                dep_digest = dependency['feature_schema_sha256']
                dep_schema = schemas_by_hash[dep_digest]
                lines.append(
                    f'        {{ feature: {_identifier(dep_schema["feature"]["name"])}, '
                    f'featureSchemaSha256: {_ts_string(dep_digest)} }},',
                )
            lines.append('      ],')
        if 'parameter' in table:
            lines.append(f'      parameter: {_ts_string(table["parameter"])},')
        if 'return_index' in table:
            lines.append(f'      returnIndex: {int(table["return_index"])},')
        if 'value_position' in table:
            lines.append(f'      valuePosition: {int(table["value_position"])},')
    else:
        raise CTwoCodegenError(f'Unsupported fastdb call-db table kind {table.get("kind")!r}.')
    lines.append('    },')
    return lines


def _binding_name(schema: dict[str, Any], codec_ref: dict[str, Any]) -> str:
    profile = FASTDB_CODEC_IDS[codec_ref['id']]
    return f'{_const_identifier(schema["feature"]["name"])}_{_const_identifier(profile.replace(".v1", ""))}_CODEC'


def _call_db_binding_name(call_schema: dict[str, Any]) -> str:
    return (
        f'{_const_identifier(call_schema["method"])}_'
        f'{_const_identifier(call_schema["direction"])}_CALL_DB_CODEC'
    )


def _identifier(value: str) -> str:
    result = ''.join(ch if ch == '_' or ch.isalnum() else '_' for ch in value)
    if not result:
        return '_'
    if result[0].isdigit():
        result = f'_{result}'
    if result in _TS_RESERVED_WORDS:
        result = f'{result}_'
    return result


def _const_identifier(value: str) -> str:
    result: list[str] = []
    previous_lower_or_digit = False
    for ch in value:
        if ch.isalnum():
            if ch.isupper() and previous_lower_or_digit and result and result[-1] != '_':
                result.append('_')
            result.append(ch.upper())
            previous_lower_or_digit = ch.islower() or ch.isdigit()
        elif result and result[-1] != '_':
            result.append('_')
            previous_lower_or_digit = False
    return ''.join(result).strip('_') or 'FASTDB'


def _ts_string(value: str) -> str:
    return json.dumps(value)


def _render_ts_string_array(values: object) -> str:
    if not isinstance(values, list):
        return ''
    return ', '.join(_ts_string(str(value)) for value in values)


_TS_RESERVED_WORDS = {
    'abstract',
    'any',
    'as',
    'asserts',
    'async',
    'await',
    'bigint',
    'boolean',
    'break',
    'case',
    'catch',
    'class',
    'const',
    'constructor',
    'continue',
    'debugger',
    'declare',
    'default',
    'delete',
    'do',
    'else',
    'enum',
    'export',
    'extends',
    'false',
    'finally',
    'for',
    'from',
    'function',
    'get',
    'global',
    'if',
    'implements',
    'import',
    'in',
    'infer',
    'instanceof',
    'interface',
    'is',
    'keyof',
    'let',
    'module',
    'namespace',
    'never',
    'new',
    'null',
    'number',
    'object',
    'of',
    'package',
    'private',
    'protected',
    'public',
    'readonly',
    'require',
    'return',
    'satisfies',
    'set',
    'static',
    'string',
    'super',
    'switch',
    'symbol',
    'this',
    'throw',
    'true',
    'try',
    'type',
    'typeof',
    'undefined',
    'unique',
    'unknown',
    'var',
    'void',
    'while',
    'with',
    'yield',
}


def main(argv: list[str] | None = None) -> int:
    parser = argparse.ArgumentParser(
        prog='python -m c_two.fastdb.typescript',
        description='Generate C-Two FastDB TypeScript codec helpers from a C-Two contract and FastDB artifacts.',
    )
    parser.add_argument('contract_path', help='C-Two portable contract descriptor JSON path')
    parser.add_argument('output_path', help='TypeScript helper output path')
    parser.add_argument(
        '--schema',
        action='append',
        default=[],
        help='FastDB schema descriptor JSON file or artifact bundle; repeatable',
    )
    args = parser.parse_args(argv)
    run_codegen_c_two_ts(args.contract_path, args.output_path, args.schema)
    return 0


if __name__ == '__main__':
    raise SystemExit(main())
