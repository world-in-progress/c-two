"""Source-level guards for the Python SDK responsibility boundary."""
from __future__ import annotations

import ast
from pathlib import Path
import re

import pytest


def _rust_function(source: str, name: str) -> str:
    """Limit guards to one function, ignoring comment/string brace contents."""
    masked = re.sub(
        r'"(?:\\.|[^"\\])*"|//[^\n]*|/\*.*?\*/',
        lambda match: ' ' * len(match.group()),
        source,
        flags=re.DOTALL,
    )
    declaration = re.search(rf'\bfn\s+{re.escape(name)}\b', masked)
    assert declaration is not None, f'Rust function {name!r} is missing'
    start = declaration.start()
    opening = masked.index('{', declaration.end())
    depth = 1
    for index in range(opening + 1, len(masked)):
        depth += (masked[index] == '{') - (masked[index] == '}')
        if depth == 0:
            return source[start:index + 1]
    raise AssertionError(f'Rust function {name!r} has no closing brace')


def _rust_compact(source: str) -> str:
    """Normalize formatting for token guards, without matching comments."""
    code = re.sub(
        r'"(?:\\.|[^"\\])*"|//[^\n]*|/\*.*?\*/',
        lambda match: match.group() if match.group().startswith('"') else '',
        source,
        flags=re.DOTALL,
    )
    return re.sub(r'\s+', '', code)


def test_registry_does_not_own_relay_control_plane_mechanisms():
    """Relay control-plane HTTP/retry/cache behavior belongs in Rust core."""
    source_path = (
        Path(__file__).resolve().parents[2]
        / "src"
        / "c_two"
        / "transport"
        / "registry.py"
    )
    tree = ast.parse(source_path.read_text())

    forbidden_imports: list[str] = []
    forbidden_calls: list[str] = []
    forbidden_classes: list[str] = []
    forbidden_names: list[str] = []
    forbidden_route_fields: list[str] = []
    forbidden_pool_names: list[str] = []

    for node in ast.walk(tree):
        if isinstance(node, ast.Import):
            for alias in node.names:
                if alias.name in {"urllib.request", "urllib.error"}:
                    forbidden_imports.append(alias.name)
        elif isinstance(node, ast.ImportFrom):
            if node.module in {"urllib.request", "urllib.error"}:
                forbidden_imports.append(node.module)
        elif isinstance(node, ast.ClassDef):
            if node.name == "_RouteCache":
                forbidden_classes.append(node.name)
        elif isinstance(node, ast.FunctionDef):
            if node.name == "_is_local_relay_url":
                forbidden_names.append(node.name)
        elif isinstance(node, ast.Name):
            if node.id in {"RustHttpClientPool", "_http_pool"}:
                forbidden_pool_names.append(node.id)
        elif isinstance(node, ast.Attribute):
            if node.attr == "_http_pool":
                forbidden_pool_names.append(node.attr)
        elif isinstance(node, ast.Call):
            func = node.func
            if isinstance(func, ast.Attribute):
                full_name = _attribute_name(func)
                if full_name in {
                    "urllib.request.urlopen",
                    "urllib.request.Request",
                    "time.sleep",
                }:
                    forbidden_calls.append(full_name)
                if full_name == "route.get" and _first_literal_arg(node) == "ipc_address":
                    forbidden_route_fields.append("route.get('ipc_address')")
            elif isinstance(func, ast.Name) and func.id == "dict":
                continue
        elif isinstance(node, ast.Subscript):
            if (
                isinstance(node.value, ast.Name)
                and node.value.id == "route"
                and _literal_slice(node) == "ipc_address"
            ):
                forbidden_route_fields.append("route['ipc_address']")

    assert forbidden_imports == []
    assert forbidden_classes == []
    assert forbidden_names == []
    assert forbidden_calls == []
    assert forbidden_route_fields == []
    assert forbidden_pool_names == []


def _attribute_name(node: ast.Attribute) -> str:
    parts: list[str] = [node.attr]
    value = node.value
    while isinstance(value, ast.Attribute):
        parts.append(value.attr)
        value = value.value
    if isinstance(value, ast.Name):
        parts.append(value.id)
    return ".".join(reversed(parts))


def _first_literal_arg(node: ast.Call) -> object | None:
    if not node.args:
        return None
    arg = node.args[0]
    if isinstance(arg, ast.Constant):
        return arg.value
    return None


def _literal_slice(node: ast.Subscript) -> object | None:
    if isinstance(node.slice, ast.Constant):
        return node.slice.value
    return None


def test_import_does_not_expose_logo_banner():
    import c_two

    assert not hasattr(c_two, "LOGO" + "_UNICODE")


def test_top_level_exposes_register_concurrency_facade():
    import c_two as cc
    from c_two.transport.server.scheduler import ConcurrencyConfig, ConcurrencyMode

    assert cc.ConcurrencyConfig is ConcurrencyConfig
    assert cc.ConcurrencyMode is ConcurrencyMode
    assert {'ConcurrencyConfig', 'ConcurrencyMode'} <= set(cc.__all__)

    cfg = cc.ConcurrencyConfig(mode=cc.ConcurrencyMode.PARALLEL)
    assert cfg.mode is cc.ConcurrencyMode.PARALLEL


