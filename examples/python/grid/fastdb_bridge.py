import c_two as cc
from c_two.fastdb import bridge as fdb_bridge

from grid.grid_fdb_crm import FastdbGridRule, FastdbGridSchema, FastdbMaybeText, GridFastdb
from grid.nested_grid import NestedGrid


def grid_fastdb_bridge() -> dict[str, cc.ResourceBridge]:
    bridges = fdb_bridge.derive_c_two_bridge(
        cc,
        GridFastdb,
        NestedGrid,
        methods=['get_grid_infos', 'get_active_grid_infos', 'hello'],
    )
    bridges['get_schema'] = cc.bridge(output=_schema_output)
    bridges['subdivide_grids'] = cc.bridge(input=_two_int_arrays_input, output=_string_list_output)
    bridges['get_parent_keys'] = cc.bridge(input=_two_int_arrays_input, output=_string_list_output)
    bridges['none_hello'] = cc.bridge(input=_text_input, output=_maybe_text_output)
    return bridges


def _two_int_arrays_input(levels, global_ids):
    return [int(value) for value in levels], [int(value) for value in global_ids]


def _text_input(value):
    return str(value)


def _schema_output(schema):
    bounds = [float(value) for value in schema.bounds]
    first_size = [float(value) for value in schema.first_size]
    rules = [
        FastdbGridRule(level=index, width=int(rule[0]), height=int(rule[1]))
        for index, rule in enumerate(schema.subdivide_rules)
    ]
    return (
        [
            FastdbGridSchema(
                epsg=int(schema.epsg),
                min_x=bounds[0],
                min_y=bounds[1],
                max_x=bounds[2],
                max_y=bounds[3],
                first_width=first_size[0],
                first_height=first_size[1],
            ),
        ],
        rules,
    )


def _string_list_output(values):
    return ['' if value is None else str(value) for value in values]


def _maybe_text_output(value):
    return [
        FastdbMaybeText(
            is_null=value is None,
            value='' if value is None else str(value),
        ),
    ]
