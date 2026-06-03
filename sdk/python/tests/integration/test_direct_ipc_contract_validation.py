from __future__ import annotations

from collections.abc import Callable

import pytest

import c_two as cc
from c_two.config.settings import settings
from c_two.crm.contract import crm_contract
from c_two.transport.registry import _ProcessRegistry


@pytest.fixture(autouse=True)
def _clean_registry():
    previous_relay = settings.relay_anchor_address
    settings.relay_anchor_address = None
    _ProcessRegistry.reset()
    yield
    _ProcessRegistry.reset()
    settings.relay_anchor_address = previous_relay


class CountingResource:
    def __init__(self) -> None:
        self.calls: list[str] = []

    def echo(self, value):
        self.calls.append("echo")
        return value

    def extra(self, value: str) -> str:
        self.calls.append("extra")
        return value


def _method(
    source: str,
    namespace: dict[str, object] | None = None,
    *,
    func_name: str = "echo",
) -> Callable:
    scope: dict[str, object] = {"cc": cc, "__name__": __name__}
    if namespace:
        scope.update(namespace)
    exec(source, scope)
    return scope[func_name]  # type: ignore[return-value]


def _contract(attrs: dict[str, object]) -> type:
    return cc.crm(namespace="test.direct_ipc_contract", version="0.1.0")(
        type("SharedRoute", (), {"__module__": __name__, **attrs}),
    )


def _mismatch_cases() -> list[tuple[str, type, type]]:
    base = _contract({"echo": _method("def echo(self, value: int) -> int: ...")})
    return [
        (
            "parameter-type",
            base,
            _contract({"echo": _method("def echo(self, value: str) -> int: ...")}),
        ),
        (
            "return-type",
            base,
            _contract({"echo": _method("def echo(self, value: int) -> str: ...")}),
        ),
        (
            "method-set",
            base,
            _contract(
                {
                    "echo": _method("def echo(self, value: int) -> int: ..."),
                    "extra": _method(
                        "def extra(self, value: str) -> str: ...",
                        func_name="extra",
                    ),
                },
            ),
        ),
        (
            "access-mode",
            _contract({"echo": cc.read(_method("def echo(self, value: int) -> int: ..."))}),
            _contract({"echo": cc.write(_method("def echo(self, value: int) -> int: ..."))}),
        ),
        (
            "default-annotation-shape",
            _contract({"echo": _method("def echo(self, value: list[int]) -> list[int]: ...")}),
            _contract({"echo": _method("def echo(self, value: list[str]) -> list[str]: ...")}),
        ),
    ]


@pytest.mark.parametrize(
    ("case_name", "server_contract", "client_contract"),
    _mismatch_cases(),
    ids=[case[0] for case in _mismatch_cases()],
)
def test_thread_local_rejects_same_tag_contract_fingerprint_mismatch(
    case_name: str,
    server_contract: type,
    client_contract: type,
):
    assert case_name
    assert server_contract.__cc_namespace__ == client_contract.__cc_namespace__
    assert server_contract.__cc_name__ == client_contract.__cc_name__
    assert server_contract.__cc_version__ == client_contract.__cc_version__
    assert crm_contract(server_contract) != crm_contract(client_contract)

    resource = CountingResource()
    cc.register(server_contract, resource, name="grid")

    with pytest.raises(TypeError, match="CRM contract mismatch"):
        cc.connect(client_contract, name="grid")

    assert resource.calls == []


@pytest.mark.parametrize(
    ("case_name", "server_contract", "client_contract"),
    _mismatch_cases(),
    ids=[case[0] for case in _mismatch_cases()],
)
def test_explicit_direct_ipc_rejects_same_tag_contract_fingerprint_mismatch_without_relay(
    case_name: str,
    server_contract: type,
    client_contract: type,
):
    assert case_name
    resource = CountingResource()
    cc.register(server_contract, resource, name="grid")
    address = cc.server_address()
    assert address is not None

    cc.set_relay_anchor("http://127.0.0.1:9")

    with pytest.raises(RuntimeError, match="CRM contract mismatch"):
        cc.connect(client_contract, name="grid", address=address)

    assert resource.calls == []


def test_direct_ipc_connect_succeeds_with_bad_relay_setting_when_contract_matches():
    contract = _contract({"echo": _method("def echo(self, value: str) -> str: ...")})
    cc.register(contract, CountingResource(), name="grid")
    address = cc.server_address()
    assert address is not None

    cc.set_relay_anchor("http://127.0.0.1:9")

    proxy = cc.connect(contract, name="grid", address=address)
    try:
        assert proxy.echo("ok") == "ok"
    finally:
        cc.close(proxy)


def test_runtime_session_rejects_direct_ipc_expected_contract_missing_or_malformed_hashes():
    contract = _contract({"echo": _method("def echo(self, value: str) -> str: ...")})
    cc.register(contract, CountingResource(), name="grid")
    address = cc.server_address()
    assert address is not None
    session = _ProcessRegistry.get()._runtime_session  # noqa: SLF001
    expected = crm_contract(contract)

    invalid_hashes = [
        ("", expected.signature_hash),
        (expected.abi_hash, ""),
        ("not-a-sha256", expected.signature_hash),
        ("A" * 64, expected.signature_hash),
    ]

    for abi_hash, signature_hash in invalid_hashes:
        with pytest.raises(ValueError, match="hash"):
            session.acquire_ipc_client(
                address,
                "grid",
                expected.crm_ns,
                expected.crm_name,
                expected.crm_ver,
                abi_hash,
                signature_hash,
            )


def test_direct_ipc_route_contract_projection_includes_abi_and_signature_hashes():
    from c_two._native import RustClientPool

    contract = _contract({"echo": _method("def echo(self, value: str) -> str: ...")})
    expected = crm_contract(contract)
    cc.register(contract, CountingResource(), name="grid")
    address = cc.server_address()
    assert address is not None

    pool = RustClientPool.instance()
    raw_client = pool.acquire(address)
    try:
        assert raw_client.route_contract("grid") == ("grid", *expected.native_args())
    finally:
        pool.release(address)

    session = _ProcessRegistry.get()._runtime_session  # noqa: SLF001
    bound_client = session.acquire_ipc_client(
        address,
        "grid",
        *expected.native_args(),
    )
    try:
        assert bound_client.route_name == "grid"
        assert bound_client.route_contract("grid") == ("grid", *expected.native_args())
    finally:
        session.release_ipc_client(address)


def test_route_bound_native_ipc_client_cannot_dispatch_an_alternate_route_name():
    grid_contract = _contract({"echo": _method("def echo(self, value: str) -> str: ...")})
    other_contract = cc.crm(namespace="test.direct_ipc_contract", version="0.1.0")(
        type(
            "OtherRoute",
            (),
            {
                "__module__": __name__,
                "echo": _method("def echo(self, value: str) -> str: ..."),
            },
        ),
    )
    grid_resource = CountingResource()
    other_resource = CountingResource()
    cc.register(grid_contract, grid_resource, name="grid")
    cc.register(other_contract, other_resource, name="other")
    address = cc.server_address()
    assert address is not None

    grid = cc.connect(grid_contract, name="grid", address=address)
    try:
        call = getattr(grid.client._client, "call")  # noqa: SLF001
        with pytest.raises(TypeError):
            call("other", "echo", b"")
    finally:
        cc.close(grid)

    assert grid_resource.calls == []
    assert other_resource.calls == []