def test_top_level_exposes_public_override_schemas():
    import c_two as cc
    from c_two.config import (
        BaseIPCOverrides,
        ClientIPCOverrides,
        ServerIPCOverrides,
    )

    assert cc.BaseIPCOverrides is BaseIPCOverrides
    assert cc.ServerIPCOverrides is ServerIPCOverrides
    assert cc.ClientIPCOverrides is ClientIPCOverrides
    assert {
        'BaseIPCOverrides',
        'ServerIPCOverrides',
        'ClientIPCOverrides',
    } <= set(cc.__all__)


def test_top_level_exposes_contract_projection_tools():
    import c_two as cc
    from c_two.crm.descriptor import (
        contract_descriptor_diagnostics,
        export_contract_descriptor,
        export_contract_release_ref,
    )
    from c_two.crm.infer import infer_crm_from_resource

    assert cc.contract_descriptor_diagnostics is contract_descriptor_diagnostics
    assert cc.export_contract_descriptor is export_contract_descriptor
    assert cc.export_contract_release_ref is export_contract_release_ref
    assert cc.infer_crm_from_resource is infer_crm_from_resource
    assert {
        'contract_descriptor_diagnostics',
        'export_contract_descriptor',
        'export_contract_release_ref',
        'infer_crm_from_resource',
    } <= set(cc.__all__)


def test_contract_release_identity_is_not_reimplemented_in_python():
    source_path = (
        Path(__file__).resolve().parents[2]
        / 'src'
        / 'c_two'
        / 'crm'
        / 'descriptor.py'
    )
    tree = ast.parse(source_path.read_text(encoding='utf-8'))
    release_export = next(
        node
        for node in tree.body
        if isinstance(node, ast.FunctionDef)
        and node.name == 'export_contract_release_ref'
    )

    native_imports = {
        alias.name
        for node in ast.walk(release_export)
        if isinstance(node, ast.ImportFrom) and node.module == 'c_two._native'
        for alias in node.names
    }
    calls = {
        node.func.id
        for node in ast.walk(release_export)
        if isinstance(node, ast.Call) and isinstance(node.func, ast.Name)
    }
    all_imports = {
        alias.name
        for node in ast.walk(tree)
        if isinstance(node, ast.Import)
        for alias in node.names
    }
    import_from_modules = {
        node.module
        for node in ast.walk(tree)
        if isinstance(node, ast.ImportFrom)
    }
    release_literals = {
        node.value
        for node in ast.walk(release_export)
        if isinstance(node, ast.Constant) and isinstance(node.value, str)
    }

    assert 'contract_release_ref_json' in native_imports
    assert 'contract_release_ref_json' in calls
    assert 'hashlib' not in all_imports
    assert 'hashlib' not in import_from_modules
    assert 'descriptor_sha256' not in release_literals


def test_payload_abi_internals_are_not_public_sdk_surface():
    import importlib.util

    import c_two as cc

    forbidden = {
        'CodecBinding',
        'CodecRef',
        'MethodCodecShape',
        'PayloadAbiBinding',
        'PayloadAbiRef',
        'bind_' + 'codec',
        'use_' + 'codec',
        'MethodPayloadAbiShape',
        'MethodParameterShape',
    }

    assert forbidden.isdisjoint(set(cc.__all__))
    for name in forbidden:
        assert not hasattr(cc, name)

    removed_package = 'c_two.' + 'pro' + 'viders'
    assert importlib.util.find_spec(removed_package) is None


def test_python_examples_do_not_import_removed_provider_package():
    root = Path(__file__).resolve().parents[4]
    examples_root = root / 'examples' / 'python'
    removed_package = 'c_two.' + 'pro' + 'viders'
    offenders = []
    for path in examples_root.rglob('*.py'):
        text = path.read_text(encoding='utf-8')
        if removed_package in text:
            offenders.append(str(path.relative_to(root)))

    assert offenders == []


def test_error_facade_does_not_reimplement_wire_codec():
    source_path = Path(__file__).resolve().parents[2] / "src" / "c_two" / "error.py"
    source = source_path.read_text(encoding="utf-8")

    legacy = "legacy"
    forbidden = [
        ".tobytes()",
        ".decode('utf-8')",
        '.decode("utf-8")',
        ".split(':', 1)",
        '.split(":", 1)',
        "int(code_raw)",
        "Unknown error code {code_value}",
        "invalid UTF-8",
        "missing ':' separator",
        "invalid code",
        f"encode_error_{legacy}",
        f"decode_error_{legacy}",
        f"to_{legacy}_bytes",
        f"from_{legacy}_bytes",
    ]

    offenders = [needle for needle in forbidden if needle in source]
    assert offenders == []
    assert "_native.error_registry" in source
    assert "_native.encode_error_wire" in source
    assert "_native.decode_error_wire_parts" in source


