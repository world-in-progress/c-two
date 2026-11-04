"""Real Core IPC/relay calls through Python call-options connection views."""
from __future__ import annotations

import threading
import time
from concurrent.futures import ThreadPoolExecutor

import httpx
import pytest

from tests.fixtures import local_server

from c_two._native import RuntimeSession, resolve_server_ipc_config

import c_two as cc

import c_two.observe
from c_two.transport.registry import _get_default_runtime, _reset_default_runtime


@pytest.fixture(params=['bound', 'class'])
def invoke(request):
    if request.param == 'bound':
        return lambda crm, *args, **kwargs: crm.echo(*args, **kwargs)
    return lambda crm, *args, **kwargs: type(crm).echo(crm, *args, **kwargs)


@cc.crm(namespace='test.call_options.integration', version='0.1.0')
class TimedContract:
    @cc.read
    def echo(self, value: object, timeout: float = 0.0) -> object:
        ...


class TimedResource:
    def __init__(self):
        self.calls = 0
        self.started = threading.Event()
        self.finished = threading.Event()
        self.completed = []

    def echo(self, value, timeout=0.0):
        self.calls += 1
        self.started.set()
        time.sleep(timeout)
        self.completed.append(value)
        self.finished.set()
        return value


@pytest.fixture
def ipc_connection(unique_ipc_address, request):
    _reset_default_runtime()
    if hasattr(request, 'param'):
        slots, budget = request.param
        cc.set_call_execution_limits(
            max_outstanding_calls=slots, payload_buffer_budget_bytes=budget,
        )
    resource = TimedResource()
    server = local_server(unique_ipc_address)
    server.register_crm(TimedContract, resource, name='timed')
    server.start()
    connection = cc.connect(TimedContract, name='timed', address=unique_ipc_address)
    try:
        yield connection, resource
    finally:
        cc.close(connection)
        server.shutdown()
        _reset_default_runtime()


class SlowPickle:
    caller_threads = []

    def __reduce__(self):
        type(self).caller_threads.append(threading.get_ident())
        time.sleep(0.08)
        return str, ('encoded',)


def test_ipc_zero_timeout_sends_no_request(ipc_connection, invoke):
    connection, resource = ipc_connection
    view = cc.with_call_options(connection, timeout=0)
    SlowPickle.caller_threads = []
    try:
        with pytest.raises(cc.error.CallDeadlineExceeded) as failure:
            invoke(view, SlowPickle())
        assert failure.value.transport_phase == 'pre_dispatch'
        assert SlowPickle.caller_threads == []
        assert resource.calls == 0
        assert connection.echo('after') == 'after'
    finally:
        cc.close(view)


def test_ipc_serializer_consumes_original_deadline_on_caller(ipc_connection, invoke):
    connection, resource = ipc_connection
    view = cc.with_call_options(connection, timeout=0.02)
    SlowPickle.caller_threads = []
    try:
        with pytest.raises(cc.error.CallDeadlineExceeded) as failure:
            invoke(view, SlowPickle())
        assert failure.value.transport_phase == 'pre_dispatch'
        assert SlowPickle.caller_threads == [threading.get_ident()]
        assert resource.calls == 0
        assert connection.echo('after') == 'after'
    finally:
        cc.close(view)


def test_ipc_after_dispatch_timeout_keeps_pool_and_late_response_cleanup(ipc_connection):
    connection, resource = ipc_connection
    view = cc.with_call_options(connection, timeout=0.05)
    value = b'x' * (2 * 1024 * 1024)
    try:
        with pytest.raises(cc.error.CallDeadlineExceeded) as failure:
            cc.hold(view.echo)(value, timeout=0.2)
        assert failure.value.transport_phase == 'dispatch_uncertain'
        assert resource.started.wait(1)
        assert connection.echo('next') == 'next'
        deadline = time.monotonic() + 2
        while value not in resource.completed and time.monotonic() < deadline:
            time.sleep(0.01)
        assert value in resource.completed
        assert connection.echo('after-late') == 'after-late'
    finally:
        cc.close(view)


def test_concurrent_views_share_connection_with_independent_timeouts_and_close(ipc_connection):
    connection, resource = ipc_connection
    short = cc.with_call_options(connection, timeout=0.05)
    long = cc.with_call_options(connection, timeout=2)
    assert short.client.route_uid == long.client.route_uid == connection.client.route_uid
    try:
        with ThreadPoolExecutor(max_workers=2) as executor:
            slow = executor.submit(short.echo, 'slow', timeout=0.2)
            assert resource.started.wait(1)
            cc.close(short)
            fast = executor.submit(long.echo, 'fast', timeout=0.01)
            assert fast.result(2) == 'fast'
            with pytest.raises(cc.error.CallDeadlineExceeded):
                slow.result(2)
        assert connection.echo('original') == 'original'
    finally:
        cc.close(short)
        cc.close(long)


