import c_two as cc
import pyarrow as pa

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
    first_size: list[float]  # [width, height]
    subdivide_rules: list[list[int]]  # [(sub_width, sub_height), ...]
        
    def serialize(grid_schema: 'GridSchema') -> bytes:
        arrow_schema = pa.schema([
            pa.field('epsg', pa.int32()),
            pa.field('bounds', pa.list_(pa.float64())),
            pa.field('first_size', pa.list_(pa.float64())),
            pa.field('subdivide_rules', pa.list_(pa.list_(pa.int32())))
        ])
        
        data = {
            'epsg': grid_schema.epsg,
            'bounds': grid_schema.bounds,
            'first_size': grid_schema.first_size,
            'subdivide_rules': grid_schema.subdivide_rules
        }
        
        table = pa.Table.from_pylist([data], schema=arrow_schema)
        return serialize_from_table(table)

    def deserialize(arrow_bytes: bytes) -> 'GridSchema':
        row = deserialize_to_rows(arrow_bytes)[0]
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
        return serialize_from_table(table)
    
    def deserialize(arrow_bytes: bytes) -> 'GridAttribute':
        row = deserialize_to_rows(arrow_bytes)[0]
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

# Helpers ##################################################


def serialize_from_table(table: pa.Table) -> bytes:
    sink = pa.BufferOutputStream()
    with pa.ipc.new_stream(sink, table.schema) as writer:
        writer.write_table(table)
    binary_data = sink.getvalue().to_pybytes()
    return binary_data

def deserialize_to_table(serialized_data: bytes) -> pa.Table:
    buffer = pa.py_buffer(serialized_data)
    with pa.ipc.open_stream(buffer) as reader:
        table = reader.read_all()
    return table

def deserialize_to_rows(serialized_data: bytes) -> dict:
    buffer = pa.py_buffer(serialized_data)

    with pa.ipc.open_stream(buffer) as reader:
        table = reader.read_all()

    return table.to_pylist()