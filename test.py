from src.test.module import schema, CRM, GridAttribute, Get_Wrapper

# Grid parameters
redis_host = 'localhost'
redis_port = 6379
epsg = 2326
first_size = [64.0, 64.0]
bounds = [808357.5, 824117.5, 838949.5, 843957.5]
subdivide_rules = [
    #    64x64,  32x32,  16x16,    8x8,    4x4,    2x2,    1x1
    [478, 310], [2, 2], [2, 2], [2, 2], [2, 2], [2, 2], [1, 1]
]

if __name__ == '__main__':
    
    crm: CRM
    init_wrapper = Get_Wrapper(schema.InitializeRequest.DESCRIPTOR.full_name)
    init_args = init_wrapper.serialize(redis_host, redis_port, epsg, bounds, first_size, subdivide_rules)
    crm, _ = CRM.create(init_args)
    print(crm.epsg)
    
    query_wrapper = Get_Wrapper(schema.GetGridInfosRequest.DESCRIPTOR.full_name)
    query_args = query_wrapper.serialize(1, [0])
    results, _ = crm.get_grid_infos(query_args)
    res: GridAttribute = results[0]
    print(res.activate, res.deleted, res.level, res.global_id, res.local_id, res.type, res.elevation, res.min_x, res.min_y, res.max_x, res.min_y)
    