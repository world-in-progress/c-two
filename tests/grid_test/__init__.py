from .crm import CRM, GridAttribute
from proto.common import base_pb2 as base
from proto.schema import schema_pb2 as schema
from c_two.wrapper import register_wrapper

# InitParams ##################################################
def forward_init(redis_host: str, redis_port: int, epsg: int, bounds: list[float], first_size: list[float], subdivide_rules: list[list[int]]) -> schema.InitParams:
    proto = schema.InitParams()
    proto.redis_host = redis_host
    proto.redis_port = redis_port
    proto.epsg = epsg
    proto.bounds.extend(bounds)
    proto.first_size.extend(first_size)
    proto.subdivide_rules.extend([schema.SubdivideRule(rule=rule) for rule in subdivide_rules])
    return proto

def inverse_init(proto_bytes: bytes) -> tuple[str, int, int, list[float], list[float], list[list[int]]]:
    proto: schema.InitParams = schema.InitParams()
    proto.ParseFromString(proto_bytes)
    epsg: int = int(proto.epsg)
    redis_host = str(proto.redis_host)
    redis_port = int(proto.redis_port)
    bounds : list[float] = list(proto.bounds)
    first_size: list[float] = list(proto.first_size)
    subdivide_rules: list[list[int]] = [list(rule.rule) for rule in proto.subdivide_rules]
    return redis_host, redis_port, epsg, bounds, first_size, subdivide_rules

# PeerGridInfos ##################################################
def forward_peer_grid_infos(level: int, global_ids: list[int]) -> schema.PeerGridInfos:
    proto = schema.PeerGridInfos()
    proto.level = level
    proto.global_ids.extend(global_ids)
    return proto

def inverse_peer_grid_infos(proto_bytes: bytes) -> tuple[int, list[int]]:
    proto: schema.InitParams = schema.PeerGridInfos()
    proto.ParseFromString(proto_bytes)
    level: int = int(proto.level)
    global_ids: list[int] = list(proto.global_ids)
    return level, global_ids

# GridAttributes ##################################################
def forward_grid_attributes(data: list[GridAttribute]) -> schema.GridAttributes:
    proto = schema.GridAttributes()
    proto.attributes.extend([
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

def inverse_grid_attributes(proto_bytes: bytes) -> tuple[dict[str, base.Code|str], list[GridAttribute]]:
    proto: schema.InitParams = schema.GridAttributes()
    proto.ParseFromString(proto_bytes)
    grid_attributes = [
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
        for grid in proto.attributes
    ]
    return grid_attributes
    
# Register CRM-related wrappers ##################################################
register_wrapper(schema.InitParams, forward_init, inverse_init)
register_wrapper(schema.PeerGridInfos, forward_peer_grid_infos, inverse_peer_grid_infos)
register_wrapper(schema.GridAttributes, forward_grid_attributes, inverse_grid_attributes)
