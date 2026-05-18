# C-Two Optional Codec Providers

Provider modules let applications attach explicit payload codec identities to business types without making C-Two core understand the underlying wire format.

## Arrow

Use `@arrow.record` for dataclass-like records encoded as Arrow IPC. `list[Record]` is the normal batch annotation: regular calls materialize `list[Record]`, while `cc.hold(proxy.method)(...)` returns an `ArrowBatchView[Record]` when the response buffer is retained. `arrow.Batch[Record]` remains available when a method should return an Arrow-backed view even for ordinary calls.

```python
import c_two as cc
from c_two.providers import arrow

@arrow.record
class Cell:
    level: int
    global_id: int
    active: bool

def list_cells(self) -> list[Cell]:
    ...

def view_cells(self) -> arrow.Batch[Cell]:
    ...

with cc.hold(client.list_cells)() as held:
    table = held.value.to_table()
    raw_wire_buffer = held.buffer
```

## Custom

Use `@custom.record` when the project owns the runtime codec. C-Two exports the declared `CodecRef`; each language SDK or application must provide the real encode/decode/from-buffer implementation behind that identity.

```python
from c_two.providers import custom

@custom.record(
    id="org.example.payload",
    version="1",
    schema_text='{"kind":"payload","fields":["value"]}',
)
class Payload:
    value: int

    def serialize(value: "Payload") -> bytes:
        ...

    def deserialize(data: bytes | memoryview) -> "Payload":
        ...

    def from_buffer(data: memoryview) -> "Payload":
        ...
```
