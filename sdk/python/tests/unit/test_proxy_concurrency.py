"""Unit tests for thread-local proxy native concurrency adapter."""
from __future__ import annotations

from contextlib import nullcontext
from types import SimpleNamespace
from unittest.mock import Mock

import pytest

from c_two.error import ResourceClosed
from c_two.transport.client.proxy import CRMProxy


class Resource:
    def op(self):
        return 'ok'


class RecordingResource:
    def __init__(self):
        self.calls = []

    def op(self):
        self.calls.append('op')
        return 'ok'


class RouteClosedMessageResource:
    def op(self):
        raise RuntimeError('route closed')


class FakeScheduler:
    def __init__(self, *, snapshot, method_index):
        self._snapshot = snapshot
        self._method_index = method_index
        self.guard_calls = []

    @property
    def is_unconstrained(self):
        return self._snapshot.is_unconstrained

    def method_idx(self, method_name):
        return self._method_index[method_name]

    def execution_guard(self, method_idx):
        self.guard_calls.append(method_idx)
        return nullcontext()


def test_unconstrained_call_direct_still_enters_native_guard_for_lifecycle():
    scheduler = FakeScheduler(
        snapshot=SimpleNamespace(
            mode='parallel',
            max_pending=None,
            max_workers=None,
            closed=False,
            is_unconstrained=True,
        ),
        method_index={'op': 0},
    )
    proxy = CRMProxy.thread_local(Resource(), scheduler=scheduler)
    assert proxy.call_direct('op', ()) == 'ok'
    assert scheduler.guard_calls == [0]


def test_constrained_call_direct_enters_native_guard_by_method_index():
    scheduler = FakeScheduler(
        snapshot=SimpleNamespace(
            mode='exclusive',
            max_pending=None,
            max_workers=None,
            closed=False,
            is_unconstrained=False,
        ),
        method_index={'op': 7},
    )
    proxy = CRMProxy.thread_local(Resource(), scheduler=scheduler)
    assert proxy.call_direct('op', ()) == 'ok'
    assert scheduler.guard_calls == [7]


def test_closed_call_direct_fails_before_entering_resource():
    resource = RecordingResource()
    scheduler = FakeScheduler(
        snapshot=SimpleNamespace(
            mode='parallel',
            max_pending=None,
            max_workers=None,
            closed=True,
            is_unconstrained=False,
        ),
        method_index={'op': 0},
    )
    scheduler.execution_guard = Mock(side_effect=RuntimeError('route closed'))
    proxy = CRMProxy.thread_local(resource, name='grid', scheduler=scheduler)

    with pytest.raises(ResourceClosed, match="route 'grid' is closed") as exc_info:
        proxy.call_direct('op', ())
    assert exc_info.value.details == {'route': 'grid'}
    assert resource.calls == []


def test_resource_runtime_error_named_route_closed_is_not_reclassified():
    scheduler = FakeScheduler(
        snapshot=SimpleNamespace(
            mode='parallel',
            max_pending=None,
            max_workers=None,
            closed=False,
            is_unconstrained=True,
        ),
        method_index={'op': 0},
    )
    proxy = CRMProxy.thread_local(
        RouteClosedMessageResource(),
        name='grid',
        scheduler=scheduler,
    )

    with pytest.raises(RuntimeError, match='route closed') as exc_info:
        proxy.call_direct('op', ())
    assert not isinstance(exc_info.value, ResourceClosed)
    assert scheduler.guard_calls == [0]
