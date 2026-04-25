from tests.fixtures.ihello import HelloData


class HelloImpl:
    """Lightweight resource implementation for testing. No heavy dependencies."""

    def greeting(self, name: str) -> str:
        return f'Hello, {name}!'

    def add(self, a: int, b: int) -> int:
        return a + b

    def echo_none(self, msg: str) -> str | None:
        if msg == 'none':
            return None
        return msg

    def get_items(self, ids: list[int]) -> list[str]:
        return [f'item-{i}' for i in ids]

    def get_data(self, id: int) -> HelloData:
        return HelloData(name=f'data-{id}', value=id * 10)