def test_obtained_hold_survives_deadline_and_view_close(ipc_connection):
    connection, _ = ipc_connection
    view = cc.with_call_options(connection, timeout=0.1)
    try:
        held = cc.hold(view.echo)(b'owned')
        cc.close(view)
        time.sleep(0.15)
        assert held.value == b'owned'
        held.release()
        with pytest.raises(RuntimeError):
            _ = held.value
    finally:
        cc.close(view)


@pytest.fixture
def http_connection(start_c3_relay):
    resource = TimedResource()
    name = 'timed-call-options'
    cc.register(TimedContract, resource, name=name)
    registry = _get_default_runtime()
    identity = registry._runtime_session.ensure_server()
    relay = start_c3_relay()
    max_payload_size = resolve_server_ipc_config(
        registry._runtime_session.server_ipc_overrides,  # noqa: SLF001
    )['max_payload_size']
    with httpx.Client(
        trust_env=False, timeout=5,
        headers={'x-c2-local-namespace': registry.local_endpoint_context().namespace_id},
    ) as http:
        response = http.post(f'{relay.url}/_register', json={
            'name': name,
            'server_id': identity['server_id'],
            'server_instance_id': identity['server_instance_id'],
            'address': identity['ipc_address'],
            'max_payload_size': max_payload_size,
        })
        assert response.status_code == 201, response.text
    connection = cc.connect(TimedContract, name=name, address=relay.url)
    try:
        yield connection, resource
    finally:
        cc.close(connection)
        cc.unregister(name)


def test_http_original_deadline_explicit_unlimited_and_long_override(http_connection):
    connection, resource = http_connection
    short = cc.with_call_options(connection, timeout=0.02)
    unlimited = cc.with_call_options(short, timeout=None)
    longer = cc.with_call_options(short, timeout=2)
    try:
        with pytest.raises(cc.error.CallDeadlineExceeded) as failure:
            short.echo(SlowPickle())
        assert failure.value.transport_phase == 'pre_dispatch'
        assert resource.calls == 0
        assert unlimited.echo('unlimited', timeout=0.08) == 'unlimited'
        assert longer.echo('longer', timeout=0.08) == 'longer'
        with pytest.raises(cc.error.CallDeadlineExceeded) as failure:
            short.echo('dispatched', timeout=0.1)
        assert failure.value.transport_phase == 'dispatch_uncertain'
        assert longer.echo('subsequent') == 'subsequent'
    finally:
        cc.close(short)
        cc.close(unlimited)
        cc.close(longer)


@pytest.mark.parametrize('ipc_connection', [(1, None)], indirect=True)
def test_slot_capacity_rejects_before_pickle_and_releases_after_real_work(ipc_connection, invoke, monkeypatch):
    connection, resource = ipc_connection
    view = cc.with_call_options(connection, timeout=0.05)
    second_view = cc.with_call_options(connection, timeout=1)
    release_resource = threading.Event()

    def gated_echo(value, timeout=0.0):
        resource.calls += 1
        resource.started.set()
        try:
            assert release_resource.wait(10)
            resource.completed.append(value)
            return value
        finally:
            resource.finished.set()

    monkeypatch.setattr(resource, 'echo', gated_echo)
    SlowPickle.caller_threads = []
    try:
        with pytest.raises(cc.error.CallDeadlineExceeded) as timeout:
            view.echo('active', timeout=0.2)
        assert timeout.value.transport_phase == 'dispatch_uncertain'
        assert resource.started.wait(1)
        # Server callback entry can precede the client's asynchronous send
        # receipt. Keep the callback gated while that receipt refunds input.
        deadline = time.monotonic() + 2
        while (cc.observe.call_execution_snapshot()['used_retained_bytes']
               and time.monotonic() < deadline):
            time.sleep(0.01)
        snapshot = cc.observe.call_execution_snapshot()
        assert snapshot['max_operations'] == 1
        assert snapshot['used_operations'] == 1
        # The sent input has left the budget; the unfinished reply still owns
        # the operation slot and prevents another call from being admitted.
        assert snapshot['used_retained_bytes'] == 0
        with pytest.raises(cc.error.CallCapacityExceeded) as failure:
            invoke(second_view, SlowPickle())
        assert int(failure.value.code) == 717
        assert failure.value.details['transport_phase'] == 'pre_dispatch'
        assert failure.value.details['fallback_eligible'] == 'false'
        assert SlowPickle.caller_threads == []
        release_resource.set()
        assert resource.finished.wait(2)
        deadline = time.monotonic() + 2
        while cc.observe.call_execution_snapshot()['used_operations'] and time.monotonic() < deadline:
            time.sleep(0.01)
        assert cc.observe.call_execution_snapshot()['used_operations'] == 0
        assert view.echo('after') == 'after'
    finally:
        release_resource.set()
        cc.close(view)
        cc.close(second_view)


