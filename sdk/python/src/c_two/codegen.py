"""Rust-authoritative portable contract artifact projection."""

from __future__ import annotations

from dataclasses import dataclass
from typing import Literal

from . import _native

ContractCodegenError = _native.ContractCodegenError
ContractCodegenTarget = Literal["rust", "python", "typescript"]


@dataclass(frozen=True, slots=True)
class ContractArtifact:
    relative_path: str
    kind: str
    bytes: bytes
    sha256: str
    owner: str
    source: str


@dataclass(frozen=True, slots=True)
class ContractArtifactSet:
    artifacts: tuple[ContractArtifact, ...]
    total_bytes: int

    def get(self, relative_path: str) -> ContractArtifact | None:
        """Return an artifact by its portable relative path."""
        for artifact in self.artifacts:
            if artifact.relative_path == relative_path:
                return artifact
        return None


def compile_contract_artifacts(
    descriptor_json: bytes | bytearray | memoryview,
    *,
    target: ContractCodegenTarget,
) -> ContractArtifactSet:
    """Compile one v2 descriptor through the shared Rust/Core authority chain."""
    projection = _native.compile_contract_artifacts_projection(
        bytes(descriptor_json),
        target=target,
    )
    artifacts = tuple(
        ContractArtifact(
            relative_path=item["relative_path"],
            kind=item["kind"],
            bytes=item["bytes"],
            sha256=item["sha256"],
            owner=item["owner"],
            source=item["source"],
        )
        for item in projection["artifacts"]
    )
    return ContractArtifactSet(
        artifacts=artifacts,
        total_bytes=projection["total_bytes"],
    )


__all__ = [
    "ContractArtifact",
    "ContractArtifactSet",
    "ContractCodegenError",
    "ContractCodegenTarget",
    "compile_contract_artifacts",
]