def test_registry_does_not_own_generic_relay_or_route_authority():
    source_path = (
        Path(__file__).resolve().parents[2]
        / "src"
        / "c_two"
        / "transport"
        / "registry.py"
    )
    source = source_path.read_text(encoding="utf-8")

    forbidden = [
        "_relay_control_client",
        "_relay_control_address",
        "_relay_control_client_for",
        "_http_pool",
        "RustHttpClientPool",
        "RustRelayAwareHttpClient",
        "RelayControlClient",
        "resolve_matching",
        "resolve_routes",
        "route_uid",
        "route_revision",
        "FallbackDenied(",
    ]
    offenders = [needle for needle in forbidden if needle in source]
    assert offenders == []
    assert "self._runtime_session.acquire_ipc_client" in source
    assert "self._runtime_session.connect_via_relay" in source
    assert "self._runtime_session.connect_explicit_relay_http" in source


def test_runtime_endpoint_and_accepted_budget_authority_are_native():
    root = Path(__file__).resolve().parents[4]
    registry = root / 'sdk/python/src/c_two/transport/registry.py'
    tree = ast.parse(registry.read_text(encoding='utf-8'))
    state_fields = {
        node.attr for node in ast.walk(tree)
        if isinstance(node, ast.Attribute) and isinstance(node.ctx, ast.Store)
    }
    assert not {
        field for field in state_fields
        if any(part in field for part in (
            'endpoint', 'execution_limits', 'execution_context',
            'outstanding_calls', 'retained_input_budget',
        ))
    }
    calls = {
        _attribute_name(node.func) for node in ast.walk(tree)
        if isinstance(node, ast.Call) and isinstance(node.func, ast.Attribute)
    }
    assert 'self._runtime_session.set_local_endpoint' in calls
    assert 'self._runtime_session.set_call_execution_limits' in calls
    assert 'new_session.inherit_local_endpoint_selection' in calls
    swap = next(
        node for node in ast.walk(tree)
        if isinstance(node, ast.FunctionDef) and node.name == '_swap_runtime_session'
    )
    inheritance = [
        node for node in ast.walk(swap)
        if isinstance(node, ast.Call)
        and isinstance(node.func, ast.Attribute)
        and _attribute_name(node.func) == 'new_session.inherit_local_endpoint_selection'
    ]
    assert len(inheritance) == 1
    assert len(inheritance[0].args) == 1
    assert isinstance(inheritance[0].args[0], ast.Name)
    assert inheritance[0].args[0].id == 'runtime_session'
    # Swapping inherits Core's lazy/accepted selection without resolving a
    # context, re-reading configuration, or deriving OS paths in Python.
    swap_calls = {
        ast.unparse(node.func) for node in ast.walk(swap)
        if isinstance(node, ast.Call)
    }
    assert not any(
        part in call for call in swap_calls
        for part in ('local_endpoint_context', 'resolve_local_endpoint',
                     'set_local_endpoint', 'getenv', 'environ', 'Path',
                     'getuid', 'getsid', 'expanduser', 'tempfile', 'socket')
    )
    assert not any(
        isinstance(node, ast.Attribute)
        and (node.attr == 'environ' or 'local_endpoint' in node.attr)
        and node.attr != 'inherit_local_endpoint_selection'
        for node in ast.walk(swap)
    )
    assert any(
        isinstance(node, ast.Attribute)
        and node.attr == 'call_execution_limits_overrides'
        and isinstance(node.value, ast.Name)
        and node.value.id == 'runtime_session'
        and isinstance(node.ctx, ast.Load)
        for node in ast.walk(tree)
    )

    native = (root / 'sdk/python/native/src/runtime_session_ffi.rs').read_text(encoding='utf-8')
    getter = _rust_compact(_rust_function(native, 'call_execution_limits_overrides'))
    assert 'letoverrides=self.inner.call_execution_limits_overrides();' in getter
    context = _rust_compact(_rust_function(native, 'local_endpoint_context'))
    assert 'self.inner.local_endpoint_context()' in context
    inheritance_native = _rust_compact(_rust_function(native, 'inherit_local_endpoint_selection'))
    assert 'previous:&Self' in inheritance_native
    assert 'self.inner.inherit_local_endpoint_selection(&previous.inner)' in inheritance_native
    struct = re.search(
        r'\bpub\s+struct\s+PyRuntimeSession\s*\{(.*?)\n\s*\}', native, flags=re.DOTALL,
    )
    assert struct is not None
    fields = set(re.findall(r'^\s*(\w+)\s*:', struct.group(1), flags=re.MULTILINE))
    assert not {field for field in fields if 'endpoint' in field or 'overrides' in field}


def test_endpoint_admin_facade_only_projects_native_context_and_actions():
    path = Path(__file__).resolve().parents[2] / 'src/c_two/transport/endpoint.py'
    tree = ast.parse(path.read_text(encoding='utf-8'))
    imported = {
        alias.name for node in ast.walk(tree) if isinstance(node, ast.Import)
        for alias in node.names
    } | {
        node.module for node in ast.walk(tree) if isinstance(node, ast.ImportFrom)
    }
    assert not imported.intersection({'os', 'pathlib', 'socket', 'json', 'tempfile'})
    assert any(
        isinstance(node, ast.ImportFrom) and node.module == 'c_two._native'
        and any(alias.name == 'LocalEndpointContext' for alias in node.names)
        for node in tree.body
    )
    functions = {node.name: node for node in tree.body if isinstance(node, ast.FunctionDef)}
    selection = functions['_selected_context']
    assert any(
        isinstance(node, ast.Call) and isinstance(node.func, ast.Attribute)
        and node.func.attr == 'local_endpoint_context'
        and isinstance(node.func.value, ast.Attribute)
        and node.func.value.attr == '_runtime_session'
        for node in ast.walk(selection)
    )
    for name, native_operation in (
        ('inspect_endpoint', 'inspect_endpoint_endpoint'),
        ('reap_endpoint', 'reap_endpoint_credential'),
        ('sweep_endpoints', 'PyEndpointSweep'),
    ):
        assert any(
            isinstance(node, ast.Call) and isinstance(node.func, ast.Attribute)
            and node.func.attr == native_operation
            and any(keyword.arg == 'context' for keyword in node.keywords)
            for node in ast.walk(functions[name])
        ), name


