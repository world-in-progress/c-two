"""A second lightweight CRM/ICRM pair for multi-CRM server testing."""
import pickle

import c_two as cc


@cc.icrm(namespace='test.counter', version='0.1.0')
class ICounter:
    """Lightweight ICRM for a stateful counter resource."""

    def get(self) -> int:
        ...

    def increment(self, amount: int) -> int:
        ...

    def reset(self) -> int:
        ...


class Counter:
    """Stateful CRM that maintains an integer counter."""

    def __init__(self, initial: int = 0):
        self._value = initial

    def get(self) -> int:
        return self._value

    def increment(self, amount: int) -> int:
        self._value += amount
        return self._value

    def reset(self) -> int:
        old = self._value
        self._value = 0
        return old
