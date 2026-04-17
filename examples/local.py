"""Single-process example with thread preference.

Demonstrates:
  - cc.register() to register multiple CRMs
  - cc.connect() with automatic thread-local optimization (zero serde)
  - cc.close() and cc.unregister() lifecycle
  - Multi-CRM in one process

Run:
    uv run python examples/local.py
"""
import os, sys
sys.path.insert(0, os.path.abspath(os.path.join(os.path.dirname(__file__), '../src/')))

import c_two as cc


# ── Define CRM contracts ──────────────────────────────────────────

@cc.crm(namespace='demo.greeter', version='0.1.0')
class Greeter:
    @cc.read
    def greet(self, name: str) -> str: ...

    @cc.read
    def language(self) -> str: ...


@cc.crm(namespace='demo.counter', version='0.1.0')
class Counter:
    def increment(self, amount: int) -> int: ...

    @cc.read
    def value(self) -> int: ...

    def reset(self) -> int: ...


# ── Define CRM implementations ──────────────────────────────────────

class GreeterImpl:
    def __init__(self, lang: str = 'en'):
        self._lang = lang
        self._templates = {
            'en': 'Hello, {}!',
            'zh': '你好, {}!',
            'ja': 'こんにちは, {}!',
        }

    def greet(self, name: str) -> str:
        return self._templates.get(self._lang, 'Hi, {}!').format(name)

    def language(self) -> str:
        return self._lang


class CounterImpl:
    def __init__(self, initial: int = 0):
        self._value = initial

    def increment(self, amount: int) -> int:
        self._value += amount
        return self._value

    def value(self) -> int:
        return self._value

    def reset(self) -> int:
        old = self._value
        self._value = 0
        return old


# ── Main ────────────────────────────────────────────────────────────

def main():
    # 1. Register CRMs — implicitly starts a shared IPC server
    cc.register(Greeter, GreeterImpl(lang='zh'), name='greeter')
    cc.register(Counter, CounterImpl(initial=100), name='counter')
    print(f'Server address: {cc.server_address()}')
    print(f'Registered routes: greeter, counter\n')

    # 2. Connect with thread preference (same process → zero serde)
    greeter = cc.connect(Greeter, name='greeter')
    counter = cc.connect(Counter, name='counter')

    print(f'Greeter proxy mode: {greeter.client._mode}')   # → "thread"
    print(f'Counter proxy mode: {counter.client._mode}\n')  # → "thread"

    # 3. Use the CRMs
    print(greeter.greet('World'))
    print(f'Language: {greeter.language()}\n')

    print(f'Counter starts at: {counter.value()}')
    counter.increment(10)
    counter.increment(5)
    print(f'After +10 and +5: {counter.value()}')
    old = counter.reset()
    print(f'Reset (was {old}): {counter.value()}\n')

    # 4. Cleanup
    cc.close(greeter)
    cc.close(counter)
    cc.unregister('greeter')
    cc.unregister('counter')
    cc.shutdown()
    print('Done — all resources cleaned up.')


if __name__ == '__main__':
    main()