def test_explicit_ipc_connect_branch_bypasses_relay_facade():
    source_path = (
        Path(__file__).resolve().parents[2]
        / "src"
        / "c_two"
        / "transport"
        / "registry.py"
    )
    source = source_path.read_text(encoding="utf-8")
    ipc_branch = source.split("elif address is not None:", 1)[1].split("else:", 1)[0]

    assert "self._runtime_session.acquire_ipc_client" in ipc_branch
    forbidden = [
        "_sync_relay_override",
        "connect_via_relay",
        "connect_explicit_relay_http",
        "ResourceUnavailable",
        "RegistryUnavailable",
    ]
    offenders = [needle for needle in forbidden if needle in ipc_branch]
    assert offenders == []


def test_python_does_not_own_buffer_lease_accounting():
    root = Path(__file__).resolve().parents[2] / "src" / "c_two"
    offenders = []
    forbidden = [
        "class " + "Hold" + "Registry",
        "weakref." + "ref(request_buf",
        "_hold" + "_registry",
        "_hold" + "_entries",
        "total_held_bytes " + "+=",
    ]
    for path in root.rglob("*.py"):
        text = path.read_text(encoding="utf-8")
        for needle in forbidden:
            if needle in text:
                offenders.append(f"{path.relative_to(root)}:{needle}")
        # Match the old registry's identifier, without rejecting unrelated
        # public budgets such as max_entries.
        for node in ast.walk(ast.parse(text)):
            if (isinstance(node, ast.Name) and node.id == "_entries") or (
                isinstance(node, ast.Attribute) and node.attr == "_entries"
            ):
                offenders.append(f"{path.relative_to(root)}:_entries")
    assert offenders == []


def test_python_server_bridge_does_not_own_readiness_polling():
    import inspect
    from c_two.transport.server.native import NativeServerBridge

    source = inspect.getsource(NativeServerBridge)
    forbidden = [
        "os.path.exists",
        "while not os.path",
        "self._started",
        "_started =",
    ]
    offenders = [needle for needle in forbidden if needle in source]
    assert offenders == []

    start_source = inspect.getsource(NativeServerBridge.start)
    assert "ensure_host_started" in start_source

    shutdown_source = inspect.getsource(NativeServerBridge.shutdown)
    assert "if self.is_started()" not in shutdown_source


def test_native_server_bridge_constructor_has_no_crm_registration_compat():
    import inspect
    from c_two.transport.server.native import NativeServerBridge

    signature = inspect.signature(NativeServerBridge)
    for obsolete in ("crm_class", "crm_instance", "concurrency", "name"):
        assert obsolete not in signature.parameters

    source = inspect.getsource(NativeServerBridge.__init__)
    forbidden = [
        "Register initial CRM",
        "if crm_class is not None",
        "self.register_crm(",
        "_default_concurrency",
        "_default_name",
    ]
    offenders = [needle for needle in forbidden if needle in source]
    assert offenders == []


def test_runtime_session_does_not_infer_started_from_socket_file():
    from pathlib import Path

    root = Path(__file__).resolve().parents[4]
    session_rs = root / "core" / "runtime" / "c2-core" / "src" / "session.rs"
    source = session_rs.read_text(encoding="utf-8")
    assert "socket_path().exists()" not in source


def test_runtime_session_uses_commit_gated_server_registration() -> None:
    root = Path(__file__).resolve().parents[4]
    session_rs = root / "core" / "runtime" / "c2-core" / "src" / "session.rs"
    source = session_rs.read_text(encoding="utf-8")

    assert "server.reserve_route(route)" in source
    assert "server.commit_reserved_route(" in source
    assert "server.abort_reserved_route(" in source
    assert "server.register_route(route)" not in source


