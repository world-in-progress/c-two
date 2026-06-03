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


def test_top_level_custom_transferable_api_is_removed():
    assert not hasattr(cc, 'transferable')
    assert not hasattr(cc, 'transfer')


def test_default_transferables_use_pinned_pickle_protocol(monkeypatch):
    payload_plan_mod = importlib.import_module('c_two.crm.payload_plan')
    from c_two.crm.payload_plan import (
        DEFAULT_PICKLE_PROTOCOL,
        python_pickle_input_binding,
        python_pickle_output_binding,
    )

    protocols: list[int | None] = []
    real_dumps = pickle.dumps

    def record_dumps(value: object, *args: object, **kwargs: object) -> bytes:
        protocols.append(kwargs.get('protocol'))
        return real_dumps(value, *args, **kwargs)

    monkeypatch.setattr(payload_plan_mod.pickle, 'dumps', record_dumps)

    def echo(value: int) -> int:
        return value

    input_binding = python_pickle_input_binding(echo)
    output_binding = python_pickle_output_binding(echo)

    assert input_binding.serialize(1) == real_dumps(1, protocol=DEFAULT_PICKLE_PROTOCOL)
    assert output_binding.serialize(2) == real_dumps(2, protocol=DEFAULT_PICKLE_PROTOCOL)
    assert protocols == [DEFAULT_PICKLE_PROTOCOL, DEFAULT_PICKLE_PROTOCOL]


def test_transfer_input_output_codec_overrides_are_removed_from_crm_planning():
    from c_two.crm.transferable import transfer

    class StablePayload:
        value: int

        def serialize(value: 'StablePayload') -> bytes:
            return str(value.value).encode()

        def deserialize(data: bytes) -> 'StablePayload':
            return StablePayload(int(data))

    with pytest.raises(TypeError, match='codec overrides were removed'):
        @cc.crm(namespace='test.descriptor', version='0.1.0')
        class StableContract:
            @transfer(input=StablePayload, output=StablePayload)
            def echo(self, value: StablePayload) -> StablePayload:
                ...


def test_transfer_buffer_hold_is_not_server_borrowing_api():
    from c_two.crm.transferable import transfer

    with pytest.raises(ValueError, match='input_lifetime'):
        transfer(buffer='hold')


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


def test_plain_python_annotations_are_python_pickle_fallback_not_payload_plan():
    class PlainPayload:
        value: int

    @cc.crm(namespace='test.descriptor.python-only-transferable', version='0.1.0')
    class PythonOnlyContract:
        def echo(self, value: PlainPayload) -> PlainPayload:
            ...

    from c_two.crm.descriptor import build_contract_descriptor

    descriptor = build_contract_descriptor(PythonOnlyContract)
    method = descriptor['methods'][0]

    assert method['wire']['input']['family'] == 'python-pickle-default'
    assert method['wire']['output']['family'] == 'python-pickle-default'
    assert method['parameters'][0]['annotation'] == {
        'kind': 'python_type',
        'module': __name__,
        'name': 'PlainPayload',
    }
    with pytest.raises(ValueError, match='python-pickle-default'):
        cc.export_contract_descriptor(PythonOnlyContract)


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


def test_runtime_and_examples_do_not_reference_removed_transferable_api():
    repo = Path(__file__).resolve().parents[4]
    roots = [
        repo / 'sdk/python/src',
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
            transferables: dict[str, int] = {}
            for node in ast.walk(tree):
                if not isinstance(node, ast.ClassDef):
                    continue
                decorators = {
                    decorator_call_name(decorator): decorator
                    for decorator in node.decorator_list
                }
                removed_decorator = decorators.get('cc.' + 'transferable')
                if removed_decorator is None:
                    continue
                transferables[node.name] = node.lineno

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
                        line = transferables[name]
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
