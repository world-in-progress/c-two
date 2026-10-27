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
        shm_threshold: int | None = None,
        remote_payload_chunk_size: int | None = None,
    ) -> None:
        self._relay_anchor_address = _clean_optional_str(relay_anchor_address)
        self._shm_threshold: int | None = None
        self._remote_payload_chunk_size: int | None = None
        if shm_threshold is not None:
            self.shm_threshold = shm_threshold
        if remote_payload_chunk_size is not None:
            self.remote_payload_chunk_size = remote_payload_chunk_size

    @property
    def relay_anchor_address(self) -> str | None:
        if self._relay_anchor_address is not None:
            return self._relay_anchor_address
        return _resolve_relay_anchor_address()

    @relay_anchor_address.setter
    def relay_anchor_address(self, value: str | None) -> None:
        self._relay_anchor_address = _clean_optional_str(value)

    @property
    def shm_threshold(self) -> int:
        if self._shm_threshold is not None:
            return self._shm_threshold
        return _resolve_shm_threshold(self._shm_overrides())

    @shm_threshold.setter
    def shm_threshold(self, value: int | None) -> None:
        if value is None:
            self._shm_threshold = None
            return
        value = int(value)
        if value <= 0:
            raise ValueError(f'shm_threshold must be > 0, got {value}')
        self._shm_threshold = value

    @property
    def remote_payload_chunk_size(self) -> int:
        if self._remote_payload_chunk_size is not None:
            return self._remote_payload_chunk_size
        return _resolve_remote_payload_chunk_size(None)

    @remote_payload_chunk_size.setter
    def remote_payload_chunk_size(self, value: int | None) -> None:
        if value is None:
            self._remote_payload_chunk_size = None
            return
        self._remote_payload_chunk_size = _resolve_remote_payload_chunk_size(int(value))

    def _shm_overrides(self) -> dict[str, int]:
        overrides: dict[str, int] = {}
        if self._shm_threshold is not None:
            overrides['shm_threshold'] = self._shm_threshold
        return overrides

    def _remote_payload_chunk_size_override(self) -> int | None:
        return self._remote_payload_chunk_size


def _clean_optional_str(value: str | None) -> str | None:
    if value is None:
        return None
    value = value.strip()
    return value or None


def _resolve_shm_threshold(shm_overrides: dict[str, int]) -> int:
    from c_two._native import resolve_shm_threshold

    return int(resolve_shm_threshold(shm_overrides))


def _resolve_relay_anchor_address() -> str | None:
    from c_two._native import resolve_relay_anchor_address

    return resolve_relay_anchor_address()


def _resolve_remote_payload_chunk_size(override_value: int | None) -> int:
    from c_two._native import resolve_remote_payload_chunk_size

    return int(resolve_remote_payload_chunk_size(override_value))


settings = C2Settings()