def test_native_route_contract_boundaries_have_no_empty_defaults_or_raw_calls():
    root = Path(__file__).resolve().parents[4]
    native_root = root / "sdk" / "python" / "native" / "src"

    runtime_session = (native_root / "runtime_session_ffi.rs").read_text(
        encoding="utf-8",
    )
    core_ffi = (native_root / "core_ffi.rs").read_text(encoding="utf-8")

    forbidden_defaults = [
        'route_name=""',
        'expected_crm_ns=""',
        'expected_crm_name=""',
        'expected_crm_ver=""',
        'expected_abi_hash=""',
        'expected_signature_hash=""',
        'crm_ns=""',
        'crm_name=""',
        'crm_ver=""',
        'abi_hash=""',
        'signature_hash=""',
    ]
    default_offenders = [
        needle
        for needle in forbidden_defaults
        if needle in _rust_compact(runtime_session)
    ]
    assert default_offenders == []

    for removed in ("client_ffi.rs", "http_ffi.rs", "server_ffi.rs"):
        assert not (native_root / removed).exists()
    assert "Connect::DirectIpc" in runtime_session
    assert "Connect::ExplicitRelay" in runtime_session
    assert "Connect::RelayAware" in runtime_session
    # Native clients are already contract-bound. Preparation reserves exactly
    # one Core scope; consuming it passes encoded bytes without another route.
    client_impl = re.search(r'#\[pymethods\]\s*impl\s+PyCoreClient\s*\{', core_ffi)
    assert client_impl is not None
    client_source = core_ffi[client_impl.start():]
    begin = _rust_compact(_rust_function(client_source, 'begin_call'))
    call = _rust_compact(_rust_function(client_source, 'call'))
    finish = _rust_compact(_rust_function(core_ffi, 'finish'))
    take = _rust_compact(_rust_function(core_ffi, 'take'))
    clone = _rust_compact(_rust_function(core_ffi, 'client'))
    assert 'letclient=self.client()?;' in begin
    assert 'detach(move||client.begin_call(method_name))' in begin
    assert 'self.begin_call(py,method_name)?.call(py,data)' in call
    assert 'encoded:c2_core::EncodedCall' in finish
    assert 'detach(move||encoded.call_held())' in finish
    assert 'self.inner.lock().take()' in take
    assert 'self.inner.lock().clone()' in clone
    assert '.lock()' not in begin + call + finish
    for name in ('call', 'call_prepared'):
        prepared_call = _rust_compact(_rust_function(core_ffi, name))
        assert 'letmutprepared=self.take()?;' in prepared_call
        assert 'begin_call(' not in prepared_call
    for source in (core_ffi, runtime_session):
        assert re.search(
            r'\bfn\s+call(?:<[^>]*>)?\s*\([^)]*\broute_name\s*:', source,
        ) is None


def test_python_crm_call_surfaces_do_not_accept_route_key_arguments():
    root = Path(__file__).resolve().parents[4]
    scan_roots = [
        root / "sdk" / "python" / "src",
        root / "sdk" / "python" / "tests",
    ]
    route_key_names = {"route_name", "name", "route"}
    offenders: list[str] = []

    for scan_root in scan_roots:
        for path in scan_root.rglob("*.py"):
            tree = ast.parse(path.read_text(encoding="utf-8"))
            for node in ast.walk(tree):
                if isinstance(node, ast.FunctionDef) and node.name == "call":
                    positional = [
                        arg.arg for arg in node.args.posonlyargs + node.args.args
                    ]
                    first_payload = positional[1] if positional and positional[0] == "self" else (
                        positional[0] if positional else None
                    )
                    if first_payload in route_key_names:
                        offenders.append(
                            f"{path.relative_to(root)}:{node.lineno}: "
                            f"call() accepts route-key argument {first_payload!r}",
                        )
                elif isinstance(node, ast.Call):
                    if not (
                        isinstance(node.func, ast.Attribute)
                        and node.func.attr == "call"
                    ):
                        continue
                    for keyword in node.keywords:
                        if keyword.arg in route_key_names:
                            offenders.append(
                                f"{path.relative_to(root)}:{node.lineno}: "
                                f".call() uses route-key keyword {keyword.arg!r}",
                            )
                    if len(node.args) >= 3:
                        offenders.append(
                            f"{path.relative_to(root)}:{node.lineno}: "
                            ".call() uses three or more positional arguments",
                        )

    assert offenders == []


@pytest.mark.parametrize('mode', ['ipc', 'http'])
def test_crm_proxy_does_not_pass_route_name_into_native_call(mode):
    import inspect
    import textwrap
    from c_two.transport.client.proxy import CRMProxy

    tree = ast.parse(textwrap.dedent(inspect.getsource(CRMProxy.call)))
    assert not any(
        isinstance(node, ast.Attribute) and node.attr == '_name'
        for node in ast.walk(tree)
    )

    class RouteBoundClientSpy:
        def __init__(self):
            self.calls = []

        def call(self, method_name, data):
            self.calls.append((method_name, data))
            return b'response'

    client = RouteBoundClientSpy()
    proxy = getattr(CRMProxy, mode)(client, 'already-bound-route')
    try:
        for payload in (None, b'request', bytearray(), memoryview(b'request')):
            assert proxy.call('method', payload) == b'response'
            method, forwarded = client.calls[-1]
            assert method == 'method'
            if payload is None:
                assert forwarded == b''
            else:
                assert forwarded is payload
    finally:
        proxy.terminate()


def test_python_server_dispatcher_does_not_own_response_allocation():
    import inspect
    from c_two.transport.server.native import NativeServerBridge

    source = inspect.getsource(NativeServerBridge._make_dispatcher)
    forbidden = [
        "response_pool",
        "len(res_part) >",
        "write_from_buffer",
        "bytes(res_part)",
        "seg_idx",
        "is_dedicated",
    ]
    offenders = [needle for needle in forbidden if needle in source]
    assert offenders == []


