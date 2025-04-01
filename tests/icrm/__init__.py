import c_two as cc
import pyarrow as pa

class GridAttribute:
    """
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
    def __init__(self, deleted: bool, activate: bool, type: int, level: int, global_id: int, elevation: float,  local_id: int | None = None, min_x: float | None = None, min_y: float | None = None, max_x: float | None = None, max_y: float | None = None):
        self.deleted: bool = deleted
        self.activate: bool = activate
        self.type: int = type
        self.level: int = level
        self.global_id: int = global_id
        self.local_id: int = local_id
        self.elevation: float = elevation
        self.min_x: float = min_x
        self.min_y: float = min_y
        self.max_x: float = max_x
        self.max_y: float = max_y

class ICRM:
    """
    ICRM
    =
    Interface of Core Resource Model (ICRM) specifies how to interact with CRM. 
    """
    
    direction: str # At component side, direction is '->'; at crm side, direction is '<-'

    @cc.transfer(input_name='GridInfos', output_name='GridKeys')
    def get_parent_keys(self, levels: list[int], global_ids: list[int]) -> list[str | None]:
        return

    @cc.transfer(input_name='PeerGridInfos', output_name='GridAttributes')
    def get_grid_infos(self, level: int, global_ids: list[int]) -> list[GridAttribute]:
        return
    
    @cc.transfer(input_name='GridInfos', output_name='GridKeys')
    def subdivide_grids(self, levels: list[int], global_ids: list[int]) -> list[str]:
        return
    
    @cc.transfer(output_name='GridInfos')
    def get_active_grid_infos(self) -> tuple[list[int], list[int]]:
        return
    
# Define transfer methods ##################################################

# GridInfo
def serialize_grid_info(level: int, global_id: int) -> bytes:
    schema = pa.schema([
        pa.field('level', pa.int8()),
        pa.field('global_id', pa.pa.int32())
    ])
    
    data = {
        'level': level,
        'global_id': global_id
    }
    
    table = pa.Table.from_pylist([data], schema=schema)
    return cc.message.serialize_from_table(table)

def deserialize_grid_info(arrow_bytes: bytes) -> tuple[int, int]:
    row = cc.message.deserialize_to_rows(arrow_bytes)[0]
    return (
        row['level'],
        row['global_id']
    )
    
# PeerGridInfos
def serialize_peer_grid_infos(level: int, global_ids: list[int]) -> bytes:
    schema = pa.schema([
        pa.field('level', pa.int8()),
        pa.field('global_ids', pa.list_(pa.int32()))
    ])
    
    data = {
        'level': level,
        'global_ids': global_ids
    }
    
    table = pa.Table.from_pylist([data], schema=schema)
    return cc.message.serialize_from_table(table)

def deserialize_peer_grid_infos(arrow_bytes: bytes) -> tuple[int, list[int]]:
    row = cc.message.deserialize_to_rows(arrow_bytes)[0]
    return (
        row['level'],
        row['global_ids']
    )
    
# GridAttributes
def serialize_grid_attributes(data: list[GridAttribute]) -> bytes:
    schema = pa.schema([
        pa.field('deleted', pa.bool_()),
        pa.field('activate', pa.bool_()),
        pa.field('type', pa.int8()),
        pa.field('level', pa.int8()),
        pa.field('global_id', pa.int32()),
        pa.field('local_id', pa.int32(), nullable=True),
        pa.field('elevation', pa.float64()),
        pa.field('min_x', pa.float64(), nullable=True),
        pa.field('min_y', pa.float64(), nullable=True),
        pa.field('max_x', pa.float64(), nullable=True),
        pa.field('max_y', pa.float64(), nullable=True),
    ])
    
    data_dicts = [
        {
            'deleted': grid.deleted,
            'activate': grid.activate,
            'type': grid.type,
            'level': grid.level,
            'global_id': grid.global_id,
            'local_id': grid.local_id,
            'elevation': grid.elevation,
            'min_x': grid.min_x,
            'min_y': grid.min_y,
            'max_x': grid.max_x,
            'max_y': grid.max_y
        }
        for grid in data
    ]
    
    table = pa.Table.from_pylist(data_dicts, schema=schema)
    return cc.message.serialize_from_table(table)

def deserialize_grid_attributes(arrow_bytes: bytes) -> list[GridAttribute]:
    rows = cc.message.deserialize_to_rows(arrow_bytes)
    grids = [
        GridAttribute(
            deleted=row['deleted'],
            activate=row['activate'],
            type=row['type'],
            level=row['level'],
            global_id=row['global_id'],
            local_id=row['local_id'],
            elevation=row['elevation'],
            min_x=row['min_x'],
            min_y=row['min_y'],
            max_x=row['max_x'],
            max_y=row['max_y']
        )
        for row in rows
    ]
    return grids

# GridInfos
def serialize_grid_infos(levels: list[int], global_ids: list[int]) -> bytes:
    schema = pa.schema([
        pa.field('levels', pa.int8()),
        pa.field('global_ids', pa.int32())
    ])
    table = pa.Table.from_arrays(
        [
            pa.array(levels, type=pa.int8()), 
            pa.array(global_ids, type=pa.int32())
        ],
        schema=schema
    )
    return cc.message.serialize_from_table(table)

def deserialize_grid_infos(arrow_bytes: bytes) -> tuple[list[int], list[list[int]]]:
    table = cc.message.deserialize_to_table(arrow_bytes)
    levels = table.column('levels').to_pylist()
    global_ids = table.column('global_ids').to_pylist()
    return levels, global_ids

# GridKeys
def serialize_grid_keys(keys: list[str]) -> bytes:
    schema = pa.schema([pa.field('keys', pa.string())])
    data = {'keys': keys}
    table = pa.Table.from_pydict(data, schema=schema)
    return cc.message.serialize_from_table(table)

def deserialize_grid_keys(arrow_bytes: bytes) -> list[str]:
    table = cc.message.deserialize_to_table(arrow_bytes)
    keys = table.column('keys').to_pylist()
    return keys

# Register transfer methods ##################################################

cc.message.register_wrapper('GridInfo', serialize_grid_info, deserialize_grid_info)
cc.message.register_wrapper('PeerGridInfos', serialize_peer_grid_infos, deserialize_peer_grid_infos)
cc.message.register_wrapper('GridAttributes', serialize_grid_attributes, deserialize_grid_attributes)
cc.message.register_wrapper('GridInfos', serialize_grid_infos, deserialize_grid_infos)
cc.message.register_wrapper('GridKeys', serialize_grid_keys, deserialize_grid_keys)
    