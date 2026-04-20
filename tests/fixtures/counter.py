"""A second lightweight CRM contract / resource pair for multi-CRM server testing."""
import pickle

import c_two as cc


@cc.crm(namespace='test.counter', version='0.1.0')
class Counter:
    """Lightweight CRM contract for a stateful counter resource."""

    def get(self) -> int:
        ...

    def increment(self, amount: int) -> int:
        ...

    def reset(self) -> int:
        ...


class CounterImpl:
    """Stateful resource that maintains an integer counter."""

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