def test_core_python_response_bridge_does_not_accept_shm_coordinate_tuples():
    root = Path(__file__).resolve().parents[4]
    core_ffi = root / "sdk" / "python" / "native" / "src" / "core_ffi.rs"
    source = core_ffi.read_text(encoding="utf-8")
    parser_source = _rust_function(source, 'materialize_python_bytes')

    forbidden = [
        "PyTuple",
        "seg_idx: int",
        "offset: int",
        "data_size: int",
        "is_dedicated: bool",
        "seg_idx, offset, data_size",
    ]
    offenders = [needle for needle in forbidden if needle in parser_source]
    assert offenders == []
    assert 'materialize_python_payload_plan(py,value,nbytes)' in _rust_compact(parser_source)

    sink_source = (core_ffi.parent / 'writable_sink.rs').read_text(encoding='utf-8')
    sink = _rust_compact(sink_source)
    assert 'bytes:Option<Vec<u8>>' in sink
    export = _rust_compact(_rust_function(sink_source, '__getbuffer__'))
    transfer = _rust_compact(_rust_function(sink_source, 'take_bytes'))
    assert 'letmutstate=this.inner.lock();' in export
    assert 'if!state.active' in export
    assert '(*view).buf=bytes.as_mut_ptr().cast();' in export
    assert '(*view).obj=ffi::Py_NewRef(slf.as_ptr());' in export
    assert 'state.exports+=1;' in export
    assert 'letmutstate=self.inner.lock();' in transfer
    assert 'state.active=false;' in transfer
    assert transfer.index('ifstate.exports!=0') < transfer.index('.bytes.take()')
    assert 'Err(PyBufferError::new_err(' in transfer
    release = _rust_compact(_rust_function(sink_source, '__releasebuffer__'))
    assert 'self.inner.lock().exports-=1;' in release


def test_prepared_payload_detection_requires_write_into_before_nbytes():
    root = Path(__file__).resolve().parents[4]
    sink_source = (
        root / "sdk" / "python" / "native" / "src" / "writable_sink.rs"
    ).read_text(encoding="utf-8")
    helper_source = _rust_compact(_rust_function(sink_source, 'prepared_plan_nbytes'))

    assert 'plan.getattr("write_into")' in helper_source
    assert "value.is_callable()" in helper_source
    assert helper_source.index('plan.getattr("write_into")') < helper_source.index('plan.getattr("nbytes")')
    assert helper_source.index('plan.getattr("write_into")') < helper_source.index('plan.getattr("byte_length")')
    assert 'Ok(_)=>returnOk(None)' in helper_source
    assert 'Err(err)if!err.is_instance_of::<PyAttributeError>(plan.py())=>returnErr(err)' in helper_source


def test_python_ipc_config_facade_does_not_validate_override_keys():
    root = Path(__file__).resolve().parents[2] / "src" / "c_two"
    ipc_source = (root / "config" / "ipc.py").read_text(encoding="utf-8")
    registry_source = (root / "transport" / "registry.py").read_text(encoding="utf-8")

    forbidden = [
        "_SERVER_KEYS",
        "_CLIENT_KEYS",
        "_FORBIDDEN_IPC_KEYS",
        "_clean_ipc_overrides",
        "_normalize_server_ipc_overrides",
        "_normalize_client_ipc_overrides",
        "unknown IPC override",
        "shm_threshold is a global transport policy",
    ]
    offenders = [
        needle
        for needle in forbidden
        if needle in ipc_source or needle in registry_source
    ]
    assert offenders == []


def test_python_route_concurrency_wrapper_does_not_expose_public_close_authority():
    root = Path(__file__).resolve().parents[2] / "src" / "c_two" / "transport" / "server"
    scheduler_source = (root / "scheduler.py").read_text(encoding="utf-8")
    native_source = (
        Path(__file__).resolve().parents[4]
        / "sdk"
        / "python"
        / "native"
        / "src"
        / "route_concurrency_ffi.rs"
    ).read_text(encoding="utf-8")

    assert "def close(" not in scheduler_source
    assert "def shutdown(" not in scheduler_source
    assert "fn close(" not in native_source
    assert "fn shutdown(" not in native_source
    assert "_shutdown_internal" not in scheduler_source
    assert "fn _shutdown_internal(" not in native_source


def test_python_native_does_not_expose_legacy_shutdown_signal_payloads():
    from c_two import _native

    assert not hasattr(_native, "SHUTDOWN_CLIENT_BYTES")
    assert not hasattr(_native, "SHUTDOWN_ACK_BYTES")


def test_python_native_server_bridge_does_not_expose_public_bool_unit_lifecycle_bypass():
    root = Path(__file__).resolve().parents[4]
    native_root = root / "sdk" / "python" / "native" / "src"
    bridge_source = (
        root / "sdk" / "python" / "src" / "c_two" / "transport" / "server" / "native.py"
    ).read_text(encoding="utf-8")
    runtime_session_source = (
        native_root / "runtime_session_ffi.rs"
    ).read_text(encoding="utf-8")

    assert not (native_root / "server_ffi.rs").exists()
    assert "self._rust_server.register_route(" not in bridge_source
    assert "self._rust_server.shutdown()" not in bridge_source
    assert "self._rust_server._shutdown_runtime_barrier()" not in bridge_source
    assert "runtime_session.register_route(" in bridge_source
    assert "runtime_session.shutdown(" in bridge_source
    assert "host.register(definition)" in runtime_session_source
    assert "registration.close()" in runtime_session_source
    assert "host.shutdown_with_timeout(timeout)" in runtime_session_source
    assert "outcome.get('removed_routes')" not in bridge_source
    assert "_close_outcome_is_hook_safe" in bridge_source


