"""HTTP client for the FastDB-backed Grid CRM.

Usage (3-terminal workflow):

    # Terminal 1 - start the HTTP relay
    c3 relay -b 0.0.0.0:8300

    # Terminal 2 - start the FastDB Grid CRM and register it with the relay
    uv run python examples/python/fastdb_relay_resource.py --relay-url http://127.0.0.1:8300

    # Terminal 3 - run this client
    uv run python examples/python/fastdb_relay_client.py --relay-url http://127.0.0.1:8300
"""
from __future__ import annotations

import argparse
import sys
from pathlib import Path

EXAMPLES_ROOT = Path(__file__).resolve().parent
sys.path.insert(0, str(EXAMPLES_ROOT))

import c_two as cc
from grid.grid_fdb_crm import GridFastdb
from relay_config import ensure_http_relay_url, resolved_relay_url

DEFAULT_RELAY_URL = 'http://127.0.0.1:8300'
ROUTE_NAME = 'examples/grid-fastdb'


def _relay_url(value: str) -> str:
    try:
        return ensure_http_relay_url(value)
    except ValueError as exc:
        raise argparse.ArgumentTypeError(str(exc)) from exc


def parse_args(argv: list[str] | None = None) -> argparse.Namespace:
    parser = argparse.ArgumentParser(
        description='Connect to the FastDB-backed Grid CRM through an HTTP relay.',
    )
    parser.add_argument(
        '--relay-url',
        type=_relay_url,
        default=resolved_relay_url(DEFAULT_RELAY_URL),
        help=(
            'HTTP relay URL used for name resolution '
            f'(default: C2_RELAY_ANCHOR_ADDRESS or {DEFAULT_RELAY_URL}).'
        ),
    )
    args = parser.parse_args(argv)
    try:
        args.relay_url = ensure_http_relay_url(args.relay_url)
    except ValueError as exc:
        parser.error(str(exc))
    return args


def main() -> None:
    args = parse_args()
    cc.set_relay_anchor(args.relay_url)

    grid: GridFastdb = cc.connect(GridFastdb, name=ROUTE_NAME)
    print(f'[FastDB Client] Connected via HTTP (mode: {grid.client._mode})\n')

    schema_rows, rule_rows = grid.get_schema()
    schema = schema_rows[0]
    print(
        '[FastDB Client] Schema: '
        f'epsg={schema.epsg}, bounds=({schema.min_x}, {schema.min_y}, {schema.max_x}, {schema.max_y}), '
        f'first=({schema.first_width}, {schema.first_height}), rules={len(rule_rows)}'
    )

    msg = grid.hello('FastDB relay client')
    print(f'[FastDB Client] hello -> {msg}')

    maybe = grid.none_hello('world')[0]
    print(f'[FastDB Client] none_hello("world") -> is_null={bool(maybe.is_null)}, value={maybe.value!r}')

    infos = grid.get_grid_infos(1, [0])
    attr = infos[0]
    print(
        f'[FastDB Client] Grid 1-0: activate={bool(attr.activate)}, level={attr.level}, '
        f'global_id={attr.global_id}, bounds=({attr.min_x}, {attr.min_y}, {attr.max_x}, {attr.max_y})'
    )

    keys = grid.subdivide_grids([1], [0])
    print(f'[FastDB Client] Subdivided 1-0 -> {len(keys)} children: {keys[:4]}')

    child_ids = [int(key.split('-')[1]) for key in keys]
    children = grid.get_grid_infos(2, child_ids[:4])
    for child in children:
        print(
            f'  Child 2-{child.global_id}: activate={bool(child.activate)}, '
            f'bounds=({child.min_x:.1f}, {child.min_y:.1f}, {child.max_x:.1f}, {child.max_y:.1f})'
        )

    parent_keys = grid.get_parent_keys([2], [child_ids[-1]])
    print(f'[FastDB Client] Parent of 2-{child_ids[-1]} -> {parent_keys[0]}')

    active = grid.get_active_grid_infos()
    print(f'[FastDB Client] Active grids: {len(active)} total')

    with cc.hold(grid.get_active_grid_infos)() as held:
        table = held.value
        columns = held.value.column
        print(
            '[FastDB Client] Held active IDs: '
            f'count={len(table)}, first=({columns.level[0]}, {columns.global_id[0]})'
        )

    cc.close(grid)
    print('\n[FastDB Client] Done.')


if __name__ == '__main__':
    main()
