"""Process-level configuration facade for Python code overrides."""
from __future__ import annotations

class C2Settings:
    """Process-level facade for SDK code overrides.

    Environment variables and ``.env`` files are resolved by the Rust
    ``c2-config`` resolver. This object only stores Python code-level
    overrides such as ``cc.set_relay_anchor()``.
    """

    def __init__(
        self,
        *,
        relay_anchor_address: str | None = None,
    ) -> None:
        self._relay_anchor_address = _clean_optional_str(relay_anchor_address)

    @property
    def relay_anchor_address(self) -> str | None:
        if self._relay_anchor_address is not None:
            return self._relay_anchor_address
        return _resolve_relay_anchor_address()

    @relay_anchor_address.setter
    def relay_anchor_address(self, value: str | None) -> None:
        self._relay_anchor_address = _clean_optional_str(value)


def _clean_optional_str(value: str | None) -> str | None:
    if value is None:
        return None
    value = value.strip()
    return value or None


def _resolve_relay_anchor_address() -> str | None:
    from c_two._native import resolve_relay_anchor_address

    return resolve_relay_anchor_address()


settings = C2Settings()
