"""Source-level guards for the Python SDK responsibility boundary."""
from __future__ import annotations

import ast
from pathlib import Path


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
        export_contract_payload_abi_artifacts,
        export_contract_descriptor,
    )
    from c_two.crm.infer import infer_crm_from_resource

    assert cc.contract_descriptor_diagnostics is contract_descriptor_diagnostics
    assert cc.export_contract_payload_abi_artifacts is export_contract_payload_abi_artifacts
    assert cc.export_contract_descriptor is export_contract_descriptor
    assert cc.infer_crm_from_resource is infer_crm_from_resource
    assert {
        'contract_descriptor_diagnostics',
        'export_contract_payload_abi_artifacts',
        'export_contract_descriptor',
        'infer_crm_from_resource',
    } <= set(cc.__all__)


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


def test_python_does_not_own_buffer_lease_accounting():
    root = Path(__file__).resolve().parents[2] / "src" / "c_two"
    offenders = []
    forbidden = [
        "class " + "Hold" + "Registry",
        "weakref." + "ref(request_buf",
        "_hold" + "_registry",
        "_entr" + "ies",
        "total_held_bytes " + "+=",
    ]
    for path in root.rglob("*.py"):
        text = path.read_text(encoding="utf-8")
        for needle in forbidden:
            if needle in text:
                offenders.append(f"{path.relative_to(root)}:{needle}")
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
    assert "start_and_wait" in start_source

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
    session_rs = root / "core" / "runtime" / "c2-runtime" / "src" / "session.rs"
    source = session_rs.read_text(encoding="utf-8")
    assert "socket_path().exists()" not in source


def test_runtime_session_uses_commit_gated_server_registration() -> None:
    root = Path(__file__).resolve().parents[4]
    session_rs = root / "core" / "runtime" / "c2-runtime" / "src" / "session.rs"
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
    server_ffi = (native_root / "server_ffi.rs").read_text(encoding="utf-8")
    client_ffi = (native_root / "client_ffi.rs").read_text(encoding="utf-8")
    http_ffi = (native_root / "http_ffi.rs").read_text(encoding="utf-8")

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
        if needle in runtime_session or needle in server_ffi
    ]
    assert default_offenders == []

    assert "CRM calls require a route-bound client" in client_ffi
    assert "CRM calls require a route-bound relay-aware client" in http_ffi
    old_call_signature = "fn call<'py>(\n        &self,\n        py: Python<'py>,\n        route_name: &str,"
    assert old_call_signature not in client_ffi
    assert old_call_signature not in http_ffi
    assert old_call_signature not in runtime_session


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


def test_crm_proxy_does_not_pass_route_name_into_native_call():
    import inspect
    from c_two.transport.client.proxy import CRMProxy

    source = inspect.getsource(CRMProxy.call)
    assert "self._name" not in source
    assert "self._client.call(method_name, data or b'')" in source


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


def test_native_server_response_parser_does_not_accept_shm_coordinate_tuples():
    root = Path(__file__).resolve().parents[4]
    server_ffi = root / "sdk" / "python" / "native" / "src" / "server_ffi.rs"
    source = server_ffi.read_text(encoding="utf-8")
    start = source.index("fn parse_response_meta")
    end = source.index("// ---------------------------------------------------------------------------", start)
    parser_source = source[start:end]

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
    bridge_source = (
        root / "sdk" / "python" / "src" / "c_two" / "transport" / "server" / "native.py"
    ).read_text(encoding="utf-8")
    native_source = (
        root / "sdk" / "python" / "native" / "src" / "server_ffi.rs"
    ).read_text(encoding="utf-8")
    runtime_session_source = (
        root / "sdk" / "python" / "native" / "src" / "runtime_session_ffi.rs"
    ).read_text(encoding="utf-8")

    assert "fn shutdown(" not in native_source
    assert "fn unregister_route(" not in native_source
    assert "fn register_route(" not in native_source
    assert "unregister_route_blocking" not in native_source
    assert "fn _shutdown_runtime_barrier(" not in native_source
    assert "shutdown_runtime_barrier_blocking" in native_source
    assert "self._rust_server.register_route(" not in bridge_source
    assert "self._rust_server.shutdown()" not in bridge_source
    assert "self._rust_server._shutdown_runtime_barrier()" not in bridge_source
    assert "shutdown_runtime_barrier_blocking(py, timeout)" in runtime_session_source
    assert "outcome.get('removed_routes')" not in bridge_source
    assert "_close_outcome_is_hook_safe" in bridge_source


