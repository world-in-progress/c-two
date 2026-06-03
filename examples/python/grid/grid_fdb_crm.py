import c_two as cc
import fastdb4py as fdb


@fdb.feature
class GridId:
    level: fdb.I32
    global_id: fdb.I32


@fdb.feature
class FastdbGridSchema:
    epsg: fdb.I32
    min_x: fdb.F64
    min_y: fdb.F64
    max_x: fdb.F64
    max_y: fdb.F64
    first_width: fdb.F64
    first_height: fdb.F64


@fdb.feature
class FastdbGridRule:
    level: fdb.I32
    width: fdb.I32
    height: fdb.I32


@fdb.feature
class FastdbGridAttribute:
    level: fdb.I32
    type: fdb.I32
    activate: fdb.BOOL
    global_id: fdb.I32
    deleted: fdb.BOOL
    elevation: fdb.F64
    local_id: fdb.I32
    min_x: fdb.F64
    min_y: fdb.F64
    max_x: fdb.F64
    max_y: fdb.F64


@fdb.feature
class FastdbMaybeText:
    is_null: fdb.BOOL
    value: fdb.WSTR


@cc.crm(namespace='demo.grid.fastdb', version='0.1.0')
class GridFastdb:
    """FastDB-first portable CRM contract for the grid resource."""

    def get_schema(self) -> tuple[fdb.Batch[FastdbGridSchema], fdb.Batch[FastdbGridRule]]:
        ...

    def subdivide_grids(self, levels: fdb.Array[fdb.I32], global_ids: fdb.Array[fdb.I32]) -> fdb.Array[fdb.WSTR]:
        ...

    def get_parent_keys(self, levels: fdb.Array[fdb.I32], global_ids: fdb.Array[fdb.I32]) -> fdb.Array[fdb.WSTR]:
        ...

    def get_grid_infos(self, level: fdb.I32, global_ids: fdb.Array[fdb.I32]) -> fdb.Batch[FastdbGridAttribute]:
        ...

    def get_active_grid_infos(self) -> fdb.Batch[GridId]:
        ...

    def hello(self, name: fdb.WSTR) -> fdb.WSTR:
        ...

    def none_hello(self, message: fdb.WSTR) -> fdb.Batch[FastdbMaybeText]:
        ...