def test_python_native_does_not_build_transport_runtime_directly():
    root = Path(__file__).resolve().parents[4]
    native_root = root / "sdk" / "python" / "native" / "src"
    direct_builder_uses: list[str] = []

    for path in native_root.rglob("*.rs"):
        text = path.read_text(encoding="utf-8")
        if "tokio::runtime::Builder::" in text:
            direct_builder_uses.append(path.relative_to(root).as_posix())

    assert direct_builder_uses == []
    cargo = (native_root.parent / "Cargo.toml").read_text(encoding="utf-8")
    assert "c2-server" not in cargo
    assert "c2-http" not in cargo
    assert "c2-ipc" not in cargo


def test_python_native_server_start_wait_uses_core_responsive_fence():
    root = Path(__file__).resolve().parents[4]
    core_host = (
        root / "core" / "runtime" / "c2-core" / "src" / "host.rs"
    ).read_text(encoding="utf-8")
    core_server = (
        root / "core" / "transport" / "c2-server" / "src" / "server.rs"
    ).read_text(encoding="utf-8")

    assert "wait_until_responsive(options.startup_timeout)" in core_host
    assert "wait_until_ready(options.startup_timeout)" not in core_host
    assert "pub async fn wait_until_responsive(&self, timeout: Duration)" in core_server


def test_relay_does_not_keep_second_crm_tag_validator():
    root = Path(__file__).resolve().parents[4]
    route_table = root / "core" / "transport" / "c2-http" / "src" / "relay" / "route_table.rs"
    source = route_table.read_text(encoding="utf-8")

    assert "fn valid_crm_tag_field" not in source
    assert "c2_contract::validate_crm_tag" in source
    assert "c2_wire::handshake::validate_crm_tag" not in source


def test_route_authority_uses_canonical_relay_id_validator():
    root = Path(__file__).resolve().parents[4]
    authority = root / "core" / "transport" / "c2-http" / "src" / "relay" / "authority.rs"
    source = authority.read_text(encoding="utf-8")

    assert "c2_config::validate_relay_id" in source
    assert "relay_id.trim().is_empty()" not in source


def test_route_authority_reports_invalid_ipc_address_as_validation_error():
    root = Path(__file__).resolve().parents[4]
    authority = root / "core" / "transport" / "c2-http" / "src" / "relay" / "authority.rs"
    state = root / "core" / "transport" / "c2-http" / "src" / "relay" / "state.rs"
    authority_source = authority.read_text(encoding="utf-8")
    state_source = state.read_text(encoding="utf-8")

    assert 'InvalidAddress{reason:String}' in _rust_compact(authority_source)
    validation = _rust_compact(_rust_function(authority_source, 'validate_ipc_address'))
    assert 'self.state.endpoint_context().endpoint(address)' in validation
    assert '.map_err(|err|ControlError::InvalidAddress{reason:err.to_string(),' in validation
    assert 'ConfigSources::from_process' not in validation
    assert 'local_endpoint_from_ipc_address' not in validation
    assert 'ControlError::InvalidAddress{reason}' in _rust_compact(state_source)


def test_python_crm_metadata_is_not_parsed_from_slash_tag():
    root = Path(__file__).resolve().parents[2] / "src" / "c_two" / "transport" / "server" / "native.py"
    source = root.read_text(encoding="utf-8")

    assert "tag.split('/')" not in source


def test_route_table_direct_mutations_validate_tombstones_and_private_identity():
    root = Path(__file__).resolve().parents[4]
    route_table = root / "core" / "transport" / "c2-http" / "src" / "relay" / "route_table.rs"
    source = route_table.read_text(encoding="utf-8")

    compact = _rust_compact(source)
    _rust_function(source, 'valid_tombstone')
    _rust_function(source, 'valid_server_instance_id')
    _rust_function(source, 'valid_relay_url')
    assert 'self.valid_tombstone(&tombstone)' in compact
    assert 'valid_relay_url(&entry.relay_url)' in compact
    assert 'valid_relay_url(&url)' in compact
    assert 'letremoved=self.routes.get(&key).cloned();' in compact
    assert 'if!self.apply_tombstone(tombstone)' in compact
    # A guard elsewhere in this module cannot excuse an unchecked mutation.
    for name in (
        'unregister_route_with_tombstone',
        'unregister_local_route_with_tombstone',
        'unregister_local_route_if_matches',
    ):
        mutation = _rust_compact(_rust_function(source, name))
        assert mutation.index('if!self.valid_tombstone(&tombstone)') < mutation.index(
            'letremoved=self.routes.get(&key).cloned();',
        )
        assert 'if!self.apply_tombstone(tombstone)' in mutation
    apply = _rust_compact(_rust_function(source, 'apply_tombstone'))
    assert apply.index('if!self.valid_tombstone(&tombstone)') < apply.index(
        'self.advance_catalog_revision()',
    ) < apply.index('self.routes.remove(&key)')
    address = _rust_compact(_rust_function(source, 'valid_ipc_address'))
    assert 'address.strip_prefix("ipc://")' in address
    assert '.is_some_and(|id|c2_config::validate_ipc_region_id(id).is_ok())' in address
    assert 'local_endpoint_from_ipc_address' not in address
    assert 'ConfigSources::from_process' not in address
    identity = _rust_compact(_rust_function(source, 'valid_server_id'))
    assert 'c2_config::validate_server_id(server_id).is_ok()' in identity
    assert 'server_id.len()<=MAX_WIRE_TEXT_BYTES' in identity
    assert 'starts_with("ipc://")' not in source
    assert "valid_nonempty_identity" not in source


