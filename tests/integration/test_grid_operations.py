import pytest
import c_two as cc
from examples.grid.icrm import IGrid, GridAttribute

pytestmark = pytest.mark.timeout(30)


class TestGridBasicOperations:
    def test_hello(self, grid_server):
        with cc.compo.runtime.connect_crm(grid_server, IGrid) as grid:
            assert grid.hello('World') == 'Hello, World!'

    def test_none_hello_returns_none(self, grid_server):
        with cc.compo.runtime.connect_crm(grid_server, IGrid) as grid:
            assert grid.none_hello('world') is None

    def test_none_hello_returns_value(self, grid_server):
        with cc.compo.runtime.connect_crm(grid_server, IGrid) as grid:
            result = grid.none_hello('test')
            assert result == 'Hello, test!'

    def test_get_schema(self, grid_server):
        with cc.compo.runtime.connect_crm(grid_server, IGrid) as grid:
            schema = grid.get_schema()
            assert schema.epsg == 2326
            assert len(schema.bounds) == 4
            assert schema.bounds[0] == pytest.approx(808357.5)
            assert schema.bounds[1] == pytest.approx(824117.5)
            assert schema.bounds[2] == pytest.approx(838949.5)
            assert schema.bounds[3] == pytest.approx(843957.5)
            assert schema.first_size == pytest.approx([64.0, 64.0])
            assert len(schema.subdivide_rules) > 0

    def test_get_grid_infos(self, grid_server):
        with cc.compo.runtime.connect_crm(grid_server, IGrid) as grid:
            infos = grid.get_grid_infos(1, [0])
            assert len(infos) == 1
            info = infos[0]
            assert isinstance(info, GridAttribute)
            assert info.level == 1
            assert info.global_id == 0
            assert info.activate is True
            assert info.deleted is False

    def test_get_active_grid_infos(self, grid_server):
        with cc.compo.runtime.connect_crm(grid_server, IGrid) as grid:
            levels, global_ids = grid.get_active_grid_infos()
            assert len(levels) > 0
            assert len(levels) == len(global_ids)
            # All returned grids should be at level 1 initially
            assert all(lvl == 1 for lvl in levels)


class TestGridSubdivision:
    def test_subdivide_returns_child_keys(self, grid_server):
        with cc.compo.runtime.connect_crm(grid_server, IGrid) as grid:
            keys = grid.subdivide_grids([1], [0])
            assert len(keys) > 0
            for key in keys:
                assert '-' in key  # format: "level-global_id"

    def test_subdivide_deactivates_parent(self, grid_server):
        with cc.compo.runtime.connect_crm(grid_server, IGrid) as grid:
            grid.subdivide_grids([1], [0])
            parent = grid.get_grid_infos(1, [0])[0]
            assert parent.activate is False

    def test_children_are_active_after_subdivide(self, grid_server):
        with cc.compo.runtime.connect_crm(grid_server, IGrid) as grid:
            keys = grid.subdivide_grids([1], [0])
            child_level = int(keys[0].split('-')[0])
            child_ids = [int(k.split('-')[1]) for k in keys]
            children = grid.get_grid_infos(child_level, child_ids)
            assert len(children) == len(keys)
            assert all(c.activate is True for c in children)

    def test_get_parent_keys_after_subdivide(self, grid_server):
        with cc.compo.runtime.connect_crm(grid_server, IGrid) as grid:
            keys = grid.subdivide_grids([1], [0])
            child_levels = [int(k.split('-')[0]) for k in keys]
            child_ids = [int(k.split('-')[1]) for k in keys]
            parent_keys = grid.get_parent_keys(child_levels, child_ids)
            for pk in parent_keys:
                if pk is not None:
                    assert pk.startswith('1-')  # parent is at level 1

    def test_combined_subdivide_flow(self, grid_server):
        """Subdivide → get infos on children → verify children active, parent deactivated."""
        with cc.compo.runtime.connect_crm(grid_server, IGrid) as grid:
            # Get initial active grids
            init_levels, init_ids = grid.get_active_grid_infos()
            assert 0 in init_ids

            # Subdivide grid at level 1, global_id 0
            keys = grid.subdivide_grids([1], [0])
            assert len(keys) > 0

            # Parent should be deactivated
            parent = grid.get_grid_infos(1, [0])[0]
            assert parent.activate is False

            # Children should be active
            child_level = int(keys[0].split('-')[0])
            child_ids = [int(k.split('-')[1]) for k in keys]
            children = grid.get_grid_infos(child_level, child_ids)
            assert all(c.activate is True for c in children)
            assert all(c.level == child_level for c in children)

            # Active grids should now include children, not the parent at id 0
            new_levels, new_ids = grid.get_active_grid_infos()
            assert len(new_levels) > 0
