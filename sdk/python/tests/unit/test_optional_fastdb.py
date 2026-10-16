"""Exercise optional dependencies in fresh interpreters, outside pytest imports."""

import importlib.metadata
import subprocess
import sys
import textwrap
from pathlib import Path

import pytest


SPEC = {
    "schema": "fastdb.payload.v1",
    "profile": "record.v1",
    "entries": [
        {
            "id": "value",
            "cardinality": "one",
            "type": {"kind": "u8", "nullable": False},
        },
    ],
    "components": [],
}


@pytest.fixture
def isolated_source(tmp_path):
    import c_two

    # -S removes every site package, including C-Two's version metadata. Keep
    # only that metadata alongside the actual package/native extension source.
    metadata_root = tmp_path / "metadata"
    metadata_dir = metadata_root / "c_two.dist-info"
    metadata_dir.mkdir(parents=True)
    metadata_dir.joinpath("METADATA").write_text(
        importlib.metadata.distribution("c-two").read_text("METADATA"),
        encoding="utf-8",
    )
    package_root = str(Path(c_two.__file__).resolve().parents[1])
    return textwrap.dedent(f"""
        import sys
        sys.path[:0] = [{package_root!r}, {str(metadata_root)!r}]
        import importlib.abc
        import importlib.util

        if sys.flags.no_site:
            assert importlib.util.find_spec('fastdb4py') is None
            assert importlib.util.find_spec('numpy') is None

        optional_imports = []

        class RejectOptionalImports(importlib.abc.MetaPathFinder):
            def find_spec(self, fullname, path=None, target=None):
                if fullname.split('.')[0] in {{'fastdb4py', 'numpy'}}:
                    optional_imports.append(fullname)
                    raise ModuleNotFoundError(
                        f'optional dependency {{fullname}} is unavailable',
                        name=fullname,
                    )

        sys.meta_path.insert(0, RejectOptionalImports())

        def assert_no_optional_imports():
            assert optional_imports == [], optional_imports
            assert not any(
                name.split('.')[0] in {{'fastdb4py', 'numpy'}}
                for name in sys.modules
            )
    """)


def run_isolated(bootstrap, code, *, site_packages=False):
    command = [sys.executable, "-I"]
    if not site_packages:
        command.append("-S")
    result = subprocess.run(
        [*command, "-c", bootstrap + textwrap.dedent(code)],
        capture_output=True,
        text=True,
        timeout=15,
    )
    assert result.returncode == 0, result.stdout + result.stderr


@pytest.mark.parametrize("site_packages", [False, True])
def test_plain_crm_local_calls_and_errors_never_import_fastdb(
    isolated_source, site_packages,
):
    run_isolated(isolated_source, """
        import c_two as cc
        assert_no_optional_imports()

        class Packet:
            def __init__(self, value):
                self.value = value

        @cc.crm(namespace='test.optional-local', version='0.1.0')
        class Echo:
            def echo(self, value: Packet) -> Packet: ...
            def nested(self, value: dict[str, list[int | None]]) -> list[int | None]: ...
            def nothing(self, value: None) -> None: ...
            def fail(self) -> None: ...

        class Resource:
            def echo(self, value):
                return value
            def nested(self, value):
                return value['values']
            def nothing(self, value):
                assert value is None
            def fail(self):
                raise ValueError('ordinary failure')

        assert_no_optional_imports()
        cc.register(Echo, Resource(), name='optional-local')
        try:
            with cc.connect(Echo, name='optional-local') as proxy:
                packet = Packet(42)
                assert proxy.echo(packet) is packet
                assert proxy.nested({'values': [7, None]}) == [7, None]
                assert proxy.nothing(None) is None
                try:
                    proxy.fail()
                except cc.error.ResourceExecuteFunction as exc:
                    assert 'ordinary failure' in str(exc)
                    assert not exc.details
                else:
                    raise AssertionError('missing resource error')
        finally:
            cc.shutdown()
        assert_no_optional_imports()
    """, site_packages=site_packages)


