import c_two as cc

from grid.transferables import GridSchema, GridAttribute

@cc.crm(namespace='demo.grid', version='0.1.0')
class Grid:
    """
    Grid
    =
    CRM contract for a nested multi-resolution grid resource. 
    """
    def get_schema(self) -> GridSchema:
        """Method to get grid schema

        Returns:
            GridSchema: grid schema
        """
        ...
    
    def subdivide_grids(self, levels: list[int], global_ids: list[int]) -> list[str | None]:
        """
        Subdivide grids by turning off parent grids' activate flag and activating children's activate flags
        if the parent grid is activate and not deleted.

        Args:
            levels (list[int]): Array of levels for each grid to subdivide
            global_ids (list[int]): Array of global IDs for each grid to subdivide

        Returns:
            tuple[list[int], list[int]]: The levels and global IDs of the subdivided grids.
        """
        ...
    
    def get_parent_keys(self, levels: list[int], global_ids: list[int]) -> list[str | None]:
        ...

    def get_grid_infos(self, level: int, global_ids: list[int]) -> list[GridAttribute]:
        """Method to get all attributes for provided grids having same level

        Args:
            level (int): level of provided grids
            global_ids (list[int]): global_ids of provided grids

        Returns:
            grid_infos (list[GridAttribute]): grid infos organized by GridAttribute objects with attributes: 
            level, global_id, local_id, type, elevation, deleted, activate, min_x, min_y, max_x, max_y
        """
        ...
    
    def get_active_grid_infos(self) -> tuple[list[int], list[int]]:
        """Method to get all active grids' global ids and levels

        Returns:
            tuple[list[int], list[int]]: active grids' global ids and levels
        """
        ...
        
    def hello(self, name: str) -> str:
        ...
    
    def none_hello(self, message: str) -> str | None:
        ...
    
    @cc.on_shutdown
    def terminate(self) -> None:
        ...
