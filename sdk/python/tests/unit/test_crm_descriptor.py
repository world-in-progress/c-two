import pickle
import importlib
import ast
import re
from pathlib import Path
from typing import Any

import pytest

import c_two as cc


def _contract_hashes(crm_class: type) -> tuple[str, str]:
    from c_two.crm.descriptor import build_contract_fingerprints

    return build_contract_fingerprints(crm_class)


def test_transferable_decorator_records_abi_declarations_and_rejects_invalid_forms():
    @cc.transferable(abi_id='test.payload.v1')
    class PayloadById:
        value: int

        def serialize(value: 'PayloadById') -> bytes:
            return str(value.value).encode()

        def deserialize(data: bytes) -> 'PayloadById':
            return PayloadById(int(data))

    @cc.transferable(abi_schema='{"kind":"payload","version":1}')
    class PayloadBySchema:
        value: int

        def serialize(value: 'PayloadBySchema') -> bytes:
            return str(value.value).encode()

        def deserialize(data: bytes) -> 'PayloadBySchema':
            return PayloadBySchema(int(data))

    assert PayloadById.__cc_transferable_abi__ == {
        'abi_id': 'test.payload.v1',
        'abi_schema': None,
    }
    assert PayloadBySchema.__cc_transferable_abi__ == {
        'abi_id': None,
        'abi_schema': '{"kind":"payload","version":1}',
    }

    with pytest.raises(ValueError, match='either abi_id or abi_schema'):
        cc.transferable(abi_id='one', abi_schema='two')(PayloadById)
    with pytest.raises(ValueError, match='abi_id cannot be empty'):
        cc.transferable(abi_id='  ')(PayloadById)
    with pytest.raises(TypeError, match='abi_schema must be a string'):
        cc.transferable(abi_schema=object())(PayloadById)  # type: ignore[arg-type]


def test_default_transferables_use_pinned_pickle_protocol(monkeypatch):
    transferable_mod = importlib.import_module('c_two.crm.transferable')
    from c_two.crm.transferable import DEFAULT_PICKLE_PROTOCOL, create_default_transferable

    protocols: list[int | None] = []
    real_dumps = pickle.dumps

    def record_dumps(value: object, *args: object, **kwargs: object) -> bytes:
        protocols.append(kwargs.get('protocol'))
        return real_dumps(value, *args, **kwargs)

    monkeypatch.setattr(transferable_mod.pickle, 'dumps', record_dumps)

    def echo(value: int) -> int:
        return value

    input_transferable = create_default_transferable(echo, is_input=True)
    output_transferable = create_default_transferable(echo, is_input=False)

    assert input_transferable.serialize(1) == real_dumps(1, protocol=DEFAULT_PICKLE_PROTOCOL)
    assert output_transferable.serialize(2) == real_dumps(2, protocol=DEFAULT_PICKLE_PROTOCOL)
    assert protocols == [DEFAULT_PICKLE_PROTOCOL, DEFAULT_PICKLE_PROTOCOL]


def test_descriptor_uses_explicit_custom_transferable_abi_and_not_python_names():
    @cc.transferable(abi_id='test.payload.stable.v1')
    class StablePayload:
        value: int

        def serialize(value: 'StablePayload') -> bytes:
            return str(value.value).encode()

        def deserialize(data: bytes) -> 'StablePayload':
            return StablePayload(int(data))

    @cc.crm(namespace='test.descriptor', version='0.1.0')
    class StableContract:
        @cc.transfer(input=StablePayload, output=StablePayload)
        def echo(self, value: StablePayload) -> StablePayload:
            ...

    from c_two.crm.descriptor import build_contract_descriptor

    descriptor = build_contract_descriptor(StableContract)
    encoded = repr(descriptor)

    assert 'test.payload.stable.v1' in encoded
    assert f'{StablePayload.__module__}.{StablePayload.__qualname__}' not in encoded
    assert _contract_hashes(StableContract)[0] != _contract_hashes(StableContract)[1]