def test_no_payload_portable_descriptor_needs_no_fastdb(isolated_source):
    run_isolated(isolated_source, """
        import json
        import c_two as cc

        @cc.crm(namespace='test.optional-empty', version='0.1.0')
        class Empty:
            @cc.transfer()
            def ping(self) -> None: ...

        descriptor = json.loads(cc.export_contract_descriptor(Empty))
        reference = json.loads(cc.export_contract_release_ref(Empty))
        assert descriptor['methods'][0]['bindings'] == {'input': None, 'output': None}
        assert reference['contract_schema'] == 'c-two.contract.v2'
        assert_no_optional_imports()
    """)


def test_plain_annotation_validation_without_fastdb(isolated_source):
    run_isolated(isolated_source, """
        import c_two as cc
        from c_two.crm.descriptor import build_contract_descriptor

        Payload = type('Payload', (), {'__module__': 'fastdb4py.payload'})

        @cc.crm(namespace='test.optional-annotations', version='0.1.0')
        class Ordinary:
            def echo(self, value: Payload) -> Payload: ...

        method = build_contract_descriptor(Ordinary)['methods'][0]
        assert method['parameters'][0]['annotation']['kind'] == 'python_type'
        assert method['return']['kind'] == 'python_type'

        @cc.crm(namespace='test.optional-unannotated', version='0.1.0')
        class Unannotated:
            def echo(self, value) -> int: ...

        try:
            build_contract_descriptor(Unannotated)
        except TypeError as exc:
            assert 'missing a type annotation' in str(exc), str(exc)
        else:
            raise AssertionError('missing annotation was accepted')
        assert_no_optional_imports()
    """)


def test_ordinary_error_details_do_not_load_or_impersonate_fastdb(isolated_source):
    run_isolated(isolated_source, """
        from c_two.crm.transferable import _cause_details

        PayloadError = type('PayloadError', (RuntimeError,), {
            '__module__': 'fastdb4py.payload',
            'code': 3006,
            'symbol': 'DIGEST_MISMATCH',
        })
        assert _cause_details(ValueError('ordinary failure')) == {}
        assert _cause_details(PayloadError('forged failure')) == {}
        assert_no_optional_imports()
    """)


def test_pickle_ipc_between_processes_without_fastdb(isolated_source):
    contract = textwrap.dedent("""
        import c_two as cc

        @cc.crm(namespace='test.optional-ipc', version='0.1.0')
        class Echo:
            def echo(self, value: dict[str, list[int | None]]) -> dict[str, list[int | None]]: ...
            def fail(self) -> None: ...
    """)
    client = isolated_source + contract + textwrap.dedent("""
        with cc.connect(Echo, name='optional-ipc', address=sys.argv[1]) as proxy:
            assert proxy.client._mode == 'ipc'
            original = {'values': [42, None]}
            copied = proxy.echo(original)
            assert copied == original
            assert copied is not original
            assert copied['values'] is not original['values']
            try:
                proxy.echo(lambda: None)
            except cc.error.ClientSerializeInput as exc:
                assert not exc.details
            else:
                raise AssertionError('unpicklable input was accepted')
            try:
                proxy.fail()
            except cc.error.ResourceExecuteFunction as exc:
                assert 'ordinary IPC failure' in str(exc)
                assert not exc.details
            else:
                raise AssertionError('missing resource error')
        cc.shutdown()
        assert_no_optional_imports()
    """)
    server = contract + textwrap.dedent(f"""
        import subprocess

        class Resource:
            def echo(self, value):
                return value
            def fail(self):
                raise ValueError('ordinary IPC failure')

        cc.register(Echo, Resource(), name='optional-ipc')
        try:
            result = subprocess.run(
                [sys.executable, '-I', '-S', '-c', {client!r}, cc.server_address()],
                capture_output=True, text=True, timeout=10,
            )
            assert result.returncode == 0, result.stdout + result.stderr
        finally:
            cc.shutdown()
        assert_no_optional_imports()
    """)
    run_isolated(isolated_source, server)