def test_python_native_server_ffi_does_not_build_tokio_runtime_directly():
    root = Path(__file__).resolve().parents[4]
    native_root = root / "sdk" / "python" / "native" / "src"
    direct_builder_uses: list[str] = []

    for path in native_root.rglob("*.rs"):
        text = path.read_text(encoding="utf-8")
        if "tokio::runtime::Builder::" in text:
            direct_builder_uses.append(path.relative_to(root).as_posix())

    assert direct_builder_uses == []

    server_ffi = (native_root / "server_ffi.rs").read_text(encoding="utf-8")
    assert "ServerRuntimeBuilder::build(" in server_ffi


def test_python_native_server_start_wait_uses_core_responsive_fence():
    root = Path(__file__).resolve().parents[4]
    server_ffi = (
        root / "sdk" / "python" / "native" / "src" / "server_ffi.rs"
    ).read_text(encoding="utf-8")
    core_server = (
        root / "core" / "transport" / "c2-server" / "src" / "server.rs"
    ).read_text(encoding="utf-8")

    assert "wait_until_responsive(timeout)" in server_ffi
    assert "wait_until_ready(timeout)" not in server_ffi.split("fn start_runtime_and_wait", 1)[1]
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

    assert "InvalidAddress { reason: String }" in authority_source
    assert "socket_path_from_ipc_address(address)" in authority_source
    assert "ControlError::InvalidAddress { reason }" in state_source


def test_python_crm_metadata_is_not_parsed_from_slash_tag():
    root = Path(__file__).resolve().parents[2] / "src" / "c_two" / "transport" / "server" / "native.py"
    source = root.read_text(encoding="utf-8")

    assert "tag.split('/')" not in source


def test_route_table_direct_mutations_validate_tombstones_and_private_identity():
    root = Path(__file__).resolve().parents[4]
    route_table = root / "core" / "transport" / "c2-http" / "src" / "relay" / "route_table.rs"
    source = route_table.read_text(encoding="utf-8")

    assert "fn valid_tombstone" in source
    assert "self.valid_tombstone(&tombstone)" in source
    assert "fn valid_server_instance_id" in source
    assert "fn valid_relay_url" in source
    assert "valid_relay_url(&entry.relay_url)" in source
    assert "valid_relay_url(&url)" in source
    assert "let removed = self.routes.get(&key).cloned();" in source
    assert "if !self.apply_tombstone(tombstone)" in source
    assert "c2_ipc::socket_path_from_ipc_address" in source
    assert 'starts_with("ipc://")' not in source
    assert "valid_nonempty_identity" not in source


def test_relay_control_client_does_not_expose_name_only_resolve_to_python():
    root = Path(__file__).resolve().parents[4]
    http_ffi = root / "sdk" / "python" / "native" / "src" / "http_ffi.rs"
    source = http_ffi.read_text(encoding="utf-8")
    start = source.index("impl PyRustRelayControlClient")
    end = source.index("fn py_http_error", start)
    control_client_impl = source[start:end]

    assert "fn resolve(&self" not in control_client_impl
    assert "inner.resolve(&name)" not in control_client_impl
    assert "registration_token" not in control_client_impl


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
    assert "UpstreamResponseUnavailable" in call_handler
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
