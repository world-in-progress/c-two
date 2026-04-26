import pickle
import c_two as cc


@cc.transferable
class HelloData:
    """A simple transferable data type for testing custom serialization."""
    name: str
    value: int

    def serialize(data: 'HelloData') -> bytes:
        return pickle.dumps({'name': data.name, 'value': data.value})

    def deserialize(data_bytes: bytes) -> 'HelloData':
        d = pickle.loads(data_bytes)
        return HelloData(name=d['name'], value=d['value'])


@cc.transferable
class HelloItems:
    """Transferable for list[str] used as return type of get_items."""
    def serialize(items: list[str]) -> bytes:
        return pickle.dumps(items)

    def deserialize(data_bytes: bytes) -> list[str]:
        return pickle.loads(data_bytes)


@cc.crm(namespace='test.hello', version='0.1.0')
class Hello:
    """Lightweight CRM contract for unit testing."""

    def greeting(self, name: str) -> str:
        """Simple string in, string out."""
        ...

    def add(self, a: int, b: int) -> int:
        """Multiple primitive parameters."""
        ...

    def echo_none(self, msg: str) -> str | None:
        """Returns None when msg is 'none', otherwise echoes."""
        ...

    def get_items(self, ids: list[int]) -> list[str]:
        """List parameter and list return."""
        ...

    def get_data(self, id: int) -> HelloData:
        """Returns a custom Transferable type."""
        ...
