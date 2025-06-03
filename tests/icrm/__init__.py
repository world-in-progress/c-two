import c_two as cc
import pyarrow as pa

# Define transferables ##################################################

@cc.transferable
class GridSchema:
    """
    Grid Schema
    ---
    - epsg (int): the EPSG code of the grid
    - bounds (list[float]): the bounds of the grid in the format [min_x, min_y, max_x, max_y]
    - first_size (float): the size of the first grid (unit: m)
    - subdivide_rules (list[tuple[int, int]]): the subdivision rules of the grid in the format [(sub_width, sub_height)]
    """
    epsg: int
    bounds: list[float]  # [min_x, min_y, max_x, max_y]
    first_size: float
    subdivide_rules: list[list[int]]  # [(sub_width, sub_height), ...]
        
    def serialize(grid_schema: 'GridSchema') -> bytes:
        arrow_schema = pa.schema([
            pa.field('epsg', pa.int32()),
            pa.field('bounds', pa.list_(pa.float64())),
            pa.field('first_size', pa.float64()),
            pa.field('subdivide_rules', pa.list_(pa.list_(pa.int32())))
        ])
        
        data = {
            'epsg': grid_schema.epsg,
            'bounds': grid_schema.bounds,
            'first_size': grid_schema.first_size,
            'subdivide_rules': grid_schema.subdivide_rules
        }
        
        table = pa.Table.from_pylist([data], schema=arrow_schema)
        return cc.message.serialize_from_table(table)

    def deserialize(arrow_bytes: bytes) -> 'GridSchema':
        row = cc.message.deserialize_to_rows(arrow_bytes)[0]
        return GridSchema(
            epsg=row['epsg'],
            bounds=row['bounds'],
            first_size=row['first_size'],
            subdivide_rules=row['subdivide_rules']
        )

@cc.transferable
class GridAttribute:
    """
    Attributes of Grid
    ---
    - level (int8): the level of the grid
    - type (int8): the type of the grid, default to 0
    - activate (bool), the subdivision status of the grid
    - deleted (bool): the deletion status of the grid, default to False
    - elevation (float64): the elevation of the grid, default to -9999.0
    - global_id (int32): the global id within the bounding box that subdivided by grids all in the level of this grid
    - local_id (int32): the local id within the parent grid that subdivided by child grids all in the level of this grid
    - min_x (float64): the min x coordinate of the grid
    - min_y (float64): the min y coordinate of the grid
    - max_x (float64): the max x coordinate of the grid
    - max_y (float64): the max y coordinate of the grid
    """
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
    
    def serialize(data: 'GridAttribute') -> bytes:
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
        
        table = pa.Table.from_pylist([data.__dict__], schema=schema)
        return cc.message.serialize_from_table(table)
    
    def deserialize(arrow_bytes: bytes) -> 'GridAttribute':
        row = cc.message.deserialize_to_rows(arrow_bytes)[0]
        return GridAttribute(
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

# @cc.transferable
# class GridInfo:
#     def serialize(level: int, global_id: int) -> bytes:
#         schema = pa.schema([
#             pa.field('level', pa.int8()),
#             pa.field('global_id', pa.pa.int32())
#         ])
        
#         data = {
#             'level': level,
#             'global_id': global_id
#         }
        
#         table = pa.Table.from_pylist([data], schema=schema)
#         return cc.message.serialize_from_table(table)

#     def deserialize(arrow_bytes: bytes) -> tuple[int, int]:
#         row = cc.message.deserialize_to_rows(arrow_bytes)[0]
#         return (
#             row['level'],
#             row['global_id']
#         )

# @cc.transferable
# class PeerGridInfos:
#     def serialize(level: int, global_ids: list[int]) -> bytes:
#         schema = pa.schema([
#             pa.field('level', pa.int8()),
#             pa.field('global_ids', pa.list_(pa.int32()))
#         ])
        
#         data = {
#             'level': level,
#             'global_ids': global_ids
#         }
        
#         table = pa.Table.from_pylist([data], schema=schema)
#         return cc.message.serialize_from_table(table)

#     def deserialize(bytes: bytes) -> tuple[int, list[int]]:
#         row = cc.message.deserialize_to_rows(bytes)[0]
#         return (
#             row['level'],
#             row['global_ids']
#         )

# @cc.transferable
# class GridInfos:
#     def serialize(levels: list[int], global_ids: list[int]) -> bytes:
#         schema = pa.schema([
#             pa.field('levels', pa.int8()),
#             pa.field('global_ids', pa.int32())
#         ])
#         table = pa.Table.from_arrays(
#             [
#                 pa.array(levels, type=pa.int8()), 
#                 pa.array(global_ids, type=pa.int32())
#             ],
#             schema=schema
#         )
#         return cc.message.serialize_from_table(table)

#     def deserialize(arrow_bytes: bytes) -> tuple[list[int], list[int]]:
#         table = cc.message.deserialize_to_table(arrow_bytes)
#         levels = table.column('levels').to_pylist()
#         global_ids = table.column('global_ids').to_pylist()
#         return levels, global_ids

# @cc.transferable
# class GridAttributes:
#     def serialize(data: list[GridAttribute]) -> bytes:
#         schema = pa.schema([
#             pa.field('attribute_bytes', pa.list_(pa.binary())),
#         ])

#         data_dict = {
#             'attribute_bytes': [GridAttribute.serialize(grid) for grid in data]
#         }
        
#         table = pa.Table.from_pylist([data_dict], schema=schema)
#         return cc.message.serialize_from_table(table)

#     def deserialize(arrow_bytes: bytes) -> list[GridAttribute]:
#         table = cc.message.deserialize_to_table(arrow_bytes)
        
#         grid_bytes = table.column('attribute_bytes').to_pylist()[0]
        
#         return [GridAttribute.deserialize(grid_byte) for grid_byte in grid_bytes]

# @cc.transferable
# class GridKeys:
#     def serialize(keys: list[str | None]) -> bytes:
#         schema = pa.schema([pa.field('keys', pa.string())])
#         data = {'keys': keys}
#         table = pa.Table.from_pydict(data, schema=schema)
#         return cc.message.serialize_from_table(table)

#     def deserialize(arrow_bytes: bytes) -> list[str | None]:
#         table = cc.message.deserialize_to_table(arrow_bytes)
#         keys = table.column('keys').to_pylist()
#         return keys

# Define ICRM ###########################################################

@cc.icrm
class IGrid:
    """
    IGrid
    =
    Interface of Core Resource Model (ICRM) specifies how to interact with CRM. 
    """
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
    