def test_descriptor_hash_changes_when_transferable_abi_schema_changes():
    @cc.transferable(abi_schema='{"kind":"payload","version":1}')
    class SchemaV1:
        value: int

        def serialize(value: 'SchemaV1') -> bytes:
            return str(value.value).encode()

        def deserialize(data: bytes) -> 'SchemaV1':
            return SchemaV1(int(data))

    @cc.transferable(abi_schema='{"kind":"payload","version":2}')
    class SchemaV2:
        value: int

        def serialize(value: 'SchemaV2') -> bytes:
            return str(value.value).encode()

        def deserialize(data: bytes) -> 'SchemaV2':
            return SchemaV2(int(data))

    @cc.crm(namespace='test.descriptor', version='0.1.0')
    class ContractV1:
        @cc.transfer(input=SchemaV1, output=SchemaV1)
        def echo(self, value: SchemaV1) -> SchemaV1:
            ...

    @cc.crm(namespace='test.descriptor', version='0.1.0')
    class ContractV2:
        @cc.transfer(input=SchemaV2, output=SchemaV2)
        def echo(self, value: SchemaV2) -> SchemaV2:
            ...

    abi_v1, sig_v1 = _contract_hashes(ContractV1)
    abi_v2, sig_v2 = _contract_hashes(ContractV2)

    assert abi_v1 != abi_v2
    assert sig_v1 != sig_v2


def test_descriptor_normalizes_optional_union_and_ignores_shutdown_without_return_annotation():
    @cc.crm(namespace='test.descriptor.optional', version='0.1.0')
    class OptionalContract:
        def echo(self, left: str | None, right: int | str) -> str | None:
            ...

        @cc.on_shutdown
        def cleanup(self):
            ...

    from c_two.crm.descriptor import build_contract_descriptor

    descriptor = build_contract_descriptor(OptionalContract)
    assert [method['name'] for method in descriptor['methods']] == ['echo']
    encoded = repr(descriptor)
    assert "'kind': 'union'" in encoded
    assert 'cleanup' not in encoded


def test_non_portable_descriptor_allows_pickle_fallback_for_plain_python_payloads():
    class PlainPayload:
        value: int

    @cc.crm(namespace='test.descriptor.pickle-fallback', version='0.1.0')
    class PickleFallback:
        def list_payloads(self) -> list[PlainPayload]:
            ...

    from c_two.crm.descriptor import build_contract_descriptor

    descriptor = build_contract_descriptor(PickleFallback)
    method = descriptor['methods'][0]

    assert method['wire']['output']['family'] == 'python-pickle-default'
    assert method['return'] == {
        'item': {
            'kind': 'python_type',
            'module': __name__,
            'name': 'PlainPayload',
        },
        'kind': 'list',
    }
    assert _contract_hashes(PickleFallback)
    with pytest.raises(ValueError, match='python-pickle-default'):
        cc.export_contract_descriptor(PickleFallback)


@pytest.mark.parametrize(
    ('method_source', 'error_pattern'),
    [
        ('def run(self, value): ...', 'annotation'),
        ('def run(self, value: Any) -> int: ...', 'Any'),
        ('def run(self, value: list) -> list[int]: ...', 'bare container'),
        ('def run(self, *values: int) -> int: ...', 'varargs'),
        ('def run(self, value: int = object()) -> int: ...', 'default'),
    ],
)
def test_descriptor_rejects_non_canonical_remote_signatures(
    method_source: str,
    error_pattern: str,
):
    namespace: dict[str, object] = {'Any': Any}
    exec(method_source, namespace)
    method = namespace['run']

    Broken = cc.crm(namespace='test.descriptor.reject', version='0.1.0')(
        type('Broken', (), {'__module__': __name__, 'run': method}),
    )

    with pytest.raises((TypeError, ValueError), match=error_pattern):
        _contract_hashes(Broken)


def test_descriptor_rejects_custom_hook_transferable_without_declared_abi():
    @cc.transferable
    class UndeclaredPayload:
        value: int

        def serialize(value: 'UndeclaredPayload') -> bytes:
            return str(value.value).encode()

        def deserialize(data: bytes) -> 'UndeclaredPayload':
            return UndeclaredPayload(int(data))

    @cc.crm(namespace='test.descriptor.reject', version='0.1.0')
    class BadContract:
        @cc.transfer(input=UndeclaredPayload, output=UndeclaredPayload)
        def echo(self, value: UndeclaredPayload) -> UndeclaredPayload:
            ...

    with pytest.raises(ValueError, match='explicit ABI'):
        _contract_hashes(BadContract)


def test_descriptor_rejects_plain_no_hook_transferable_until_field_wire_abi_exists():
    @cc.transferable
    class PlainPayload:
        value: int

    @cc.crm(namespace='test.descriptor.reject', version='0.1.0')
    class BadContract:
        def echo(self, value: PlainPayload) -> PlainPayload:
            ...

    with pytest.raises(ValueError, match='dataclass-field ABI'):
        _contract_hashes(BadContract)


