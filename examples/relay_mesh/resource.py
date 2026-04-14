"""Grid CRM process — auto-registers with the relay.

A self-contained Grid resource example demonstrating relay mesh discovery.
Setting C2_RELAY_ADDRESS (or calling cc.set_relay()) makes cc.register()
announce the resource to the relay server, enabling name-based discovery.

Run (after starting relay.py):
    uv run python examples/relay_mesh/resource.py
"""
import os, sys

sys.path.insert(0, os.path.abspath(os.path.join(os.path.dirname(__file__), '../../src/')))

import c_two as cc
import logging

logging.basicConfig(
    level=logging.INFO,
    format='%(asctime)s [resource] %(levelname)s %(message)s',
    datefmt='%H:%M:%S',
)

RELAY_URL = 'http://127.0.0.1:8300'


# ── ICRM interface ──────────────────────────────────────────────────
# Shared between CRM and client.  Uses only built-in types so pickle
# serialization works across processes without module-path issues.

@cc.icrm(namespace='demo.grid', version='0.1.0')
class IGrid:
    @cc.read
    def get_schema(self) -> dict: ...

    @cc.read
    def get_cells(self, level: int, global_ids: list[int]) -> list[dict]: ...

    def subdivide(self, level: int, global_id: int) -> list[str]: ...

    @cc.read
    def active_count(self) -> int: ...

    @cc.read
    def hello(self, name: str) -> str: ...


# ── CRM implementation ──────────────────────────────────────────────

class Grid:
    """A simple 2D hierarchical grid covering a rectangular region."""

    def __init__(
        self,
        epsg: int,
        bounds: list[float],
        cell_size: list[float],
        subdivide_rules: list[list[int]],
    ):
        self.epsg = epsg
        self.bounds = bounds
        self.cell_size = cell_size
        self.subdivide_rules = subdivide_rules

        # Compute grid dimensions per level.
        self.level_dims: list[tuple[int, int]] = [(1, 1)]
        for rule in subdivide_rules[:-1]:
            pw, ph = self.level_dims[-1]
            self.level_dims.append((pw * rule[0], ph * rule[1]))

        # Active cell storage: {(level, global_id): activate}
        w, h = self.level_dims[1]
        self._cells: dict[tuple[int, int], bool] = {
            (1, gid): True for gid in range(w * h)
        }

    def hello(self, name: str) -> str:
        return f'Hello from Grid, {name}!'

    def get_schema(self) -> dict:
        return {
            'epsg': self.epsg,
            'bounds': self.bounds,
            'cell_size': self.cell_size,
            'subdivide_rules': self.subdivide_rules,
            'level_count': len(self.subdivide_rules),
        }

    def get_cells(self, level: int, global_ids: list[int]) -> list[dict]:
        bx0, by0, bx1, by1 = self.bounds
        w, h = self.level_dims[level]
        result = []
        for gid in global_ids:
            active = self._cells.get((level, gid))
            if active is None:
                continue
            cx = gid % w
            cy = gid // w
            result.append({
                'level': level,
                'global_id': gid,
                'activate': active,
                'min_x': bx0 + (bx1 - bx0) * cx / w,
                'min_y': by0 + (by1 - by0) * cy / h,
                'max_x': bx0 + (bx1 - bx0) * (cx + 1) / w,
                'max_y': by0 + (by1 - by0) * (cy + 1) / h,
            })
        return result

    def subdivide(self, level: int, global_id: int) -> list[str]:
        key = (level, global_id)
        if key not in self._cells or not self._cells[key]:
            return []

        self._cells[key] = False
        child_level = level + 1
        sw, sh = self.subdivide_rules[level]
        w = self.level_dims[level][0]
        cx, cy = global_id % w, global_id // w
        cw = w * sw

        children = []
        for ly in range(sh):
            for lx in range(sw):
                child_gid = (cy * sh + ly) * cw + (cx * sw + lx)
                self._cells[(child_level, child_gid)] = True
                children.append(f'{child_level}-{child_gid}')
        return children

    def active_count(self) -> int:
        return sum(1 for v in self._cells.values() if v)


# ── Main ────────────────────────────────────────────────────────────

def main():
    cc.set_relay(RELAY_URL)

    grid = Grid(
        epsg=2326,
        bounds=[808357.5, 824117.5, 838949.5, 843957.5],
        cell_size=[64.0, 64.0],
        subdivide_rules=[[4, 3], [2, 2], [2, 2], [2, 2]],
    )

    cc.register(IGrid, grid, name='grid')
    print(f'Grid CRM registered (IPC: {cc.server_address()})')
    print(f'Relay: {RELAY_URL}')
    print('Press Ctrl-C to stop.\n')

    cc.serve()


if __name__ == '__main__':
    main()
