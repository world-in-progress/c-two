import json
import redis
import numpy as np
import pandas as pd
import pyarrow as pa
import pyarrow.ipc as ipc
import pyarrow.compute as pc
from ..wrapper import cc_wrapper
from ..proto import schema_pb2 as schema

# Const ##############################
GRID_DEFINITION = 'grid_definition'

ACTIVE_SET = 'Active'

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

GRID_SCHEMA = pa.schema([
    (ATTR_DELETED, pa.bool_()),
    (ATTR_ACTIVATE, pa.bool_()), 
    (ATTR_TYPE, pa.int8()),
    (ATTR_LEVEL, pa.int8()),
    (ATTR_GLOBAL_ID, pa.int32()),
    (ATTR_ELEVATION, pa.float64())
])

class GridAttribute:
    def __init__(self, deleted: bool, activate: bool, type: int, level: int, global_id: int, elevation: float,  local_id: int | None = None, min_x: float | None = None, min_y: float | None = None, max_x: float | None = None, max_y: float | None = None):
        self.deleted = deleted
        self.activate = activate
        self.type = type
        self.level = level
        self.global_id = global_id
        self.local_id = local_id
        self.elevation = elevation
        self.min_x = min_x
        self.min_y = min_y
        self.max_x = max_x
        self.max_y = max_y

# CRM ##################################################

