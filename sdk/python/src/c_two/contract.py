"""Contract tooling for authors: descriptor export, release identity and code generation."""

from __future__ import annotations

from dataclasses import dataclass
from typing import Literal

from . import _native
from ._native import CodegenError
from .crm.descriptor import export_descriptor, release_ref, diagnostics

CodegenTarget = Literal["rust", "python", "typescript"]


@dataclass(frozen=True, slots=True)
class Artifact:
    relative_path: str
    kind: str
    bytes: bytes
    sha256: str
    owner: str
    source: str


@dataclass(frozen=True, slots=True)
class ArtifactSet:
    artifacts: tuple[Artifact, ...]
    total_bytes: int

    def get(self, relative_path: str) -> Artifact | None:
        """Return an artifact by its portable relative path."""
        for artifact in self.artifacts:
            if artifact.relative_path == relative_path:
                return artifact
        return None


def compile_artifacts(
    descriptor_json: bytes | bytearray | memoryview,
    *,
    target: CodegenTarget,
) -> ArtifactSet:
    """Compile one v2 descriptor through the shared Rust/Core authority chain."""
    projection = _native.compile_contract_artifacts_projection(
        bytes(descriptor_json),
        target=target,
    )
    artifacts = tuple(
        Artifact(
            relative_path=item["relative_path"],
            kind=item["kind"],
            bytes=item["bytes"],
            sha256=item["sha256"],
            owner=item["owner"],
            source=item["source"],
        )
        for item in projection["artifacts"]
    )
    return ArtifactSet(
        artifacts=artifacts,
        total_bytes=projection["total_bytes"],
    )


__all__ = [
    "export_descriptor", "release_ref", "diagnostics",
    "Artifact",
    "ArtifactSet",
    "CodegenError",
    "CodegenTarget",
    "compile_artifacts",
]
