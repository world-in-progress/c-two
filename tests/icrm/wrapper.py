import c_two as cc
import pyarrow as pa
from . import GridSchema as GS, GridAttribute

# Define transfer wrappers ##################################################

class GridSchema(cc.AbstractWrapper):
    def serialize(schema: GS) -> bytes:
        schema = pa.schema([
            pa.field('epsg', pa.int32()),
            pa.field('bounds', pa.list_(pa.float64())),
            pa.field('first_size', pa.float64()),
            pa.field('subdivide_rules', pa.list_(pa.list_(pa.int32())))
        ])
        
        data = {
            'epsg': schema.epsg,
            'bounds': schema.bounds,
            'first_size': schema.first_size,
            'subdivide_rules': schema.subdivide_rules
        }
        
        table = pa.Table.from_pylist([data], schema=schema)
        return cc.message.serialize_from_table(table)

    def deserialize(arrow_bytes: bytes) -> GS:
        row = cc.message.deserialize_to_rows(arrow_bytes)[0]
        return GridSchema(
            epsg=row['epsg'],
            bounds=row['bounds'],
            first_size=row['first_size'],
            subdivide_rules=row['subdivide_rules']
        )

class GridInfo(cc.AbstractWrapper):
    def serialize(level: int, global_id: int) -> bytes:
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

    def deserialize(arrow_bytes: bytes) -> tuple[int, int]:
        row = cc.message.deserialize_to_rows(arrow_bytes)[0]
        return (
            row['level'],
            row['global_id']
        )

class PeerGridInfos(cc.AbstractWrapper):
    def serialize(level: int, global_ids: list[int]) -> bytes:
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

    def deserialize(bytes: bytes) -> tuple[int, list[int]]:
        row = cc.message.deserialize_to_rows(bytes)[0]
        return (
            row['level'],
            row['global_ids']
        )

class GridInfos(cc.AbstractWrapper):
    def serialize(levels: list[int], global_ids: list[int]) -> bytes:
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

    def deserialize(arrow_bytes: bytes) -> tuple[list[int], list[list[int]]]:
        table = cc.message.deserialize_to_table(arrow_bytes)
        levels = table.column('levels').to_pylist()
        global_ids = table.column('global_ids').to_pylist()
        return levels, global_ids

class GridAttributes(cc.AbstractWrapper):
    def serialize(data: list[GridAttribute]) -> bytes:
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

    def deserialize(arrow_bytes: bytes) -> list[GridAttribute]:
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

class GridKeys(cc.AbstractWrapper):
    def serialize(keys: list[str]) -> bytes:
        schema = pa.schema([pa.field('keys', pa.string())])
        data = {'keys': keys}
        table = pa.Table.from_pydict(data, schema=schema)
        return cc.message.serialize_from_table(table)

    def deserialize(arrow_bytes: bytes) -> list[str]:
        table = cc.message.deserialize_to_table(arrow_bytes)
        keys = table.column('keys').to_pylist()
        return keys