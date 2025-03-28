from .crm import CRM, GridAttribute
from proto.common import base_pb2 as base
from proto.schema import schema_pb2 as schema
from ..wrapper import Get_Wrapper, Register_Wrapper

# schema.InitializeRequest ##################################################
def serialize_init(redis_host: str, redis_port: int, epsg: int, bounds: list[float], first_size: list[float], subdivide_rules: list[list[int]]) -> schema.InitializeRequest:
    proto = schema.InitializeRequest()
    proto.redis_host = redis_host
    proto.redis_port = redis_port
    proto.epsg = epsg
    proto.bounds.extend(bounds)
    proto.first_size.extend(first_size)
    proto.subdivide_rules.extend([schema.SubdivideRule(rule=rule) for rule in subdivide_rules])
    return proto

def deserialize_init(proto: schema.InitializeRequest) -> tuple[str, int, int, list[float], list[float], list[list[int]]]:
    epsg: int = int(proto.epsg)
    redis_host = str(proto.redis_host)
    redis_port = int(proto.redis_port)
    bounds : list[float] = list(proto.bounds)
    first_size: list[float] = list(proto.first_size)
    subdivide_rules: list[list[int]] = [list(rule.rule) for rule in proto.subdivide_rules]
    return redis_host, redis_port, epsg, bounds, first_size, subdivide_rules

# schema.GetGridInfosRequest ##################################################
def serialize_sibling_grid_infos(level: int, global_ids: list[int]) -> schema.GetGridInfosRequest:
    proto = schema.GetGridInfosRequest()
    proto.level = level
    proto.global_ids.extend(global_ids)
    return proto

def deserialize_sibling_grid_infos(args: schema.GetGridInfosRequest) -> tuple[int, list[int]]:
    level: int = int(args.level)
    global_ids: list[int] = list(args.global_ids)
    return level, global_ids

# schema.GetGridInfosResponse ##################################################
def serialize_multi_grid_attributes(status: base.Status, message: str, data: list[GridAttribute]) -> schema.GetGridInfosResponse:
    proto = schema.GetGridInfosResponse()
    proto.status = status
    proto.message = message
    proto.grid_infos.extend([
        schema.GridAttribute(
            deleted=grid.deleted,
            activate=grid.activate,
            type=grid.type,
            level=grid.level,
            global_id=grid.global_id,
            elevation=grid.elevation,
            local_id=grid.local_id,
            min_x=grid.min_x,
            min_y=grid.min_y,
            max_x=grid.max_x,
            max_y=grid.max_y
        )
        for grid in data
    ])
    return proto

def deserialize_multi_grid_attributes(proto: schema.GetGridInfosResponse) -> tuple[base.Status, str, list[GridAttribute]]:
    status = proto.status
    message = proto.message
    grid_infos = [
        GridAttribute(
            deleted=grid.deleted,
            activate=grid.activate,
            type=grid.type,
            level=grid.level,
            global_id=grid.global_id,
            elevation=grid.elevation,
            local_id=grid.local_id,
            min_x=grid.min_x,
            min_y=grid.min_y,
            max_x=grid.max_x,
            max_y=grid.max_y
        )
        for grid in proto.grid_infos
    ]
    return status, message, grid_infos
    
# Register CRM-related wrappers ##################################################
Register_Wrapper(schema.InitializeRequest.DESCRIPTOR.full_name, serialize_init, deserialize_init)
Register_Wrapper(schema.GetGridInfosRequest.DESCRIPTOR.full_name, serialize_sibling_grid_infos, deserialize_sibling_grid_infos)
Register_Wrapper(schema.GetGridInfosResponse.DESCRIPTOR.full_name, serialize_multi_grid_attributes, deserialize_multi_grid_attributes)