@pytest.mark.parametrize('ipc_connection', [(2, 1)], indirect=True)
def test_unknown_pickle_bytes_charged_before_native_handoff(ipc_connection):
    connection, resource = ipc_connection
    view = cc.with_call_options(connection, timeout=1)
    SlowPickle.caller_threads = []
    try:
        with pytest.raises(cc.error.CallCapacityExceeded):
            view.echo(SlowPickle())
        assert SlowPickle.caller_threads == [threading.get_ident()]
        assert resource.calls == 0
        snapshot = cc.observe.call_execution_snapshot()
        assert snapshot['used_operations'] == 0
        assert snapshot['used_retained_bytes'] == 0
    finally:
        cc.close(view)


@pytest.mark.parametrize('ipc_connection', [(1, None)], indirect=True)
@pytest.mark.parametrize('owner_kind', ['prepared', 'rpc'])
def test_retired_domain_and_shutdown_report_unfinished_charges_honestly(ipc_connection, monkeypatch, owner_kind):
    from c_two._native import NativePreparedCall, RuntimeSession

    connection, resource = ipc_connection
    session = _get_default_runtime()._runtime_session
    replacement = RuntimeSession(max_outstanding_calls=1)
    view = cc.with_call_options(connection, timeout=30 if owner_kind == 'prepared' else 1)
    owner = None
    executor = None
    release_resource = threading.Event()
    try:
        if owner_kind == 'prepared':
            # A real prepared owner still holds its reserved permit independently
            # of client transport shutdown; no materialization/dispatch is needed.
            owner = view.client.begin_call('echo')
            assert isinstance(owner, NativePreparedCall)
            retained_bytes = 32
            owner.charge_input(retained_bytes)
        else:
            # Hold server execution with a barrier, not a guessed sleep. Its
            # operation slot outlives the caller's deadline, while its sent
            # input bytes have already returned to the payload budget.
            def gated_echo(value, timeout=0.0):
                resource.calls += 1
                resource.started.set()
                try:
                    assert release_resource.wait(10)
                    resource.completed.append(value)
                    return value
                finally:
                    resource.finished.set()

            monkeypatch.setattr(resource, 'echo', gated_echo)
            executor = ThreadPoolExecutor(max_workers=1)
            call = executor.submit(view.echo, 'active', timeout=0.0)
            assert resource.started.wait(2)
            with pytest.raises(cc.error.CallDeadlineExceeded) as failure:
                call.result(2)
            assert failure.value.transport_phase == 'dispatch_uncertain'
            assert not resource.finished.is_set()
            retained_bytes = 0

        actual = session.call_execution_snapshot()
        assert not actual['closed']
        assert actual['used_operations'] == 1
        assert actual['used_retained_bytes'] == retained_bytes
        observation = session.retire_memory_observation()
        replacement.adopt_retired_memory_observation(observation)
        snapshot = replacement.call_execution_snapshot()
        assert snapshot['used_operations'] == 0
        assert snapshot['used_retained_bytes'] == 0
        assert len(snapshot['retired']) == 1
        retired = snapshot['retired'][0]
        assert retired['state'] == 'retired'
        assert not retired['closed']
        assert retired['used_operations'] == 1
        assert retired['used_retained_bytes'] == retained_bytes

        if owner_kind == 'prepared':
            outcome = session.begin_shutdown().observe()
            assert outcome['completed']
            assert outcome['ipc_clients_drained']
            snapshot = replacement.call_execution_snapshot()
            assert len(snapshot['retired']) == 1
            retired = snapshot['retired'][0]
            assert retired['state'] == 'retired'
            assert retired['closed']
            assert retired['used_operations'] == 1
            assert retired['used_retained_bytes'] == retained_bytes
            assert resource.calls == 0
            owner.close()
        else:
            outcome = session.begin_shutdown(timeout_seconds=0.05).observe()
            assert outcome['completed'] is False, outcome
            assert outcome['ipc_clients_drained'] is False, outcome
            assert not resource.finished.is_set()
            # The caller's deadline ended only its wait. Retirement keeps the
            # dispatched operation slot until the resource finishes; its sent
            # input bytes were already returned to the payload budget.
            actual = session.call_execution_snapshot()
            assert actual['closed']
            assert actual['used_operations'] == 1
            assert actual['used_retained_bytes'] == retained_bytes
            release_resource.set()
            assert resource.finished.wait(2)
            outcome = session.begin_shutdown(timeout_seconds=2).observe()
            assert outcome['completed'], outcome
            assert outcome['ipc_clients_drained'], outcome
            assert resource.completed == ['active']

        actual = session.call_execution_snapshot()
        assert actual['closed']
        assert actual['used_operations'] == 0
        assert actual['used_retained_bytes'] == 0
        assert replacement.call_execution_snapshot()['retired'] == []
    finally:
        release_resource.set()
        if owner is not None:
            owner.close()
        if executor is not None:
            executor.shutdown(wait=True)
        cc.close(view)
        replacement.shutdown()


