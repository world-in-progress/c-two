# Grid Examples

`grid_py_crm.py` is the Python fallback CRM path for the grid resource. It defines `GridPython` plus the Python-only `GridSchema` and `GridAttribute` dataclasses returned by `NestedGrid`, so resource processes can register `NestedGrid` directly and let pickle fallback carry those objects across local Python transports. This path is intentionally rejected by strict portable export/codegen and is useful for proving local Python ergonomics before committing to a portable ABI.

The former annotation-inferred FastDB grid contract and bridge examples were
removed with the portable-payload clean cut. Cross-language examples must use
an explicit `@cc.transfer(input=spec, output=spec)` contract and the official
`fastdb4py.payload.Payload` owner; a new GIS-shaped example will be added only
after the generic Rust/Python interop proof is complete.

For Python-only exploration, start from `GridPython` and register `NestedGrid`
directly. Python pickle remains a local Python runtime facility and is
intentionally rejected by portable contract export.
