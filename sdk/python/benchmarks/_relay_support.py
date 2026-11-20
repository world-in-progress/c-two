"""SDK lifecycle for benchmarks using an external relay."""
from __future__ import annotations

import sys
import urllib.request
from collections.abc import Iterator
from contextlib import contextmanager

import c_two as cc


_NO_PROXY_OPENER = urllib.request.build_opener(urllib.request.ProxyHandler({}))


@contextmanager
def session(relay_url: str) -> Iterator[cc.Runtime]:
    try:
        with _NO_PROXY_OPENER.open(f'{relay_url}/health', timeout=5):
            pass
    except Exception as exc:
        bind = relay_url.removeprefix('http://').removeprefix('https://')
        raise SystemExit(
            f'External c3 relay is not reachable at {relay_url}. '
            f'Start it first with `c3 relay --bind {bind}` after running '
            '`python tools/dev/c3_tool.py --build --link` from the source checkout.'
        ) from exc

    runtime = cc.Runtime()
    runtime.set_relay_anchor(relay_url)
    try:
        yield runtime
    finally:
        outcome = runtime.shutdown()
        if outcome['completed']:
            if outcome['relay_errors']:
                print(f'Warning: incomplete benchmark cleanup: {outcome}', file=sys.stderr)
        else:
            message = f'benchmark shutdown still draining: {outcome}'
            if sys.exc_info()[0] is None:
                raise RuntimeError(message)
            print(f'Warning: {message}', file=sys.stderr)