@pytest.fixture
def portable_connection(unique_ipc_address, request):
    from tests.integration.test_portable_payload_runtime import Echo, EchoResource

    _reset_default_runtime()
    if hasattr(request, 'param'):
        if isinstance(request.param, dict):
            cc.set_call_execution_limits(**request.param)
        else:
            cc.set_call_execution_limits(payload_buffer_budget_bytes=request.param)
    server = local_server(unique_ipc_address)
    server.register_crm(Echo, EchoResource(), name='portable-options')
    server.start()
    connection = cc.connect(Echo, name='portable-options', address=unique_ipc_address)
    try:
        yield connection
    finally:
        cc.close(connection)
        server.shutdown()
        _reset_default_runtime()


@pytest.mark.parametrize('portable_connection', [1], indirect=True)
def test_real_known_fastdb_rejects_before_any_binary_copy(portable_connection, monkeypatch, invoke):
    from fastdb4py.payload import Payload
    from tests.integration.test_portable_payload_runtime import build_payload

    source = build_payload()
    calls = []
    original = Payload.binary_bytes

    def copy(self):
        calls.append(threading.get_ident())
        return original(self)

    monkeypatch.setattr(Payload, 'binary_bytes', copy)
    view = cc.with_call_options(portable_connection, timeout=1)
    try:
        with pytest.raises(cc.error.CallCapacityExceeded):
            invoke(view, source)
        assert calls == []
        assert cc.observe.call_execution_snapshot()['used_operations'] == 0
        assert cc.observe.call_execution_snapshot()['used_retained_bytes'] == 0
    finally:
        source.close()
        cc.close(view)


def test_fastdb_checked_owner_and_views_preserve_hold_invalidation_order(portable_connection):
    from fastdb4py.payload import PayloadError
    from tests.integration.test_portable_payload_runtime import build_payload

    source = build_payload()
    view = cc.with_call_options(portable_connection, timeout=0.1)
    try:
        held = cc.hold(view.echo)(source)
        owner = held.value
        checked = owner.entry_view(0)
        value = checked.at(0)
        cc.close(view)
        time.sleep(0.15)
        assert value.get_u8() == 7
        held.release()
        with pytest.raises(PayloadError) as failure:
            value.get_u8()
        assert failure.value.symbol == 'VIEW_INVALIDATED'
        with pytest.raises(PayloadError):
            owner.entry_view(0)
        value.close()
        checked.close()
    finally:
        source.close()
        cc.close(view)


def test_native_prepared_call_is_single_use(ipc_connection):
    import pickle

    connection, _ = ipc_connection
    view = cc.with_call_options(connection, timeout=1)
    data = pickle.dumps(('prepared', 0.0), protocol=4)
    prepared = view.client.begin_call('echo')
    try:
        response = prepared.call(data)
        try:
            assert pickle.loads(memoryview(response)) == 'prepared'
        finally:
            response.release()
        with pytest.raises(ValueError, match='consumed'):
            prepared.call(data)
    finally:
        prepared.close()
        cc.close(view)


