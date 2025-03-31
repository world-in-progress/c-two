import c_two as cc
from icrm import ICRM, GridAttribute
from proto.execute import execute_pb2 as execute, execute_pb2_grpc as execute_grpc

@cc.connect(ICRM)
def component_func(crm_conn: ICRM, args: execute.PeerGridInfos):
    grids: list[GridAttribute] = crm_conn.get_grid_infos(args.level, [global_id for global_id in args.global_ids])
    result = execute.GridAttributes()
    result.attributes.extend([
        execute.GridAttribute(
            deleted=grid.deleted,
            activate=grid.activate,
            type=grid.type,
            level=grid.level,
            global_id=grid.global_id,
            elevation=grid.elevation,
            min_x=grid.min_x,
            min_y=grid.min_y,
            max_x=grid.max_x,
            max_y=grid.max_y,
            local_id=grid.local_id
        )
        for grid in grids
    ])
    return result

class Component(execute_grpc.ComponentServiceServicer):
    
    def Execute(self, request: execute.ComponentRequest, context):
        return component_func(request.peerGridInfos, crm_address=request.crm_address)
    
    @staticmethod
    def register_to_server(server):
        execute_grpc.add_ComponentServiceServicer_to_server(Component(), server)