def test_remote_surface_transferables_do_not_use_implicit_abi_declarations():
    repo = Path(__file__).resolve().parents[4]
    roots = [
        repo / 'sdk/python/tests/fixtures',
        repo / 'sdk/python/tests/integration',
        repo / 'sdk/python/benchmarks',
        repo / 'examples/python',
    ]
    implicit_decorator = re.compile(r'^\s*@cc\.transferable(?:\(\s*\))?\s*$')
    offenders: list[str] = []

    for root in roots:
        for path in root.rglob('*.py'):
            for lineno, line in enumerate(path.read_text().splitlines(), start=1):
                if implicit_decorator.match(line):
                    offenders.append(f'{path.relative_to(repo)}:{lineno}')

    assert offenders == []


def test_remote_crm_signatures_do_not_reference_no_abi_custom_hooks():
    repo = Path(__file__).resolve().parents[4]
    roots = [
        repo / 'sdk/python/tests',
        repo / 'sdk/python/benchmarks',
        repo / 'examples/python',
    ]
    offenders: list[str] = []

    def decorator_call_name(decorator: ast.expr) -> str | None:
        target = decorator.func if isinstance(decorator, ast.Call) else decorator
        if isinstance(target, ast.Attribute) and isinstance(target.value, ast.Name):
            return f'{target.value.id}.{target.attr}'
        if isinstance(target, ast.Name):
            return target.id
        return None

    def decorator_has_abi(decorator: ast.expr) -> bool:
        return (
            isinstance(decorator, ast.Call)
            and any(kw.arg in {'abi_id', 'abi_schema'} for kw in decorator.keywords)
        )

    def names_in_annotation(annotation: ast.expr | None) -> set[str]:
        names: set[str] = set()
        if annotation is None:
            return names
        for node in ast.walk(annotation):
            if isinstance(node, ast.Name):
                names.add(node.id)
            elif isinstance(node, ast.Constant) and isinstance(node.value, str):
                names.add(node.value)
        return names

    def transfer_keyword_names(decorator: ast.expr) -> set[str]:
        if (
            not isinstance(decorator, ast.Call)
            or decorator_call_name(decorator) not in {'cc.transfer', 'transfer'}
        ):
            return set()
        names: set[str] = set()
        for kw in decorator.keywords:
            if kw.arg in {'input', 'output'} and isinstance(kw.value, ast.Name):
                names.add(kw.value.id)
        return names

    for root in roots:
        for path in root.rglob('*.py'):
            if path.name == 'test_crm_descriptor.py':
                continue
            tree = ast.parse(path.read_text(), filename=str(path))
            transferables: dict[str, tuple[int, bool, bool]] = {}
            for node in ast.walk(tree):
                if not isinstance(node, ast.ClassDef):
                    continue
                decorators = {
                    decorator_call_name(decorator): decorator
                    for decorator in node.decorator_list
                }
                transferable_decorator = decorators.get('cc.transferable')
                if transferable_decorator is None:
                    continue
                has_hooks = any(
                    isinstance(child, ast.FunctionDef)
                    and child.name in {'serialize', 'deserialize', 'from_buffer'}
                    for child in node.body
                )
                transferables[node.name] = (
                    node.lineno,
                    decorator_has_abi(transferable_decorator),
                    has_hooks,
                )

            if not transferables:
                continue

            for node in ast.walk(tree):
                if not isinstance(node, ast.ClassDef):
                    continue
                is_crm = any(
                    decorator_call_name(decorator) == 'cc.crm'
                    for decorator in node.decorator_list
                )
                if not is_crm:
                    continue
                for item in node.body:
                    if not isinstance(item, ast.FunctionDef):
                        continue
                    referenced = names_in_annotation(item.returns)
                    for arg in item.args.args + item.args.kwonlyargs:
                        referenced.update(names_in_annotation(arg.annotation))
                    for decorator in item.decorator_list:
                        referenced.update(transfer_keyword_names(decorator))
                    for name in referenced:
                        if name not in transferables:
                            continue
                        line, has_abi, has_hooks = transferables[name]
                        if has_hooks and not has_abi:
                            offenders.append(
                                f'{path.relative_to(repo)}:{line}: {name}',
                            )

    assert offenders == []


def test_contract_descriptor_hash_material_has_no_python_annotation_repr_fallbacks():
    repo = Path(__file__).resolve().parents[4]
    sources = [
        repo / 'sdk/python/src/c_two/crm/contract.py',
        repo / 'sdk/python/src/c_two/crm/descriptor.py',
    ]
    forbidden = [
        'repr(annotation)',
        'str(annotation)',
        '__qualname__',
    ]
    offenders = [
        f'{path.relative_to(repo)} contains {needle}'
        for path in sources
        for needle in forbidden
        if needle in path.read_text()
    ]

    assert offenders == []
