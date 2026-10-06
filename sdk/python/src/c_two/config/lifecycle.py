"""Typed lifecycle configuration; Rust validates and freezes all values."""
from __future__ import annotations
from dataclasses import dataclass
from typing import Literal

LifecyclePolicyName = Literal['persistent', 'owner_bound']

@dataclass(frozen=True)
class LifecycleConfig:
    policy: LifecyclePolicyName = 'persistent'
    owner_missing_grace_seconds: float | None = None

    @classmethod
    def persistent(cls) -> LifecycleConfig:
        return cls()

    @classmethod
    def owner_bound(cls, owner_missing_grace_seconds: float) -> LifecycleConfig:
        return cls('owner_bound', owner_missing_grace_seconds)

    def native_args(self) -> tuple[str, float | None]:
        return self.policy, self.owner_missing_grace_seconds