@pytest.mark.parametrize('ipc_connection', [(None, 64)], indirect=True)
@pytest.mark.parametrize('first_timeout', [15, None])
def test_sent_ipc_input_refunds_full_budget_before_reply(ipc_connection, monkeypatch, first_timeout):
    import pickle

    connection, resource = ipc_connection
    first_view = cc.with_call_options(connection, timeout=first_timeout)
    second_view = cc.with_call_options(connection, timeout=5)
    first = first_view.client.begin_call('echo')
    second = None
    active_started = threading.Event()
    active_finished = threading.Event()
    release_active = threading.Event()

    def gated_echo(value, timeout=0.0):
        resource.calls += 1
        if value == 'active':
            active_started.set()
            try:
                assert release_active.wait(12)
            finally:
                active_finished.set()
        return value

    monkeypatch.setattr(resource, 'echo', gated_echo)
    active_request = pickle.dumps(('active', 0.0), protocol=4).ljust(64, b'\0')
    second_request = pickle.dumps(('second', 0.0), protocol=4).ljust(64, b'\0')
    assert len(active_request) == len(second_request) == 64
    try:
        first.charge_input(64)
        with ThreadPoolExecutor(max_workers=2) as executor:
            active = executor.submit(first.call, active_request)
            try:
                assert active_started.wait(3)
                assert not active.done()
                second = second_view.client.begin_call('echo')
                # This bounded charge waits for the first send to return all
                # 64 bytes, even if its asynchronous send receipt trails the
                # resource callback's entry. The first reply is still gated.
                second.charge_input(64)
                assert not active_finished.is_set()
                subsequent = executor.submit(second.call, second_request)
                response = subsequent.result(6)
                try:
                    assert pickle.loads(memoryview(response)) == 'second'
                finally:
                    response.release()
                assert not active_finished.is_set()
                assert not active.done()
            finally:
                release_active.set()
            response = active.result(6)
            try:
                assert pickle.loads(memoryview(response)) == 'active'
            finally:
                response.release()
        assert resource.calls == 2
        assert cc.observe.memory_stats()['payload_buffer']['used_bytes'] == 0
    finally:
        release_active.set()
        first.close()
        if second is not None:
            second.close()
        cc.close(first_view)
        cc.close(second_view)


@pytest.mark.parametrize('ipc_connection', [(None, 64)], indirect=True)
@pytest.mark.parametrize('entry', ['charge', 'bytes', 'buffer', 'resized_buffer'])
def test_full_payload_budget_wait_releases_gil_for_reservation_release(ipc_connection, entry):
    import pickle

    connection, resource = ipc_connection
    view = cc.with_call_options(connection, timeout=2)
    first = connection.client.begin_call('echo')
    first.charge_input(64)
    cell = cc.observe.memory_stats()['payload_buffer']
    assert set(cell) == {
        'limit_bytes', 'used_bytes', 'peak_bytes',
        'rejected_allocations', 'rejected_bytes',
    }
    assert cell['limit_bytes'] == 64
    assert cell['used_bytes'] > 0
    request = pickle.dumps(('second', 0.0), protocol=4).ljust(64, b'\0')
    source = bytearray(request) if entry in ('buffer', 'resized_buffer') else request
    entered = threading.Event()

    def wait_for_budget():
        prepared = view.client.begin_call('echo')
        try:
            entered.set()
            if entry == 'charge':
                prepared.charge_input(64)
                return None
            return prepared.call(source)
        finally:
            prepared.close()

    try:
        with ThreadPoolExecutor(max_workers=1) as executor:
            future = executor.submit(wait_for_budget)
            assert entered.wait(1)
            # This observation wait gives the worker the GIL and must expire
            # independently of its native budget wait.
            from concurrent.futures import TimeoutError as FutureTimeoutError
            with pytest.raises(FutureTimeoutError):
                future.result(timeout=0.05)
            if entry == 'resized_buffer':
                source.extend(b'x')
            first.close()
            if entry == 'resized_buffer':
                with pytest.raises(ValueError, match='buffer size changed'):
                    future.result(2)
                result = None
            else:
                result = future.result(2)
            if result is not None:
                try:
                    assert pickle.loads(memoryview(result)) == 'second'
                finally:
                    result.release()
        assert resource.calls == (0 if entry in ('charge', 'resized_buffer') else 1)
        assert cc.observe.memory_stats()['payload_buffer']['used_bytes'] == 0
    finally:
        first.close()
        cc.close(view)