def test_relay_control_client_is_not_exposed_to_python_native():
    root = Path(__file__).resolve().parents[4]
    native_root = root / "sdk" / "python" / "native" / "src"
    native_source = "\n".join(
        path.read_text(encoding="utf-8")
        for path in native_root.glob("*.rs")
    )

    assert not (native_root / "http_ffi.rs").exists()
    assert "PyRustRelayControlClient" not in native_source
    assert "RelayControlClient" not in native_source


def test_relay_skip_ipc_validation_is_not_a_production_surface():
    root = Path(__file__).resolve().parents[4]
    cli_relay = (root / "cli" / "src" / "relay.rs").read_text(encoding="utf-8")
    relay_config = (
        root / "core" / "foundation" / "c2-config" / "src" / "relay.rs"
    ).read_text(encoding="utf-8")
    resolver = (
        root / "core" / "foundation" / "c2-config" / "src" / "resolver.rs"
    ).read_text(encoding="utf-8")
    router = (
        root / "core" / "transport" / "c2-http" / "src" / "relay" / "router.rs"
    ).read_text(encoding="utf-8").split("\n#[cfg(test)]\nmod tests")[0]
    server = (
        root / "core" / "transport" / "c2-http" / "src" / "relay" / "server.rs"
    ).read_text(encoding="utf-8").split("\n#[cfg(test)]\nmod tests")[0]

    production_sources = "\n".join([cli_relay, relay_config, resolver, router, server])
    forbidden = [
        "skip_ipc_validation",
        "skip-ipc-validation",
        "SKIP_VALIDATION",
    ]
    offenders = [needle for needle in forbidden if needle in production_sources]
    assert offenders == []


def test_native_server_bridge_requires_explicit_route_name():
    root = Path(__file__).resolve().parents[2] / "src" / "c_two" / "transport" / "server" / "native.py"
    source = root.read_text(encoding="utf-8")

    assert "name: str | None = None" not in source
    assert "routing_name = name if name is not None else crm_ns" not in source


def test_crm_proxy_ipc_does_not_autodiscover_route_name():
    root = Path(__file__).resolve().parents[2] / "src" / "c_two" / "transport" / "client" / "proxy.py"
    source = root.read_text(encoding="utf-8")
    start = source.index("    def ipc(")
    end = source.index("    @classmethod\n    def http(", start)
    ipc_factory = source[start:end]

    assert "Auto-discover route name" not in ipc_factory
    assert "route_names()" not in ipc_factory
    assert "names[0]" not in ipc_factory


def test_crm_proxy_does_not_expose_raw_relay_wire_entrypoint():
    root = Path(__file__).resolve().parents[2] / "src" / "c_two" / "transport" / "client" / "proxy.py"
    source = root.read_text(encoding="utf-8")

    assert "    def relay(" not in source
    assert "._client.relay(" not in source


def test_relay_router_does_not_silence_response_materialization_errors():
    root = Path(__file__).resolve().parents[4]
    router_source = (
        root / "core" / "transport" / "c2-http" / "src" / "relay" / "router.rs"
    ).read_text(encoding="utf-8")
    production = router_source.split("\n#[cfg(test)]\nmod tests")[0]
    call_handler = production[production.index("async fn call_handler"):]
    call_handler = call_handler[:call_handler.index("async fn acquire_request_client")]

    assert "into_bytes_with_pool" in call_handler
    materialization = call_handler[call_handler.index("fn materialized_response_or_error"):]
    assert "resource_unavailable_response_with_phase" in materialization
    assert '"dispatch_uncertain"' in materialization
    assert '"pre_dispatch"' not in materialization
    assert "Json(serde_json::json!" not in materialization
    assert "unwrap_or_default()" not in call_handler


def test_chunked_dispatch_does_not_default_missing_route_metadata():
    root = Path(__file__).resolve().parents[4]
    server_source = (
        root / "core" / "transport" / "c2-server" / "src" / "server.rs"
    ).read_text(encoding="utf-8")
    production = server_source.split("\n#[cfg(test)]\nmod tests")[0]

    assert "finished.route_name.unwrap_or_default()" not in production
    assert "finished.method_idx.unwrap_or(0)" not in production
    assert "chunked call missing route metadata" in production
