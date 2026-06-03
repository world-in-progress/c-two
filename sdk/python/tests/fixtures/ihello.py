from dataclasses import dataclass

import c_two as cc


@dataclass
class HelloData:
    """A simple Python-only data type for pickle fallback tests."""

    name: str
    value: int


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
        """Returns a Python-only pickle fallback type."""
        ...