@pytest.mark.parametrize('ipc_connection', [(None, 64)], indirect=True)
def test_full_payload_budget_wait_uses_original_pre_dispatch_deadline(ipc_connection):
    import pickle

    connection, resource = ipc_connection
    first = connection.client.begin_call('echo')
    first.charge_input(64)
    view = cc.with_call_options(connection, timeout=0.05)
    try:
        prepared = view.client.begin_call('echo')
        with pytest.raises(cc.error.CoreError) as failure:
            prepared.charge_input(64)
        assert failure.value.name == 'CallDeadlineExceeded'
        assert failure.value.transport_phase == 'pre_dispatch'
        prepared.close()
        assert resource.calls == 0
        assert cc.observe.call_execution_snapshot()['used_operations'] == 0
    finally:
        first.close()
        cc.close(view)


@pytest.mark.parametrize('ipc_connection', [(None, 0)], indirect=True)
def test_zero_payload_budget_accepts_zero_and_rejects_positive_input(ipc_connection):
    connection, resource = ipc_connection
    prepared = connection.client.begin_call('echo')
    try:
        prepared.charge_input(0)
        with pytest.raises(cc.error.CoreError) as failure:
            prepared.charge_input(1)
        assert failure.value.name == 'CallCapacityExceeded'
        assert failure.value.transport_phase == 'pre_dispatch'
        assert resource.calls == 0
    finally:
        prepared.close()
    assert cc.observe.memory_stats()['payload_buffer']['used_bytes'] == 0


@pytest.mark.parametrize('ipc_connection', [(None, 64)], indirect=True)
def test_payload_buffer_retirement_observation_follows_live_reservation(ipc_connection):
    import pickle

    connection, _ = ipc_connection
    response = connection.client.begin_call('echo')
    response.charge_input(64)
    old = _get_default_runtime()._runtime_session  # noqa: SLF001
    replacement = RuntimeSession()
    handoff = old.retire_memory_observation()
    replacement.adopt_retired_memory_observation(handoff)
    try:
        cells = replacement.memory_stats()['retired_payload_buffers']
        assert len(cells) == 1
        assert cells[0]['limit_bytes'] == 64
        assert cells[0]['used_bytes'] > 0
    finally:
        response.close()
    assert replacement.memory_stats()['retired_payload_buffers'][0]['used_bytes'] == 0


def test_http_stale_refusal_does_not_retain_input_for_post_replay():
    """A stale POST is reported without retaining a speculative replay copy."""
    import json
    import pickle
    from http.server import BaseHTTPRequestHandler
    from c_two.crm.contract import crm_contract
    from tests.fixtures.http_server import LoopbackHTTPServer

    expected = crm_contract(TimedContract)
    calls = [0, 0]
    headers_seen = []
    servers = []
    threads = []
    route_name = 'deadline-retry'
    call_timeout = 3.0

    def route(index):
        return {
            'name': route_name, 'relay_url': f'http://127.0.0.1:{servers[index].server_port}',
            'route_uid': f'deadline-retry-{index}',
            'ipc_address': None, 'server_id': None, 'server_instance_id': None,
            'crm_ns': expected.crm_ns, 'crm_name': expected.crm_name,
            'crm_ver': expected.crm_ver, 'abi_hash': expected.abi_hash,
            'signature_hash': expected.signature_hash, 'max_payload_size': 8 * 1024 * 1024,
        }

    def handler(index):
        class Handler(BaseHTTPRequestHandler):
            def log_message(self, *_args):
                pass

            def reply(self, status, body):
                self.send_response(status)
                self.send_header('Content-Length', str(len(body)))
                self.end_headers()
                try:
                    self.wfile.write(body)
                except (BrokenPipeError, ConnectionResetError):
                    pass

            def do_GET(self):
                if self.path.startswith('/_resolve/'):
                    self.reply(200, json.dumps([route(0), route(1)]).encode())
                elif self.path.startswith('/_probe/'):
                    self.reply(200, b'')
                else:
                    self.reply(404, b'')

            def do_POST(self):
                body = self.rfile.read(int(self.headers['Content-Length']))
                calls[index] += 1
                headers_seen.append(dict(self.headers))
                if index == 0:
                    refusal = {'version': 1, 'code': 704, 'name': 'RouteStale',
                               'message': 'authoritative stale route', 'details': {}}
                    self.reply(409, json.dumps(refusal).encode())
                else:
                    value, _business_timeout = pickle.loads(body)
                    self.reply(200, pickle.dumps(value))
        return Handler

    connection = None
    finite = None
    unlimited = None
    try:
        for index in range(2):
            server = LoopbackHTTPServer(('127.0.0.1', 0), handler(index))
            server.daemon_threads = True
            servers.append(server)
            thread = threading.Thread(target=server.serve_forever)
            thread.start()
            threads.append(thread)
        anchor = f'http://127.0.0.1:{servers[0].server_port}'
        connection = cc.connect(TimedContract, name=route_name, address=anchor)
        finite = cc.with_call_options(connection, timeout=call_timeout)
        with pytest.raises(cc.error.RouteStale):
            finite.echo('retry')
        assert calls == [1, 0]
        for headers in headers_seen:
            normalized = {key.lower(): value for key, value in headers.items()}
            assert normalized['x-c2-expected-abi-hash'] == expected.abi_hash
            assert normalized['x-c2-expected-signature-hash'] == expected.signature_hash
    finally:
        for view in (finite, unlimited, connection):
            if view is not None:
                cc.close(view)
        for server in servers:
            server.shutdown()
            server.server_close()
        for thread in threads:
            thread.join(timeout=2)