@pytest.mark.parametrize("entry", ["binding", "input", "output"])
def test_fastdb_use_without_extra_reports_install_command(isolated_source, entry):
    code = """
        import c_two as cc
        from c_two.crm.payload_plan import fastdb_payload_binding

        try:
            ENTRY
        except ImportError as exc:
            assert 'c-two[fastdb]' in str(exc), str(exc)
            assert 'install' in str(exc).lower(), str(exc)
        else:
            raise AssertionError('FastDB use without extra was accepted')
    """
    if entry == "binding":
        statement = "fastdb_payload_binding({}, label='optional')"
    else:
        statement = textwrap.dedent(f"""
            @cc.crm(namespace='test.optional-missing', version='0.1.0')
            class Missing:
                @cc.transfer({entry}={{}})
                def echo(self, value: bytes) -> bytes: ...
        """).strip()
    code = textwrap.dedent(code).replace("    ENTRY", textwrap.indent(statement, "    "))
    run_isolated(isolated_source, code)


@pytest.mark.parametrize("name,module", [
    ("Payload", __name__),
    ("Pretender", "fastdb4py.payload"),
    ("Payload", "fastdb4py.payload"),
])
@pytest.mark.parametrize("direction", ["input", "output"])
def test_payload_name_and_module_cannot_forge_fastdb_identity(name, module, direction):
    import c_two as cc
    from c_two.crm.descriptor import build_contract_descriptor
    from c_two.crm.payload_plan import fastdb_payload_binding
    from fastdb4py.payload import Payload

    pretender = type(name, (), {"__module__": module})
    assert pretender is not Payload

    @cc.crm(namespace="test.optional-identity", version="0.1.0")
    class Ordinary:
        def echo(self, value: pretender) -> pretender: ...

    descriptor = build_contract_descriptor(Ordinary)["methods"][0]
    assert descriptor["parameters"][0]["annotation"] == {
        "kind": "python_type", "module": module, "name": name,
    }
    assert descriptor["return"]["kind"] == "python_type"

    with pytest.raises(TypeError, match="fastdb4py.payload.Payload"):
        @cc.crm(namespace="test.optional-forged", version="0.1.0")
        class Forged:
            @cc.transfer(**{direction: SPEC})
            def echo(self, value: pretender) -> pretender: ...

    binding = fastdb_payload_binding(SPEC, label="identity")
    try:
        with pytest.raises(TypeError, match="requires fastdb4py.payload.Payload"):
            binding.serialize(pretender())
        with pytest.raises(TypeError, match="only fastdb4py.payload.Payload"):
            binding.invalidate(pretender())
    finally:
        binding.compiled_spec.close()

    @cc.crm(namespace="test.optional-descriptor-identity", version="0.1.0")
    class Portable:
        @cc.transfer(input=SPEC, output=SPEC)
        def echo(self, value: Payload) -> Payload: ...

    annotation = "value" if direction == "input" else "return"
    Portable.echo.__wrapped__.__annotations__[annotation] = pretender
    with pytest.raises(TypeError, match="fastdb4py.payload.Payload"):
        cc.export_contract_descriptor(Portable)


@pytest.mark.parametrize("direction", ["input", "output"])
def test_real_payload_still_requires_explicit_binding(direction):
    import c_two as cc
    from fastdb4py.payload import Payload

    with pytest.raises(TypeError, match=f"without @cc.transfer\\({direction}="):
        @cc.crm(namespace="test.optional-unbound", version="0.1.0")
        class Unbound:
            if direction == "input":
                def echo(self, value: Payload) -> None: ...
            else:
                def echo(self) -> Payload: ...
