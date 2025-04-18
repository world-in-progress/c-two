import c_two as cc
from icrm import IGrid, GridAttribute

@cc.compo.runtime.connect
def get_grid_infos(crm: IGrid, level: int, global_ids: list[int]) -> list[GridAttribute]:
    grids: list[GridAttribute] = crm.get_grid_infos(level, global_ids)
    return grids

@cc.compo.runtime.connect
def subdivide_grids(crm: IGrid, levels: list[int], global_ids: list[int]) -> list[str]:
    keys: list[str] = crm.subdivide_grids(levels, global_ids)
    return keys

@cc.compo.runtime.connect
def get_parent_keys(crm: IGrid, levels: list[int], global_ids: list[int]) -> list[str | None]:
    keys: list[str] = crm.get_parent_keys(levels, global_ids)
    return keys

@cc.compo.runtime.connect
def get_active_grid_infos(crm: IGrid) -> tuple[list[int], list[int]]:
    levels, global_ids = crm.get_active_grid_infos()
    return levels, global_ids