@pytest.mark.parametrize('ipc_connection', [(0, None)], indirect=True)
def test_zero_slots_rejects_both_entry_forms_without_reducer_or_resource(ipc_connection, invoke):
    connection, resource = ipc_connection
    view = cc.with_call_options(connection, timeout=1)
    SlowPickle.caller_threads = []
    try:
        with pytest.raises(cc.error.CallCapacityExceeded) as failure:
            invoke(view, SlowPickle())
        assert int(failure.value.code) == 717
        assert failure.value.details['transport_phase'] == 'pre_dispatch'
        assert SlowPickle.caller_threads == []
        assert resource.calls == 0
        assert cc.observe.call_execution_snapshot()['used_operations'] == 0
        assert cc.observe.call_execution_snapshot()['used_retained_bytes'] == 0
    finally:
        cc.close(view)


@pytest.mark.parametrize('timeout', [0, 1])
@pytest.mark.parametrize('portable_connection', [{'max_outstanding_calls': 0}], indirect=True)
def test_zero_slots_or_deadline_never_materializes_fastdb(portable_connection, monkeypatch, invoke, timeout):
    from fastdb4py.payload import Payload
    from tests.integration.test_portable_payload_runtime import build_payload, EchoResource

    source = build_payload()
    calls = {'report': 0, 'copy': 0, 'resource': 0}

    def forbidden(stage):
        def counted(*args):
            calls[stage] += 1
            raise AssertionError(f'{stage} must not run before admission')
        return counted

    monkeypatch.setattr(Payload, 'execution_report', forbidden('report'))
    monkeypatch.setattr(Payload, 'binary_bytes', forbidden('copy'))
    monkeypatch.setattr(EchoResource, 'echo', forbidden('resource'))
    view = cc.with_call_options(portable_connection, timeout=timeout)
    try:
        expected = cc.error.CallDeadlineExceeded if timeout == 0 else cc.error.CallCapacityExceeded
        with pytest.raises(expected):
            invoke(view, source)
        assert calls == {'report': 0, 'copy': 0, 'resource': 0}
        assert cc.observe.call_execution_snapshot()['used_operations'] == 0
        assert cc.observe.call_execution_snapshot()['used_retained_bytes'] == 0
    finally:
        source.close()
        cc.close(view)


@pytest.mark.parametrize('timeout', [..., 1])
@pytest.mark.parametrize('invalid_owner', [False, True])
def test_report_or_invalid_owner_error_keeps_input_classification_and_zero_charges(portable_connection, monkeypatch, invoke, timeout, invalid_owner):
    from fastdb4py.payload import Payload
    from tests.integration.test_portable_payload_runtime import build_payload

    source = build_payload()
    view = cc.with_call_options(portable_connection, timeout=timeout)
    if invalid_owner:
        source.close()
    else:
        def report_error(self):
            raise ValueError('report failed')
        monkeypatch.setattr(Payload, 'execution_report', report_error)
    try:
        with pytest.raises(cc.error.ClientSerializeInput) as failure:
            invoke(view, source)
        if invalid_owner:
            assert failure.value.details['cause_owner'] == 'fastdb'
            assert failure.value.details['fastdb_symbol']
        else:
            assert isinstance(failure.value.__cause__, ValueError)
        assert cc.observe.call_execution_snapshot()['used_operations'] == 0
        assert cc.observe.call_execution_snapshot()['used_retained_bytes'] == 0
    finally:
        source.close()
        cc.close(view)