class CRM():
    """ 
    CRM
    =
    The Core Resoruce Model (CRM) of `crm-nh-grid<https://github.com/world-in-progress/crm-nh-grid>`_  is a 2D grid system that can be subdivided into smaller grids by pre-declared subdivide rules.  
    """
    def __init__(self, redis_host: str, redis_port: int, epsg: int, bounds: list, first_size: list[float], subdivide_rules: list[list[int]]):
        self.redis_client = redis.Redis(host=redis_host, port=redis_port, decode_responses=False)
        
        # Check if grid definition exists in Redis
        if not self.redis_client.exists(GRID_DEFINITION):
            # Calculate level infos
            level_info: list[dict[str, int]] = [{
                'width': 1,
                'height': 1
            }]
            for level, rule in enumerate(subdivide_rules):
                if level == len(subdivide_rules) - 1: 
                    break
                sub_width: int = level_info[level]['width'] * rule[0]
                sub_height: int = level_info[level]['height'] * rule[1]
                level_info.append({
                    'width': sub_width,
                    'height': sub_height
                })
            
            # Store grid definition in Redis
            grid_definition = {
                'epsg': epsg,
                'bounds': bounds,
                'first_size': first_size,
                'level_info': level_info,
                'subdivide_rules': subdivide_rules
            }
            self.redis_client.set(GRID_DEFINITION, json.dumps(grid_definition).encode('utf-8'))
            
            # Initialize grid data (ONLY Level 1) as PyArrow Table
            with self.redis_client.pipeline() as pipe:
                level = 1
                total_width = level_info[level]['width']
                total_height = level_info[level]['height']
                num_grids = total_width * total_height
                
                data = {
                    ATTR_ACTIVATE: np.full(num_grids, True),
                    ATTR_DELETED: np.full(num_grids, False, dtype=np.bool),
                    ATTR_TYPE: np.zeros(num_grids, dtype=np.int8),
                    ATTR_LEVEL: np.full(num_grids, level, dtype=np.int8),
                    ATTR_GLOBAL_ID: np.arange(num_grids, dtype=np.int32),
                    ATTR_ELEVATION: np.full(num_grids, -9999.0, dtype=np.float64)
                }
                
                keys_to_activate = []
                batches = pa.Table.from_pydict(data, schema=GRID_SCHEMA).to_batches(max_chunksize=1000)
                for batch in batches:
                    for row_idx in range(batch.num_rows):
                        key = f'{level}-{batch[ATTR_GLOBAL_ID][row_idx].as_py()}'
                        keys_to_activate.append(key)
                        
                        single_batch = batch.slice(row_idx, 1)
                        sink = pa.BufferOutputStream()
                        with ipc.new_stream(sink, GRID_SCHEMA) as writer:
                            writer.write_batch(single_batch)
                        buffer = sink.getvalue()
                        pipe.set(key, buffer.to_pybytes())
                
                pipe.sadd('Activate', *keys_to_activate)
                pipe.execute()

        # Load grid definition from Redis
        grid_definition_bytes = self.redis_client.get(GRID_DEFINITION)
        grid_definition = json.loads(grid_definition_bytes.decode('utf-8'))
        self.epsg: int = grid_definition['epsg']
        self.bounds: list = grid_definition['bounds']
        self.first_size: list[float] = grid_definition['first_size']
        self.level_info: list[dict[str, int]] = grid_definition['level_info']
        self.subdivide_rules: list[list[int]] = grid_definition['subdivide_rules']
    
    @staticmethod
    @cc_wrapper(input_schema=schema.InitializeRequest.DESCRIPTOR.full_name, static=True)
    def create(redis_host: str, redis_port: int, epsg: int, bounds: list, first_size: list[float], subdivide_rules: list[list[int]]) -> 'CRM':
        return CRM(redis_host, redis_port, epsg, bounds, first_size, subdivide_rules)
            
    def get_local_ids(self, level: int, global_ids: np.ndarray) -> np.ndarray:
        if level == 0:
            return global_ids
        total_width = self.level_info[level]['width']
        sub_width = self.subdivide_rules[level - 1][0]
        sub_height = self.subdivide_rules[level - 1][1]
        local_x = global_ids % total_width
        local_y = global_ids // total_width
        return (((local_y % sub_height) * sub_width) + (local_x % sub_width))
    
    def get_coordinates(self, level: int, global_ids: np.ndarray) -> tuple[np.ndarray, np.ndarray, np.ndarray, np.ndarray]:
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

    def get_grid_info(self, level: int, global_id: int) -> pd.DataFrame:
        key = f'{level}-{global_id}'
        buffer = self.redis_client.get(key)
        if buffer is None:
            return pd.DataFrame(columns=[ATTR_LEVEL, ATTR_GLOBAL_ID, ATTR_LOCAL_ID, ATTR_TYPE, 
                                       ATTR_ELEVATION, ATTR_DELETED, ATTR_ACTIVATE, 
                                       ATTR_MIN_X, ATTR_MIN_Y, ATTR_MAX_X, ATTR_MAX_Y])
        
        reader = ipc.open_stream(buffer)
        grid_record = reader.read_next_batch()
        table = pa.Table.from_batches([grid_record], schema=GRID_SCHEMA)
        df = table.to_pandas(use_threads=True)
        
        # Calculate computed attributes
        local_id = self.get_local_ids(level, np.array([global_id]))[0]
        min_x, min_y, max_x, max_y = self.get_coordinates(level, np.array([global_id]))
        df[ATTR_LOCAL_ID] = local_id
        df[ATTR_MIN_X] = min_x
        df[ATTR_MIN_Y] = min_y
        df[ATTR_MAX_X] = max_x
        df[ATTR_MAX_Y] = max_y
        
        column_order = [ATTR_LEVEL, ATTR_GLOBAL_ID, ATTR_LOCAL_ID, ATTR_TYPE, ATTR_ELEVATION, 
                        ATTR_DELETED, ATTR_ACTIVATE, ATTR_MIN_X, ATTR_MIN_Y, ATTR_MAX_X, ATTR_MAX_Y]
        return df[column_order]
    
    @cc_wrapper(input_schema=schema.GetGridInfosRequest.DESCRIPTOR.full_name, output_schema=schema.GetGridInfosResponse.DESCRIPTOR.full_name)
    def get_grid_infos(self, level: int, global_ids: np.ndarray) -> list[GridAttribute]:
        keys = [f'{level}-{global_id}' for global_id in global_ids]
        buffer_list = self.redis_client.mget(keys)
        
        grid_records = []
        for buffer in buffer_list:
            if buffer:
                reader = ipc.open_stream(buffer)
                grid_records.append(reader.read_next_batch())
                
        if grid_records is None:
            return pd.DataFrame(columns=[ATTR_LEVEL, ATTR_GLOBAL_ID, ATTR_LOCAL_ID, ATTR_TYPE, 
                                       ATTR_ELEVATION, ATTR_DELETED, ATTR_ACTIVATE, 
                                       ATTR_MIN_X, ATTR_MIN_Y, ATTR_MAX_X, ATTR_MAX_Y]) 
        # Filter table by global_ids
        table = pa.Table.from_batches(grid_records, schema=GRID_SCHEMA)
        df = table.to_pandas(use_threads=True)
            
        # Calculate computed attributes
        global_ids_array = df[ATTR_GLOBAL_ID].to_numpy(dtype=np.int32)
        local_ids = self.get_local_ids(level, global_ids_array)
        min_xs, min_ys, max_xs, max_ys = self.get_coordinates(level, global_ids_array)
        df[ATTR_LOCAL_ID] = local_ids
        df[ATTR_MIN_X] = min_xs
        df[ATTR_MIN_Y] = min_ys
        df[ATTR_MAX_X] = max_xs
        df[ATTR_MAX_Y] = max_ys
        
        return [
            GridAttribute(
                deleted=row.deleted,
                activate=row.activate,
                type=row.type,
                level=row.level,
                global_id=row.global_id,
                local_id=row.local_id,
                elevation=row.elevation,
                min_x=row.min_x,
                min_y=row.min_y,
                max_x=row.max_x,
                max_y=row.max_y
            )
            for row in df.itertuples(index=False)
        ]
    
    def get_grid_children(self, level: int, global_id: int) -> np.ndarray | None:
        if (level < 0) or (level >= len(self.level_info)):
            return None
        
        width = self.level_info[level]['width']
        global_u = global_id % width
        global_v = global_id // width
        sub_width = self.subdivide_rules[level][0]
        sub_height = self.subdivide_rules[level][1]
        sub_count = sub_width * sub_height
        
        baseGlobalWidth = width * sub_width
        child_global_ids = np.ndarray(sub_count, dtype=np.int32)
        for local_id in range(sub_count):
            local_u = local_id % sub_width
            local_v = local_id // sub_width
            
            sub_global_u = global_u * sub_width + local_u
            sub_global_v = global_v * sub_height + local_v
            child_global_ids[local_id] = sub_global_v * baseGlobalWidth + sub_global_u
        
        return child_global_ids
    
    def subdivide_grids(self, levels: np.ndarray, global_ids: np.ndarray, batch_size: int = 1000) -> list[str]:
        def get_grid_children(row) -> tuple[np.ndarray, np.ndarray] | None:
            level = row[ATTR_LEVEL]
            global_id = row[ATTR_GLOBAL_ID]
            child_global_ids = self.get_grid_children(level, global_id)
            child_levels = np.full(child_global_ids.size, level + 1, dtype=np.int8)
            
            return (child_levels, child_global_ids)
        
        # Get parents data frame
        parent_keys: list[str] = []
        for level, global_id in zip(levels, global_ids):
            parent_keys.append(f'{level}-{global_id}')
        parent_buffers = self.redis_client.mget(parent_keys)
        
        parent_batches = []
        for buffer in parent_buffers:
            if buffer:
                reader = ipc.open_stream(buffer)
                parent_batches.append(reader.read_next_batch())
                
        mask = (pc.field(ATTR_DELETED) == False) & (pc.field(ATTR_ACTIVATE) == True)
        parent_df: pd.DataFrame = pa.Table.from_batches(parent_batches, schema=GRID_SCHEMA).filter(mask).to_pandas()
        
        # Get all child infos
        child_info_series: pd.DataFrame = parent_df.apply(get_grid_children, axis=1)
        if child_info_series.empty:
            return []
        
        # Create children table
        child_levels, child_global_ids = [info for infos in child_info_series for info in infos]
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
        with self.redis_client.pipeline() as pipe:
            batches = child_table.to_batches(max_chunksize=batch_size)
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
        buffers = self.redis_client.mget(keys)
        
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
        with self.redis_client.pipeline() as pipe:
            for batch in updated_table.to_batches():
                for row_idx in range(batch.num_rows):
                    key = f'{batch[ATTR_LEVEL][row_idx].as_py()}-{batch[ATTR_GLOBAL_ID][row_idx].as_py()}'
                    single_batch = batch.slice(row_idx, 1)
                    sink = pa.BufferOutputStream()
                    with ipc.new_stream(sink, GRID_SCHEMA) as writer:
                        writer.write_batch(single_batch)
                    pipe.set(key, sink.getvalue().to_pybytes())
            pipe.sadd('Activate', *keys_to_activate)
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
        buffers = self.redis_client.mget(keys)
        
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
        with self.redis_client.pipeline() as pipe:
            for batch in updated_table.to_batches():
                for row_idx in range(batch.num_rows):
                    key = f'{batch[ATTR_LEVEL][row_idx].as_py()}-{batch[ATTR_GLOBAL_ID][row_idx].as_py()}'
                    single_batch = batch.slice(row_idx, 1)
                    sink = pa.BufferOutputStream()
                    with ipc.new_stream(sink, GRID_SCHEMA) as writer:
                        writer.write_batch(single_batch)
                    pipe.set(key, sink.getvalue().to_pybytes())
            pipe.srem('Activate', *keys_to_deactivate)
            pipe.execute()
        
        return True
            
# Helpers ##############################
def lerp(a, b, t):
    return a + (b - a) * t