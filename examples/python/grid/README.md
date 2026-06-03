# Grid Examples

`grid_py_crm.py` is the Python fallback CRM path for the grid resource. It defines `GridPython` plus the Python-only `GridSchema` and `GridAttribute` dataclasses returned by `NestedGrid`, so resource processes can register `NestedGrid` directly and let pickle fallback carry those objects across local Python transports. This path is intentionally rejected by strict portable export/codegen and is useful for proving local Python ergonomics before committing to a portable ABI.

`grid_fdb_crm.py` contains the FastDB-first portable CRM contract and the FastDB feature rows for the same grid resource. `GridFastdb` uses `fastdb4py` ABI annotations for schema, subdivision, parent lookup, grid info, active grid IDs, strings, and nullable-string modeling, and pairs with `fastdb_bridge.py`, which derives or defines C-Two bridge hooks so the existing `NestedGrid` resource can remain domain-shaped while the CRM contract stays portable.

When adding new cross-language grid examples, start from `GridFastdb` and `grid_fastdb_bridge()`. When adding Python-only examples, start from `GridPython` and register `NestedGrid` directly.

The top-level `fastdb_relay_resource.py` and `fastdb_relay_client.py` scripts are the relay-backed FastDB protocol example:

```bash
c3 relay -b 0.0.0.0:8300
uv run python examples/python/fastdb_relay_resource.py --relay-url http://127.0.0.1:8300
uv run python examples/python/fastdb_relay_client.py --relay-url http://127.0.0.1:8300
```

Normal FastDB client calls return materialized values. Held FastDB calls return retained FastDB views when the C-Two FastDB bridge can build them from the response buffer. For example, `get_active_grid_infos` can be consumed through the logical batch view while the `cc.Held` object is alive:

```python
with cc.hold(grid.get_active_grid_infos)() as held:
    columns = held.value.column
    levels = columns.level
    global_ids = columns.global_id
    owned_rows = held.value.to_owned()
```

Server-side borrowed input is opt-in through `cc.register(..., input_lifetime={...})`. Use it only when the resource method accepts the same FDB input signature as `GridFastdb`; the existing `NestedGrid` bridge adapts FDB values to Python-native domain types, so methods with `bridge.input` intentionally stay on the default materialized path.
