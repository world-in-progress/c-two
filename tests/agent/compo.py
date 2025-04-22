import c_two as cc
from icrm import GridAttribute, IGrid

@cc.compo.runtime.connect
def get_grid_infos(grid: IGrid, level: int, global_ids: list[int]) -> list[GridAttribute]:
    """Method to get information for a set of grids at the same level
    
    [DO NOT CALL DIRECTLY FROM LLM] - Use the flow() function instead
    
    The complete data includes GridAttribute objects with these properties:
    - level: grid level in the hierarchy
    - global_id: unique global identifier
    - local_id: local identifier
    - type: grid type
    - elevation: elevation value
    - deleted: deletion status flag
    - activate: activation status flag
    - min_x, min_y, max_x, max_y: grid boundary coordinates
    
    Args:
        level (int): Level of the grids to fetch (all grids must be at same level)
        global_ids (list[int]): List of global IDs for the target grids
        
    Returns:
        list[GridAttribute]: List of grid attribute objects containing complete grid information
    """
    return grid.get_grid_infos(level, global_ids)

@cc.compo.runtime.connect
def subdivide_grids(grid: IGrid, levels: list[int], global_ids: list[int]) -> tuple[list[int], list[int]]:
    """Method to subdivide grids in the hierarchy
    
    [DO NOT CALL DIRECTLY FROM LLM] - Use the flow() function instead
    
    This function performs grid subdivision by:
    1. Deactivating parent grids (setting activate=False)
    2. Creating and activating child grids (setting activate=True)
    
    The subdivision only occurs if the parent grid is both:
    - Active (activate=True)
    - Not deleted (deleted=False)
    
    Args:
        levels (list[int]): Levels of the parent grids to subdivide
        global_ids (list[int]): Global IDs of the parent grids to subdivide
        
    Returns:
        tuple[list[int], list[int]]: Tuple containing two lists:
            - List of levels for the child grids
            - List of global IDs for the child grids
    """
    
    keys = grid.subdivide_grids(levels, global_ids)
    return _keys_to_levels_global_ids(keys)


# Helpers ##################################################

def _keys_to_levels_global_ids(keys: list[str]) -> tuple[list[int], list[int]]:
    """
    Convert grid keys to levels and global IDs.
    Args:
        keys (list[str]): List of grid keys in the format "level-global_id"
    Returns:
        tuple[list[int], list[int]]: Tuple of two lists - levels and global IDs
    """
    levels: list[int] = []
    global_ids: list[int] = []
    for key in keys:
        level, global_id = map(int, key.split('-'))
        levels.append(level)
        global_ids.append(global_id)
    return levels, global_ids