def test_closed_view_is_call_resource_error_before_serializer(ipc_connection, invoke):
    connection, resource = ipc_connection
    view = cc.with_call_options(connection, timeout=1)
    cc.close(view)
    SlowPickle.caller_threads = []
    with pytest.raises(cc.error.ClientCallResource):
        invoke(view, SlowPickle())
    assert SlowPickle.caller_threads == []
    assert resource.calls == 0
    assert cc.observe.call_execution_snapshot()['used_operations'] == 0
    assert cc.observe.call_execution_snapshot()['used_retained_bytes'] == 0
    assert connection.echo('after') == 'after'


def test_prepared_close_gc_and_invalid_buffer_refund_only_pre_dispatch(ipc_connection):
    import gc

    connection, resource = ipc_connection
    view = cc.with_call_options(connection, timeout=1)
    try:
        for explicit_close in [True, False]:
            prepared = view.client.begin_call('echo')
            prepared.charge_input(16)
            assert cc.observe.call_execution_snapshot()['used_operations'] == 1
            assert cc.observe.call_execution_snapshot()['used_retained_bytes'] == 16
            if explicit_close:
                prepared.close()
            del prepared
            gc.collect()
            assert cc.observe.call_execution_snapshot()['used_operations'] == 0
            assert cc.observe.call_execution_snapshot()['used_retained_bytes'] == 0

        prepared = view.client.begin_call('echo')
        prepared.charge_input(16)
        with pytest.raises(TypeError):
            prepared.call(object())
        assert cc.observe.call_execution_snapshot()['used_operations'] == 0
        assert cc.observe.call_execution_snapshot()['used_retained_bytes'] == 0
        with pytest.raises(ValueError, match='consumed'):
            prepared.call(b'')
        del prepared
        gc.collect()
        assert cc.observe.call_execution_snapshot()['used_operations'] == 0
        assert cc.observe.call_execution_snapshot()['used_retained_bytes'] == 0
        assert resource.calls == 0
        assert connection.echo('after') == 'after'
    finally:
        cc.close(view)


@pytest.mark.parametrize('readonly_alias', [False, True])
def test_mutable_backing_snapshot_survives_caller_handoff(ipc_connection, monkeypatch, readonly_alias):
    import pickle

    connection, resource = ipc_connection
    view = cc.with_call_options(connection, timeout=2)
    started = threading.Event()
    release = threading.Event()
    seen = []

    def controlled_echo(value, timeout=0.0):
        seen.append(value)
        started.set()
        assert release.wait(2)
        return value

    monkeypatch.setattr(resource, 'echo', controlled_echo)
    backing = bytearray(pickle.dumps(('initial', 0.0), protocol=4))
    source = memoryview(backing).toreadonly() if readonly_alias else backing
    try:
        with ThreadPoolExecutor(max_workers=1) as executor:
            prepared = view.client.begin_call('echo')
            future = executor.submit(prepared.call, source)
            try:
                assert started.wait(1)
                replacement = pickle.dumps(('mutated', 0.0), protocol=4)
                assert len(replacement) == len(backing)
                backing[:] = replacement
                release.set()
                response = future.result(2)
                try:
                    assert pickle.loads(memoryview(response)) == 'initial'
                finally:
                    response.release()
                assert seen == ['initial']
            finally:
                release.set()
                prepared.close()
    finally:
        if readonly_alias:
            source.release()
        cc.close(view)


@pytest.mark.parametrize('ipc_connection', [(None, 64)], indirect=True)
def test_prepared_call_consumption_releases_gil_while_a_charge_waits(ipc_connection):
    import pickle
    from concurrent.futures import TimeoutError as FutureTimeoutError

    connection, resource = ipc_connection
    occupied = connection.client.begin_call('echo')
    occupied.charge_input(64)
    view = cc.with_call_options(connection, timeout=3)
    prepared = view.client.begin_call('echo')
    request = pickle.dumps(('ready', 0.0), protocol=4).ljust(64, b'\0')
    try:
        with ThreadPoolExecutor(max_workers=2) as executor:
            charging = executor.submit(prepared.charge_input, 64)
            with pytest.raises(FutureTimeoutError):
                charging.result(timeout=0.05)
            calling = executor.submit(prepared.call, request)
            with pytest.raises(FutureTimeoutError):
                calling.result(timeout=0.05)
            occupied.close()
            charging.result(timeout=2)
            response = calling.result(timeout=2)
            try:
                assert pickle.loads(memoryview(response)) == 'ready'
            finally:
                response.release()
        assert resource.calls == 1
        assert cc.observe.memory_stats()['payload_buffer']['used_bytes'] == 0
    finally:
        occupied.close()
        prepared.close()
        cc.close(view)
