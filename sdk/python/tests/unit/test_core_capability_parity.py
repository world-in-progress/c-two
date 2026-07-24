from __future__ import annotations

from pathlib import Path

from c_two import _native


EXPECTED_ROWS = [
    {
        "capability": "descriptor_release_identity",
        "authority": "c2-contract",
        "rust": "projection",
        "python": "native_projection",
        "portable": True,
    },
    {
        "capability": "route_lifecycle",
        "authority": "c2-core",
        "rust": "facade",
        "python": "native_facade",
        "portable": True,
    },
    {
        "capability": "direct_ipc",
        "authority": "c2-core",
        "rust": "supported",
        "python": "supported",
        "portable": True,
    },
    {
        "capability": "explicit_relay",
        "authority": "c2-core",
        "rust": "supported",
        "python": "supported",
        "portable": True,
    },
    {
        "capability": "relay_aware",
        "authority": "c2-core",
        "rust": "supported",
        "python": "supported",
        "portable": True,
    },
    {
        "capability": "generated_client_service",
        "authority": "c2-codegen",
        "rust": "supported",
        "python": "supported",
        "portable": True,
    },
    {
        "capability": "no_payload_record_object_graph",
        "authority": "fastdb",
        "rust": "supported",
        "python": "supported",
        "portable": True,
    },
    {
        "capability": "owned_held_borrowed",
        "authority": "c2-core+fastdb",
        "rust": "projection",
        "python": "projection",
        "portable": True,
    },
    {
        "capability": "structured_c2_error",
        "authority": "c2-error",
        "rust": "typed",
        "python": "typed_exception",
        "portable": True,
    },
    {
        "capability": "structured_fastdb_cause",
        "authority": "fastdb+c2-error",
        "rust": "preserved",
        "python": "preserved",
        "portable": True,
    },
    {
        "capability": "python_pickle_thread_local",
        "authority": "python-sdk",
        "rust": "not_copied",
        "python": "explicitly_nonportable",
        "portable": False,
    },
    {
        "capability": "advanced_runtime_embedding",
        "authority": "c2-core",
        "rust": "public_crate",
        "python": "native_binding",
        "portable": True,
    },
]


def test_core_generated_capability_receipt_matches_python_projection_row_by_row():
    receipt = _native.core_capability_receipt()
    assert receipt["schema"] == "c-two.sdk-capability-parity.v1"
    assert receipt["rows"] == EXPECTED_ROWS


def test_python_native_contains_no_second_transport_or_retry_authority():
    native_src = Path(__file__).parents[2] / "native" / "src"
    production = "\n".join(
        path.read_text()
        for path in sorted(native_src.glob("*.rs"))
    )
    for forbidden in (
        "ClientPool",
        "SyncClient",
        "RouteBinding",
        "RelayAwareHttpClient",
        "RelayIpcConnectError",
    ):
        assert forbidden not in production
