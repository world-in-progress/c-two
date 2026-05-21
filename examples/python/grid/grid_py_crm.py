from dataclasses import dataclass

import c_two as cc


@dataclass
class GridSchema:
    epsg: int
    bounds: list[float]
    first_size: list[float]
    subdivide_rules: list[list[int]]


@dataclass
class GridAttribute:
    level: int
    type: int
    activate: bool
    global_id: int
    deleted: bool = False
    elevation: float = -9999.0
    local_id: int | None = None
    min_x: float | None = None
    min_y: float | None = None
    max_x: float | None = None
    max_y: float | None = None


@cc.crm(namespace='demo.grid.python', version='0.1.0')
class GridPython:
    """Python-only CRM contract for exercising pickle fallback with dataclasses."""

    def get_schema(self) -> GridSchema:
        ...

    def subdivide_grids(self, levels: list[int], global_ids: list[int]) -> list[str | None]:
        ...

    def get_parent_keys(self, levels: list[int], global_ids: list[int]) -> list[str | None]:
        ...

    def get_grid_infos(self, level: int, global_ids: list[int]) -> list[GridAttribute]:
        ...

    def get_active_grid_infos(self) -> tuple[list[int], list[int]]:
        ...

    def hello(self, name: str) -> str:
        ...

    def none_hello(self, message: str) -> str | None:
        ...
