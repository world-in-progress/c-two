import os
import json
import redis
import numpy as np
import pandas as pd
import pyarrow as pa
import pyarrow.ipc as ipc
import pyarrow.compute as pc
import c_two as cc
from icrm import GridAttribute, ICRM

# Const ##############################
_GRID_DEFINITION = 'grid_definition'

_ACTIVE_SET = 'Activate'

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

class CRM(ICRM):
    """ 
    CRM
    =
    The Grid Resource.  
    Grid is a 2D grid system that can be subdivided into smaller grids by pre-declared subdivide rules.  
    """
    def __init__(self, redis_host: str, redis_port: int, epsg: int, bounds: list, first_size: list[float], subdivide_rules: list[list[int]], grid_file_path: str = None):
        """Method to initialize Grid

        Args:
            redis_host (str): host name of redis service
            redis_port (str): port of redis service
            epsg (int): epsg code of the grid
            bounds (list): bounding box of the grid (organized as [min_x, min_y, max_x, max_y])
            first_size (list[float]): [width, height] of the first level grid
            subdivide_rules (list[list[int]]): list of subdivision rules per level
            grid_file_path (str, optional): path to .arrow file containing grid data. If provided, grid data will be loaded from this file
        """
        self.direction = '<-'
        self.grid_file_path = grid_file_path
        self._redis_client: redis.Redis = redis.Redis(host=redis_host, port=redis_port, decode_responses=False)
        
        # Calculate level info for later use
        level_info: list[dict[str, int]] = [{'width': 1, 'height': 1}]
        for level, rule in enumerate(subdivide_rules[:-1]):
            prev_width, prev_height = level_info[level]['width'], level_info[level]['height']
            level_info.append({
                'width': prev_width * rule[0],
                'height': prev_height * rule[1]
            })
        
        grid_definition = {
            'epsg': epsg,
            'bounds': bounds,
            'first_size': first_size,
            'level_info': level_info,
            'subdivide_rules': subdivide_rules
        }
        
        # Check if grid definition exists in Redis
        if not self._redis_client.exists(_GRID_DEFINITION):
            # Store grid definition in Redis
            self._redis_client.set(_GRID_DEFINITION, json.dumps(grid_definition).encode('utf-8'))
            
            # Load from Arrow file if provided and file exists
            if grid_file_path and os.path.exists(grid_file_path):
                try:
                    # Load grid data from Arrow file
                    self._load_grid_from_file(grid_file_path)
                except Exception as e:
                    print(f'Failed to load grid data from file: {str(e)}, the grid will be initialized using default method')
                    self._initialize_default_grid(level_info)
            else:
                # Initialize grid data using default method
                print('No grid file path provided or file does not exist, initializing default grid data...')
                self._initialize_default_grid(level_info)
                print('Successfully initialized default grid data')

        # Set members
        self.epsg: int = epsg
        self.bounds: list = bounds
        self.first_size: list[float] = first_size
        self.level_info: list[dict[str, int]] = level_info
        self.subdivide_rules: list[list[int]] = subdivide_rules
    
    def _load_grid_from_file(self, file_path: str, batch_size: int = 50000):
        """Load grid data from file streaming

        Args:
            file_path (str): Arrow file path
            batch_size (int): number of records processed per batch
        """
        keys_to_activate = []
        pipe = self._redis_client.pipeline(transaction=False)
        batch_count = 0
        
        with pa.ipc.open_file(file_path) as reader:
            num_record_batches = reader.num_record_batches
            
            for i in range(num_record_batches):
                batch = reader.get_batch(i)
                level_col = batch.column(GRID_SCHEMA.get_field_index(ATTR_LEVEL))
                global_id_col = batch.column(GRID_SCHEMA.get_field_index(ATTR_GLOBAL_ID))
                activate_col = batch.column(GRID_SCHEMA.get_field_index(ATTR_ACTIVATE))
                
                for j in range(batch.num_rows):
                    level = level_col[j].as_py()
                    global_id = global_id_col[j].as_py()
                    key = f'{level}-{global_id}'
                    
                    single_row_batch = batch.slice(j, 1)
                    sink = pa.BufferOutputStream()
                    with pa.ipc.new_stream(sink, GRID_SCHEMA) as writer:
                        writer.write_batch(single_row_batch)
                    
                    pipe.set(key, sink.getvalue().to_pybytes())
                    
                    if activate_col[j].as_py():
                        keys_to_activate.append(key)
                    
                    batch_count += 1
                    
                    if batch_count >= batch_size:
                        if keys_to_activate:
                            pipe.sadd(_ACTIVE_SET, *keys_to_activate)
                        pipe.execute()
                        pipe = self._redis_client.pipeline(transaction=False)
                        keys_to_activate = []
                        batch_count = 0
                        
        if batch_count > 0:
            if keys_to_activate:
                pipe.sadd(_ACTIVE_SET, *keys_to_activate)
            pipe.execute()
        
        print(f'Successfully loaded grid data from file {file_path}')

    def _initialize_default_grid(self, level_info: list[dict[str, int]], batch_size: int = 5000):
        """Initialize grid data (ONLY Level 1) as PyArrow Table"""
        level = 1
        total_width = level_info[level]['width']
        total_height = level_info[level]['height']
        num_grids = total_width * total_height
        
        global_ids = np.arange(num_grids, dtype=np.int32)
        
        pipe = self._redis_client.pipeline(transaction=False)
        keys_to_activate = []
        processed = 0
        
        while processed < num_grids:
            end = min(processed + batch_size, num_grids)
            current_batch_size = end - processed
            
            batch_data = {
                ATTR_ACTIVATE: np.full(current_batch_size, True),
                ATTR_DELETED: np.full(current_batch_size, False, dtype=np.bool_),
                ATTR_TYPE: np.zeros(current_batch_size, dtype=np.int8),
                ATTR_LEVEL: np.full(current_batch_size, level, dtype=np.int8),
                ATTR_GLOBAL_ID: global_ids[processed:end],
                ATTR_ELEVATION: np.full(current_batch_size, -9999.0, dtype=np.float64)
            }
            
            batch = pa.RecordBatch.from_pydict(batch_data, schema=GRID_SCHEMA)
            
            for i in range(current_batch_size):
                global_id = batch_data[ATTR_GLOBAL_ID][i]
                key = f'{level}-{global_id}'
                keys_to_activate.append(key)
                
                single_record = batch.slice(i, 1)
                
                sink = pa.BufferOutputStream()
                with pa.ipc.new_stream(sink, GRID_SCHEMA) as writer:
                    writer.write_batch(single_record)
                
                pipe.set(key, sink.getvalue().to_pybytes())
            
            processed = end
            
            if keys_to_activate:
                pipe.sadd(_ACTIVE_SET, *keys_to_activate)
            pipe.execute()
            pipe = self._redis_client.pipeline(transaction=False)
            keys_to_activate = []
    
    def terminate(self, grid_file_path: str = None):
        """Save the grid data to Arrow file, clear Redis data, and close the connection

        Args:
            grid_file_path (str, optional): The file path to save the grid data. If None, the path provided during initialization is used

        Returns:
            bool: Whether the save was successful
        """
        
        save_path = grid_file_path or self.grid_file_path
        if not save_path:
            print('No file path provided for saving grid data')
            return False
        
        try:
            # Fetch all grid keys from Redis
            all_keys = []
            cursor = 0
            while True:
                cursor, keys = self._redis_client.scan(cursor=cursor, match='[0-9]*-[0-9]*')
                all_keys.extend(keys)
                if cursor == 0:
                    break
            
            if not all_keys:
                print('No grid data found in Redis')
                return False
            
            # Create a list to store the batches
            buffer_list = self._redis_client.mget(all_keys)
            grid_records = []
            for buffer in buffer_list:
                if buffer:
                    reader = ipc.open_stream(buffer)
                    grid_records.append(reader.read_next_batch())
            
            if not grid_records:
                print('No grid data found in Redis')
                return False

            # Create a table from record batches
            table = pa.Table.from_batches(grid_records, schema=GRID_SCHEMA)
            
            # Save to .arrow file
            with pa.ipc.new_file(save_path, GRID_SCHEMA) as writer:
                writer.write_table(table)
            
            print(f'Successfully saved grid data to {save_path}')
            
            # Clear all grid-related data from Redis
            self._redis_client.delete(*all_keys)
            self._redis_client.delete(_GRID_DEFINITION)
            self._redis_client.delete(_ACTIVE_SET)
            print('Successfully cleared grid data from Redis')
            
            return True
        
        except Exception as e:
            print(f'Failed to save or clear grid data: {str(e)}')
        
        finally:
            if hasattr(self, '_redis_client') and self._redis_client:
                self._redis_client.close()
            
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
    
    @cc.message(input_name='PeerGridInfos', output_name='GridAttributes')
    def get_grid_infos(self, level: int, global_ids: list[int]) -> list[GridAttribute]:
        """Method to get all attributes for provided grids having same level

        Args:
            level (int): level of provided grids
            global_ids (list[int]): global_ids of provided grids

        Returns:
            grid_infos (list[GridAttribute]): grid infos organized by dataFrame, the order of which is: level, global_id, local_id, type, elevation, deleted, activate, tl_x, tl_y, br_x, br_y
        """
        keys = [f'{level}-{global_id}' for global_id in global_ids]
        buffer_list = self._redis_client.mget(keys)
        
        grid_records = []
        for buffer in buffer_list:
            if buffer:
                reader = ipc.open_stream(buffer)
                grid_records.append(reader.read_next_batch())
                
        if not grid_records:
            return []
        
        # Create a single table from the record batches
        table = pa.Table.from_batches(grid_records, schema=GRID_SCHEMA)
        
        # Calculate computed attributes directly using Arrow compute
        global_ids_array = table.column(GRID_SCHEMA.get_field_index(ATTR_GLOBAL_ID)).to_numpy()
        local_ids = self._get_local_ids(level, global_ids_array)
        min_xs, min_ys, max_xs, max_ys = self._get_coordinates(level, global_ids_array)
        
        # Add computed attributes as new columns to the Arrow table
        table = table.append_column(ATTR_LOCAL_ID, pa.array(local_ids))
        table = table.append_column(ATTR_MIN_X, pa.array(min_xs))
        table = table.append_column(ATTR_MIN_Y, pa.array(min_ys))
        table = table.append_column(ATTR_MAX_X, pa.array(max_xs))
        table = table.append_column(ATTR_MAX_Y, pa.array(max_ys))
    
        # Convert the Arrow table to a list of GridAttribute objects
        return  [
            GridAttribute(
                deleted=table[ATTR_DELETED][row_idx].as_py(),
                activate=table[ATTR_ACTIVATE][row_idx].as_py(),
                type=table[ATTR_TYPE][row_idx].as_py(),
                level=table[ATTR_LEVEL][row_idx].as_py(),
                global_id=table[ATTR_GLOBAL_ID][row_idx].as_py(),
                local_id=table[ATTR_LOCAL_ID][row_idx].as_py(),
                elevation=table[ATTR_ELEVATION][row_idx].as_py(),
                min_x=table[ATTR_MIN_X][row_idx].as_py(),
                min_y=table[ATTR_MIN_Y][row_idx].as_py(),
                max_x=table[ATTR_MAX_X][row_idx].as_py(),
                max_y=table[ATTR_MAX_Y][row_idx].as_py()
            )
            for row_idx in range(table.num_rows)
        ]
    
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
    
    @cc.message(input_name='GridInfos', output_name='GridKeys')
    def subdivide_grids(self, levels: list[int], global_ids: list[int]) -> list[str]:
        """
        Subdivide grids by turning off parent grids' activate flag and activating children's activate flags
        if the parent grid is activate and not deleted.

        Args:
            levels (list[int]): Array of levels for each grid to subdivide
            global_ids (list[int]): Array of global IDs for each grid to subdivide
            batch_size (int): Size of batches for writing to Redis

        Returns:
            grid_keys (list[str]): List of child grid keys in the format "level-global_id"
        """
        def get_grid_children(row) -> tuple[np.ndarray, np.ndarray] | None:
            level = row[ATTR_LEVEL]
            global_id = row[ATTR_GLOBAL_ID]
            child_global_ids = self._get_grid_children_global_ids(level, global_id)
            child_levels = np.full(child_global_ids.size, level + 1, dtype=np.int8)
            
            return (child_levels, child_global_ids)
        
        # Create parent table
        parent_keys: list[str] = []
        for level, global_id in zip(levels, global_ids):
            parent_keys.append(f'{level}-{global_id}')
        parent_buffers = self._redis_client.mget(parent_keys)
        
        parent_batches = []
        for buffer in parent_buffers:
            if buffer:
                reader = ipc.open_stream(buffer)
                parent_batches.append(reader.read_next_batch())
                
        mask = (pc.field(ATTR_DELETED) == False) & (pc.field(ATTR_ACTIVATE) == True)
        parent_table = pa.Table.from_batches(parent_batches, schema=GRID_SCHEMA).filter(mask)
        
        child_levels = [] 
        child_global_ids = []
        for row_idx in range(parent_table.num_rows):
            level = parent_table[ATTR_LEVEL][row_idx].as_py()
            global_id = parent_table[ATTR_GLOBAL_ID][row_idx].as_py()
            _global_ids = self._get_grid_children_global_ids(level, global_id)
            child_levels.extend([level + 1] * len(_global_ids))
            child_global_ids.extend(_global_ids)
        
        # parent_df: pd.DataFrame = pa.Table.from_batches(parent_batches, schema=_GRID_SCHEMA).filter(mask).to_pandas()
        
        # Get all child infos
        # child_info_series: pd.DataFrame = parent_df.apply(get_grid_children, axis=1)
        # if child_info_series.empty:
        #     return []
        
        # child_levels, child_global_ids = [info for infos in child_info_series for info in infos]
        
        # Create children table
        child_keys = [f'{level}-{global_id}' for level, global_id in zip(child_levels, child_global_ids)]
        child_num = len(child_keys)
        child_table = pa.Table.from_pydict(
            {
                ATTR_ACTIVATE: np.full(child_num, True),
                ATTR_DELETED: np.full(child_num, False, dtype=np.bool),
                ATTR_TYPE: np.zeros(child_num, dtype=np.int8),
                ATTR_LEVEL: child_levels,
                ATTR_GLOBAL_ID: child_global_ids,
                ATTR_ELEVATION: np.full(child_num, -9999.0, dtype=np.float64)
            }, 
            schema=GRID_SCHEMA
        )
        
        # Write all children to redis
        with self._redis_client.pipeline() as pipe:
            batches = child_table.to_batches(max_chunksize=10000)
            for batch in batches:
                for row_idx in range(batch.num_rows):
                    key = f'{batch[ATTR_LEVEL][row_idx].as_py()}-{batch[ATTR_GLOBAL_ID][row_idx].as_py()}'
                    single_batch = batch.slice(row_idx, 1)
                    sink = pa.BufferOutputStream()
                    with ipc.new_stream(sink, GRID_SCHEMA) as writer:
                        writer.write_batch(single_batch)
                    buffer = sink.getvalue()
                    pipe.set(key, buffer.to_pybytes())
            pipe.execute()
        
        # Deactivate parents
        self._deactivate_grids(levels, global_ids)
        
        return child_keys
    
    def _activate_grids(self, levels: np.ndarray, global_ids: np.ndarray) -> bool:
        """
        Activate multiple grids by adding them to the 'Activate' Set and setting active=True.

        Args:
            levels: Array of level values for the grids to activate.
            global_ids: Array of global IDs for the grids to activate.

        Returns:
            bool: True if all grids were successfully activated, False if any grid does not exist.
        """
        if len(levels) != len(global_ids):
            raise ValueError("Length of levels and global_ids must match")

        keys = [f'{level}-{global_id}' for level, global_id in zip(levels, global_ids)]
        buffers = self._redis_client.mget(keys)
        
        # Check if any grid does not exist
        if any(buffer is None for buffer in buffers):
            return False

        # Prepare updates
        batches = []
        for buffer in buffers:
            reader = ipc.open_stream(buffer)
            batches.append(reader.read_next_batch())
        
        table = pa.Table.from_batches(batches, schema=GRID_SCHEMA)
        df = table.to_pandas()
        
        # Filter to only grids that are not activated (shown=False)
        inactive_mask = ~df[ATTR_ACTIVATE]
        if not inactive_mask.any():
            return True

        df_to_update = df[inactive_mask].copy()
        df_to_update[ATTR_ACTIVATE] = True
        updated_table = pa.Table.from_pandas(df_to_update, schema=GRID_SCHEMA)

        # Get keys for grids that need to be activated
        keys_to_activate = [f'{row[ATTR_LEVEL]}-{row[ATTR_GLOBAL_ID]}' 
                        for _, row in df_to_update.iterrows()]

        # Write updates to Redis and update Activate Set
        with self._redis_client.pipeline() as pipe:
            for batch in updated_table.to_batches():
                for row_idx in range(batch.num_rows):
                    key = f'{batch[ATTR_LEVEL][row_idx].as_py()}-{batch[ATTR_GLOBAL_ID][row_idx].as_py()}'
                    single_batch = batch.slice(row_idx, 1)
                    sink = pa.BufferOutputStream()
                    with ipc.new_stream(sink, GRID_SCHEMA) as writer:
                        writer.write_batch(single_batch)
                    pipe.set(key, sink.getvalue().to_pybytes())
            pipe.sadd(_ACTIVE_SET, *keys_to_activate)
            pipe.execute()
        
        return True
    
    def _deactivate_grids(self, levels: np.ndarray, global_ids: np.ndarray) -> bool:
        """
        Deactivate multiple grids by removing them from the 'Activate' Set and setting shown=False.
        Only grids that are currently activated (shown=True) will be updated.

        Args:
            levels: Array of level values for the grids to deactivate.
            global_ids: Array of global IDs for the grids to deactivate.

        Returns:
            bool: True if all grids were successfully deactivated or already inactive, False if any grid does not exist.
        """
        if len(levels) != len(global_ids):
            raise ValueError("Length of levels and global_ids must match")

        keys = [f'{level}-{global_id}' for level, global_id in zip(levels, global_ids)]
        buffers = self._redis_client.mget(keys)
        
        # Check if any grid does not exist
        if any(buffer is None for buffer in buffers):
            return False

        # Prepare updates
        batches = []
        for buffer in buffers:
            reader = ipc.open_stream(buffer)
            batches.append(reader.read_next_batch())
        
        table = pa.Table.from_batches(batches, schema=GRID_SCHEMA)
        df = table.to_pandas()

        # Filter to only grids that are activated (shown=True)
        active_mask = df[ATTR_ACTIVATE]
        if not active_mask.any():
            return True
        
        df_to_update = df[active_mask].copy()
        df_to_update[ATTR_ACTIVATE] = False
        updated_table = pa.Table.from_pandas(df_to_update, schema=GRID_SCHEMA)

        # Get keys for grids that need to be deactivated
        keys_to_deactivate = [f'{row[ATTR_LEVEL]}-{row[ATTR_GLOBAL_ID]}' 
                            for _, row in df_to_update.iterrows()]

        # Write updates to Redis and update Activate Set
        with self._redis_client.pipeline() as pipe:
            for batch in updated_table.to_batches():
                for row_idx in range(batch.num_rows):
                    key = f'{batch[ATTR_LEVEL][row_idx].as_py()}-{batch[ATTR_GLOBAL_ID][row_idx].as_py()}'
                    single_batch = batch.slice(row_idx, 1)
                    sink = pa.BufferOutputStream()
                    with ipc.new_stream(sink, GRID_SCHEMA) as writer:
                        writer.write_batch(single_batch)
                    pipe.set(key, sink.getvalue().to_pybytes())
            pipe.srem(_ACTIVE_SET, *keys_to_deactivate)
            pipe.execute()
        
        return True
