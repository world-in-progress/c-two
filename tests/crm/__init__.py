import os
import c_two as cc
import numpy as np
import pandas as pd
import pyarrow as pa
import pyarrow.ipc as ipc
from icrm import IGrid, GridSchema, GridAttribute

# Const ##############################

ATTR_MIN_X = 'min_x'
ATTR_MIN_Y = 'min_y'
ATTR_MAX_X = 'max_x'
ATTR_MAX_Y = 'max_y'
ATTR_LOCAL_ID = 'local_id'

ATTR_DELETED = 'deleted'
ATTR_ACTIVATE = 'activate'

ATTR_TYPE = 'type'
ATTR_LEVEL = 'level'
ATTR_GLOBAL_ID = 'global_id'
ATTR_ELEVATION = 'elevation'

GRID_SCHEMA: pa.Schema = pa.schema([
    (ATTR_DELETED, pa.bool_()),
    (ATTR_ACTIVATE, pa.bool_()), 
    (ATTR_TYPE, pa.int8()),
    (ATTR_LEVEL, pa.int8()),
    (ATTR_GLOBAL_ID, pa.int32()),
    (ATTR_ELEVATION, pa.float64())
])

@cc.iicrm
class Grid(IGrid):
    """
    CRM
    =
    The Grid Resource.  
    Grid is a 2D grid system that can be subdivided into smaller grids by pre-declared subdivide rules.  
    """
    def __init__(self, epsg: int, bounds: list, first_size: list[float], subdivide_rules: list[list[int]], grid_file_path: str = None):
        """Method to initialize Grid

        Args:
            epsg (int): epsg code of the grid
            bounds (list): bounding box of the grid (organized as [min_x, min_y, max_x, max_y])
            first_size (list[float]): [width, height] of the first level grid
            subdivide_rules (list[list[int]]): list of subdivision rules per level
            grid_file_path (str, optional): path to .arrow file containing grid data. If provided, grid data will be loaded from this file
        """
        self.direction = '<-'
        self.epsg: int = epsg
        self.bounds: list = bounds
        self.first_size: list[float] = first_size
        self.subdivide_rules: list[list[int]] = subdivide_rules
        self.grid_file_path = grid_file_path
        
        # Initialize grid DataFrame
        self.grids = pd.DataFrame(columns=[
            ATTR_DELETED, ATTR_ACTIVATE, ATTR_TYPE, 
            ATTR_LEVEL, ATTR_GLOBAL_ID, ATTR_ELEVATION
        ])
        
        # Calculate level info for later use
        self.level_info: list[dict[str, int]] = [{'width': 1, 'height': 1}]
        for level, rule in enumerate(subdivide_rules[:-1]):
            prev_width, prev_height = self.level_info[level]['width'], self.level_info[level]['height']
            self.level_info.append({
                'width': prev_width * rule[0],
                'height': prev_height * rule[1]
            })
        
        self.grid_definition = {
            'epsg': epsg,
            'bounds': bounds,
            'first_size': first_size,
            'subdivide_rules': subdivide_rules
        }
        
        # Load from Arrow file if file exists
        if grid_file_path and os.path.exists(grid_file_path):
            try:
                # Load grid data from Arrow file
                self._load_grid_from_file(grid_file_path, batch_size=50000)
            except Exception as e:
                print(f'Failed to load grid data from file: {str(e)}, the grid will be initialized using default method')
                self._initialize_default_grid(batch_size=50000)
        else:
            # Initialize grid data using default method
            print('grid file does not exist, initializing default grid data...')
            self._initialize_default_grid(batch_size=50000)
            print('Successfully initialized default grid data')
     
    def terminate(self):
        """Save the grid data to Arrow file

        Args:
            grid_file_path (str, optional): The file path to save the grid data. If None, the path provided during initialization is used

        Returns:
            bool: Whether the save was successful
        """
        
        save_path = self.grid_file_path
        if not save_path:
            print('No file path provided for saving grid data')
            return False
        
        try:
            if self.grids.empty:
                print('No grid data to save')
                return False
            
            # Reset index to include level and globale_id columns in the table
            df = self.grids.reset_index()
            table = pa.Table.from_pandas(df, schema=GRID_SCHEMA)
            
            # Write to Arrow file
            with pa.ipc.new_file(save_path, GRID_SCHEMA) as writer:
                writer.write_table(table)
            
            print(f'Successfully saved grid data to {save_path}')
            return True
        
        except Exception as e:
            print(f'Failed to save grid data: {str(e)}')
            return False

    def _load_grid_from_file(self, file_path: str, batch_size: int = 50000):
        """Load grid data from file streaming

        Args:
            file_path (str): Arrow file path
            batch_size (int): number of records processed per batch
        """
        
        try:
            with pa.ipc.open_file(self.grid_file_path)  as reader:
                # Open the Arrow file and read the table
                all_batches = []
                for i in range(reader.num_record_batches):
                    batch = reader.get_batch(i)
                    all_batches.append(batch)
                    
                    if len(all_batches) >= batch_size / batch.num_rows:
                        partial_table = pa.Table.from_batches(all_batches, schema=GRID_SCHEMA)
                        all_batches = []
                        partial_df = partial_table.to_pandas(use_threads=True)
                        partial_df.set_index([ATTR_LEVEL, ATTR_GLOBAL_ID], inplace=True)
                        
                        if hasattr(self, 'grids') and not self.grids.empty:
                            self.grids = pd.concat([self.grids, partial_df])
                        else:
                            self.grids = partial_df
                
                if all_batches:
                    partial_table = pa.Table.from_batches(all_batches, schema=GRID_SCHEMA)
                    partial_df = partial_table.to_pandas(use_threads=True)
                    partial_df.set_index([ATTR_LEVEL, ATTR_GLOBAL_ID], inplace=True)
                        
                    if hasattr(self, 'grids') and not self.grids.empty:
                        self.grids = pd.concat([self.grids, partial_df])
                    else:
                        self.grids = partial_df
            
                self.grids = self.grids.sort_index()
                print(f'Successfully loaded {len(self.grids)} grid records from {self.grid_file_path}')
        
        except Exception as e:
            print(f'Error loading grid data from file: {str(e)}')
            raise e

    def _initialize_default_grid(self, batch_size: int = 10000):
        """Initialize grid data (ONLY Level 1) as pandas DataFrame"""
        level = 1
        total_width = self.level_info[level]['width']
        total_height = self.level_info[level]['height']
        num_grids = total_width * total_height
        
        global_ids = np.arange(num_grids, dtype=np.int32)
        grid_data = {
            ATTR_ACTIVATE: np.full(num_grids, True),
            ATTR_DELETED: np.full(num_grids, False, dtype=np.bool_),
            ATTR_TYPE: np.zeros(num_grids, dtype=np.int8),
            ATTR_LEVEL: np.full(num_grids, level, dtype=np.int8),
            ATTR_GLOBAL_ID: global_ids,
            ATTR_ELEVATION: np.full(num_grids, -9999.0, dtype=np.float64)
        }
        
        df = pd.DataFrame(grid_data)
        df.set_index([ATTR_LEVEL, ATTR_GLOBAL_ID], inplace=True)
        
        self.grids = df
        print(f'Successfully initialized grid data with {num_grids} grids at level 1')
   
    def _get_local_ids(self, level: int, global_ids: np.ndarray) -> np.ndarray:
        """Method to calculate local_ids for provided grids having same level
        
        Args:
            level (int): level of provided grids
            global_ids (list[int]): global_ids of provided grids
        
        Returns:
            local_ids (list[int]): local_ids of provided grids
        """
        if level == 0:
            return global_ids
        total_width = self.level_info[level]['width']
        sub_width = self.subdivide_rules[level - 1][0]
        sub_height = self.subdivide_rules[level - 1][1]
        local_x = global_ids % total_width
        local_y = global_ids // total_width
        return (((local_y % sub_height) * sub_width) + (local_x % sub_width))
    
    def _get_parent_global_id(self, level: int, global_id: int) -> int:
        """Method to get parent global id
        Args:
            level (int): level of provided grids
            global_id (int): global_id of provided grids
        Returns:
            parent_global_id (int): parent global id of provided grids
        """
        total_width = self.level_info[level]['width']
        sub_width = self.subdivide_rules[level - 1][0]
        sub_height = self.subdivide_rules[level - 1][1]
        u = global_id % total_width
        v = global_id // total_width
        return (v // sub_height) * sub_width + (u // sub_width)
    
    def _get_coordinates(self, level: int, global_ids: np.ndarray) -> tuple[np.ndarray, np.ndarray, np.ndarray, np.ndarray]:
        """Method to calculate coordinates for provided grids having same level
        
        Args:
            level (int): level of provided grids
            global_ids (list[int]): global_ids of provided grids

        Returns:
            coordinates (tuple[list[float], list[float], list[float], list[float]]): coordinates of provided grids, orgnized by tuple of (min_xs, min_ys, max_xs, max_ys)
        """
        bbox = self.bounds
        width = self.level_info[level]['width']
        height = self.level_info[level]['height']
        
        golbal_xs = global_ids % width
        global_ys = global_ids // width
        min_xs = bbox[0] + (bbox[2] - bbox[0]) * golbal_xs / width
        min_ys = bbox[1] + (bbox[3] - bbox[1]) * global_ys / height
        max_xs = bbox[0] + (bbox[2] - bbox[0]) * (golbal_xs + 1) / width
        max_ys = bbox[1] + (bbox[3] - bbox[1]) * (global_ys + 1) / height
        return (min_xs, min_ys, max_xs, max_ys)

    def _get_grid_info(self, level: int, global_id: int) -> pd.DataFrame:
        key = f'{level}-{global_id}'
        buffer = self._redis_client.get(key)
        if buffer is None:
            return pd.DataFrame(columns=[ATTR_LEVEL, ATTR_GLOBAL_ID, ATTR_LOCAL_ID, ATTR_TYPE, 
                                       ATTR_ELEVATION, ATTR_DELETED, ATTR_ACTIVATE, 
                                       ATTR_MIN_X, ATTR_MIN_Y, ATTR_MAX_X, ATTR_MAX_Y])
        
        reader = ipc.open_stream(buffer)
        grid_record = reader.read_next_batch()
        table = pa.Table.from_batches([grid_record], schema=GRID_SCHEMA)
        df = table.to_pandas(use_threads=True)
        
        # Calculate computed attributes
        local_id = self._get_local_ids(level, np.array([global_id]))[0]
        min_x, min_y, max_x, max_y = self._get_coordinates(level, np.array([global_id]))
        df[ATTR_LOCAL_ID] = local_id
        df[ATTR_MIN_X] = min_x
        df[ATTR_MIN_Y] = min_y
        df[ATTR_MAX_X] = max_x
        df[ATTR_MAX_Y] = max_y
        
        column_order = [ATTR_LEVEL, ATTR_GLOBAL_ID, ATTR_LOCAL_ID, ATTR_TYPE, ATTR_ELEVATION, 
                        ATTR_DELETED, ATTR_ACTIVATE, ATTR_MIN_X, ATTR_MIN_Y, ATTR_MAX_X, ATTR_MAX_Y]
        return df[column_order]
    
    def _get_grid_children_global_ids(self, level: int, global_id: int) -> list[int] | None:
        if (level < 0) or (level >= len(self.level_info)):
            return None
        
        width = self.level_info[level]['width']
        global_u = global_id % width
        global_v = global_id // width
        sub_width = self.subdivide_rules[level][0]
        sub_height = self.subdivide_rules[level][1]
        sub_count = sub_width * sub_height
        
        baseGlobalWidth = width * sub_width
        child_global_ids = [0] * sub_count
        for local_id in range(sub_count):
            local_u = local_id % sub_width
            local_v = local_id // sub_width
            
            sub_global_u = global_u * sub_width + local_u
            sub_global_v = global_v * sub_height + local_v
            child_global_ids[local_id] = sub_global_v * baseGlobalWidth + sub_global_u
        
        return child_global_ids
    
    def get_schema(self) -> GridSchema:
        """Method to get grid schema

        Returns:
            GridSchema: grid schema
        """
        return GridSchema(
            epsg=self.epsg,
            bounds=self.bounds,
            first_size=self.first_size,
            subdivide_rules=self.subdivide_rules
        )
      
    def get_parent_keys(self, levels: list[int], global_ids: list[int]) -> list[str | None]:
        """Method to get parent keys for provided grids having same level

        Args:
            levels (list[int]): levels of provided grids
            global_ids (list[int]): global_ids of provided grids

        Returns:
            parent_keys (list[str]): parent keys of provided grids, organized by list of strings in the format "level-global_id"
        """
        parent_keys: list[str | None]  = []
        for level, global_id in zip(levels, global_ids):
            if level == 1:
                parent_keys.append(None)
                continue
            
            parent_global_id = self._get_parent_global_id(level, global_id)
            parent_key = f'{level - 1}-{parent_global_id}'
            parent_keys.append(parent_key)
        return parent_keys
    
    def get_grid_infos(self, level: int, global_ids: list[int]) -> list[GridAttribute]:
        """Method to get all attributes for provided grids having same level

        Args:
            level (int): level of provided grids
            global_ids (list[int]): global_ids of provided grids

        Returns:
            grid_infos (list[GridAttribute]): grid infos organized by GridAttribute objects with attributes: 
            level, global_id, local_id, type, elevation, deleted, activate, min_x, min_y, max_x, max_y
        """
        index_keys = [(level, global_id) for global_id in global_ids]
        filtered_grids = self.grids.loc[self.grids.index.isin(index_keys)]
        if filtered_grids.empty:
            return []
        
        # Reset index to have level and global_id as columns
        filtered_grids = filtered_grids.reset_index()
        
        global_ids_numpy = filtered_grids[ATTR_GLOBAL_ID].to_numpy()
        local_ids = self._get_local_ids(level, global_ids_numpy)
        min_xs, min_ys, max_xs, max_ys = self._get_coordinates(level, global_ids_numpy)
        
        filtered_grids[ATTR_LOCAL_ID] = local_ids
        filtered_grids[ATTR_MIN_X] = min_xs
        filtered_grids[ATTR_MIN_Y] = min_ys
        filtered_grids[ATTR_MAX_X] = max_xs
        filtered_grids[ATTR_MAX_Y] = max_ys
        
        return [
            GridAttribute(
                deleted=row[ATTR_DELETED],
                activate=row[ATTR_ACTIVATE],
                type=row[ATTR_TYPE],
                level=row[ATTR_LEVEL],
                global_id=row[ATTR_GLOBAL_ID],
                local_id=row[ATTR_LOCAL_ID],
                elevation=row[ATTR_ELEVATION],
                min_x=row[ATTR_MIN_X],
                min_y=row[ATTR_MIN_Y],
                max_x=row[ATTR_MAX_X],
                max_y=row[ATTR_MAX_Y]
            )
            for _, row in filtered_grids.iterrows()
        ]
    
    def subdivide_grids(self, levels: list[int], global_ids: list[int]) -> list[str | None]:
        """
        Subdivide grids by turning off parent grids' activate flag and activating children's activate flags
        if the parent grid is activate and not deleted.

        Args:
            levels (list[int]): Array of levels for each grid to subdivide
            global_ids (list[int]): Array of global IDs for each grid to subdivide

        Returns:
            grid_keys (list[str]): List of child grid keys in the format "level-global_id"
        """
        # Get all parents
        parent_idx_keys = list(zip(levels, global_ids))
        parent_idx = pd.MultiIndex.from_tuples(parent_idx_keys, names=[ATTR_LEVEL, ATTR_GLOBAL_ID])
        existing_parents = parent_idx.intersection(self.grids.index)
        if len(existing_parents) == 0:
            return []
        
        # Filter for valid parents
        valid_parents = self.grids.loc[existing_parents]
        valid_parents = valid_parents[(valid_parents[ATTR_ACTIVATE]) & (~valid_parents[ATTR_DELETED])]
        if valid_parents.empty:
            return []
        
        # Collect all child grid information
        all_child_data = []
        all_child_keys = []
        
        # Process each parent grid
        for (level, global_id) in valid_parents.index:
            child_global_ids = self._get_grid_children_global_ids(level, global_id)
            if not child_global_ids:
                continue
            
            # Generate child keys for return value
            child_level = level + 1
            child_keys = [f'{child_level}-{child_global_id}' for child_global_id in child_global_ids]
            all_child_keys.extend(child_keys)
            
            # Create child records
            for child_global_id in child_global_ids:
                all_child_data.append((
                    child_level, child_global_id, 
                    False, True, 0, -9999.0,
                ))
        if not all_child_data:
            return []
        
        # Make child DataFrame
        children = pd.DataFrame(
            all_child_data,
            columns=[ATTR_LEVEL, ATTR_GLOBAL_ID, ATTR_DELETED, ATTR_ACTIVATE, ATTR_TYPE, ATTR_ELEVATION]
        )
        children.set_index([ATTR_LEVEL, ATTR_GLOBAL_ID], inplace=True)
        
        # Update existing children and add new ones
        child_idx = children.index
        existing_mask = child_idx.isin(self.grids.index)
        if existing_mask.any():
            # Update existing children attributes
            existing_indices = child_idx[existing_mask]
            self.grids.loc[existing_indices, ATTR_ACTIVATE] = True
            self.grids.loc[existing_indices, ATTR_DELETED] = False
            
            # Add only new children
            new_children = children.loc[~existing_mask]
            if not new_children.empty:
                self.grids = pd.concat([self.grids, new_children])
        else:
            # All children are new
            self.grids = pd.concat([self.grids, children])
    
        # Deactivate parent grids
        valid_parent_indices = valid_parents.index
        self.grids.loc[valid_parent_indices, ATTR_ACTIVATE] = False
        
        return all_child_keys
    
    def delete_grids(self, levels: list[int], global_ids: list[int]):
        """Method to delete grids.

        Args:
            levels (list[int]): levels of grids to delete
            global_ids (list[int]): global_ids of grids to delete
        """
        idx_keys = list(zip(levels, global_ids))
        idx = pd.MultiIndex.from_tuples(idx_keys, names=[ATTR_LEVEL, ATTR_GLOBAL_ID])
        existing_grids = idx.intersection(self.grids.index)
        if len(existing_grids) == 0:
            return
        
        # Filter for valid grids
        valid_grids = self.grids.loc[existing_grids]
        valid_grids = valid_grids[valid_grids[ATTR_ACTIVATE] & (~valid_grids[ATTR_DELETED])]
        if valid_grids.empty:
            return
        
        # Update deleted status
        self.grids.loc[valid_grids.index, ATTR_DELETED] = True
        self.grids.loc[valid_grids.index, ATTR_ACTIVATE] = False
    
    def get_active_grid_infos(self) -> tuple[list[int], list[int]]:
        """Method to get all active grids' global ids and levels

        Returns:
            tuple[list[int], list[int]]: active grids' global ids and levels
        """
        active_grids = self.grids[self.grids[ATTR_ACTIVATE] == True]
        # print(active_grids.index.get_level_values(0).tolist(), active_grids.index.get_level_values(1).tolist())
        return active_grids.index.get_level_values(0).tolist(), active_grids.index.get_level_values(1).tolist()

    def hello(self, name: str) -> str:
        return f'Hello, {name}!'
    
    def no_hello(self, message: str) -> str | None:
        if message == 'world':
            return None
        return f'Hello, {message}!'