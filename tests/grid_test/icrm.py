import numpy as np
import pandas as pd
from abc import ABC, abstractmethod

class ICRM(ABC):
    """
    ICRM
    =
    Interface of Core Resource Model (ICRM) specifies how to interact with open data organized by `crm-nh-grid<https://github.com/world-in-progress/crm-nh-grid>`_ and published by `Noodle <https://github.com/world-in-progress/noodle>`_. 

    Attributes of Grid
    ---
    - level (int8): the level of the grid
    - type (int8): the type of the grid, default to 0
    - subdivided (bool), the subdivision status of the grid
    - deleted (bool): the deletion status of the grid, default to False
    - elevation (float64): the elevation of the grid, default to -9999.0
    - global_id (int32): the global id within the bounding box that subdivided by grids all in the level of this grid
    - local_id (int32): the local id within the parent grid that subdivided by child grids all in the level of this grid
    - min_x (float64): the min x coordinate of the grid
    - min_y (float64): the min y coordinate of the grid
    - max_x (float64): the max x coordinate of the grid
    - max_y (float64): the max y coordinate of the grid
    """
        
    @staticmethod
    @abstractmethod
    def create(redis_host: str, redis_port: str, epsg: int, bounds: list, first_size: list[float], subdivide_rules: list[list[int]]):
        """Method to initialize CRM

        Args:
            redis_host (str): host name of redis service
            redis_port (str): port of redis service
            epsg (int): epsg code of the grid
            bounds (list): bounding box of the grid (organized as [min_x, min_y, max_x, max_y])
            first_size (list[float]): [width, height] of the first level grid
            subdivide_rules (list[list[int]]): list of subdivision rules per level
        """
        pass
    
    @abstractmethod
    def get_local_ids(self, level: int, global_ids: np.ndarray) -> np.ndarray:
        """Method to calculate local_ids for provided grids having same level
        
        Args:
            level (int): level of provided grids
            global_ids (np.ndarray): global_ids of provided grids
        
        Returns:
            local_ids (np.ndarray): local_ids of provided grids
        """
        pass
    
    @abstractmethod
    def get_coordinates(self, level: int, global_ids: np.ndarray) -> tuple[np.ndarray, np.ndarray, np.ndarray, np.ndarray]:
        """Method to calculate coordinates for provided grids having same level
        
        Args:
            level (int): level of provided grids
            global_ids (np.ndarray): global_ids of provided grids

        Returns:
            coordinates (tuple[np.ndarray, np.ndarray, np.ndarray, np.ndarray]): coordinates of provided grids, orgnized by tuple of (min_xs, min_ys, max_xs, max_ys)
        """
        pass
    
    @abstractmethod
    def get_grid_infos(self, level: int, global_ids: np.ndarray) -> pd.DataFrame:
        """Method to get all attributes for provided grids having same level

        Args:
            level (int): level of provided grids
            global_ids (np.ndarray): global_ids of provided grids

        Returns:
            grid_infos (pd.DataFrame): grid infos orgnized by dataFrame, the order of which is: level, global_id, local_id, type, elevation, deleted, activate, tl_x, tl_y, br_x, br_y
        """
        pass
    
    @abstractmethod
    def subdivide_grids(self, levels: np.ndarray, global_ids: np.ndarray, batch_size: int = 1000) -> list[str]:
        """
        Subdivide grids by turning off parent grids' activate flag and activating children's activate flags
        if the parent grid is activate and not deleted.

        Args:
            levels (np.ndarray): Array of levels for each grid to subdivide
            global_ids (np.ndarray): Array of global IDs for each grid to subdivide
            batch_size (int): Size of batches for writing to Redis

        Returns:
            grid_keys (list[str]): List of child grid keys in the format "level-global_id"
        """
        pass
