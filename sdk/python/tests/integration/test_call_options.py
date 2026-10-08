"""Real Core IPC/relay calls through Python call-options connection views."""
from __future__ import annotations

import threading
import time
from concurrent.futures import ThreadPoolExecutor

import httpx
import pytest

import c_two as cc
from c_two.transport import Server
from c_two.transport.registry import _ProcessRegistry


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
    _ProcessRegistry.reset()
    if hasattr(request, 'param'):
        slots, budget = request.param
        cc.set_call_execution_limits(
            max_outstanding_calls=slots, retained_input_budget_bytes=budget,
        )
    resource = TimedResource()
    server = Server(bind_address=unique_ipc_address)
    server.register_crm(TimedContract, resource, name='timed')
    server.start()
    connection = cc.connect(TimedContract, name='timed', address=unique_ipc_address)
    try:
        yield connection, resource
    finally:
        cc.close(connection)
        server.shutdown()
        _ProcessRegistry.reset()


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
    registry = _ProcessRegistry.get()
    identity = registry._runtime_session.ensure_server()
    relay = start_c3_relay()
    from c_two.config.ipc import _resolve_server_ipc_config
    with httpx.Client(trust_env=False, timeout=5) as http:
        response = http.post(f'{relay.url}/_register', json={
            'name': name,
            'server_id': identity['server_id'],
            'server_instance_id': identity['server_instance_id'],
            'address': identity['ipc_address'],
            'max_payload_size': _resolve_server_ipc_config()['max_payload_size'],
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
def test_slot_capacity_rejects_before_pickle_and_releases_after_real_work(ipc_connection, invoke):
    connection, resource = ipc_connection
    view = cc.with_call_options(connection, timeout=0.05)
    second_view = cc.with_call_options(connection, timeout=1)
    SlowPickle.caller_threads = []
    try:
        with pytest.raises(cc.error.CallDeadlineExceeded):
            view.echo('active', timeout=0.2)
        snapshot = cc.call_execution_snapshot()
        assert snapshot['max_operations'] == 1
        assert snapshot['used_operations'] == 1
        assert snapshot['used_retained_bytes'] > 0
        with pytest.raises(cc.error.CallCapacityExceeded) as failure:
            invoke(second_view, SlowPickle())
        assert int(failure.value.code) == 717
        assert failure.value.details['transport_phase'] == 'pre_dispatch'
        assert failure.value.details['fallback_eligible'] == 'false'
        assert SlowPickle.caller_threads == []
        assert resource.finished.wait(2)
        deadline = time.monotonic() + 2
        while cc.call_execution_snapshot()['used_operations'] and time.monotonic() < deadline:
            time.sleep(0.01)
        assert cc.call_execution_snapshot()['used_operations'] == 0
        assert view.echo('after') == 'after'
    finally:
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
        snapshot = cc.call_execution_snapshot()
        assert snapshot['used_operations'] == 0
        assert snapshot['used_retained_bytes'] == 0
    finally:
        cc.close(view)


@pytest.mark.parametrize('ipc_connection', [(1, None)], indirect=True)
@pytest.mark.parametrize('owner_kind', ['prepared', 'rpc'])
def test_retired_domain_and_shutdown_report_unfinished_charges_honestly(ipc_connection, monkeypatch, owner_kind):
    import pickle
    from c_two._native import NativePreparedCall, RuntimeSession

    connection, resource = ipc_connection
    session = _ProcessRegistry.get()._runtime_session
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
            nbytes = 32
            owner.charge_input(nbytes)
        else:
            # Hold server execution with a barrier, not a guessed sleep. Its
            # lifetime is distinct from the client's native transport future.
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
            nbytes = len(pickle.dumps(('active', 0.0), protocol=4))

        actual = session.call_execution_snapshot()
        assert not actual['closed']
        assert actual['used_operations'] == 1
        assert actual['used_retained_bytes'] == nbytes
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
        assert retired['used_retained_bytes'] == nbytes

        outcome = session.shutdown(route_names=[], relay_anchor_address=None)
        assert outcome['completed']
        assert outcome['ipc_clients_drained']
        if owner_kind == 'prepared':
            snapshot = replacement.call_execution_snapshot()
            assert len(snapshot['retired']) == 1
            retired = snapshot['retired'][0]
            assert retired['state'] == 'retired'
            assert retired['closed']
            assert retired['used_operations'] == 1
            assert retired['used_retained_bytes'] == nbytes
            assert resource.calls == 0
            owner.close()
        else:
            assert not resource.finished.is_set()
            # Explicit shutdown may finish the client future while server work
            # remains alive. Observe the actual metadata refund, never infer it
            # from server completion or assert that shutdown must stay pending.
            deadline = time.monotonic() + 2
            actual = session.call_execution_snapshot()
            while actual['used_operations'] != 0:
                assert actual['closed']
                assert actual['used_operations'] == 1
                assert actual['used_retained_bytes'] == nbytes
                assert time.monotonic() < deadline, actual
                actual = session.call_execution_snapshot()
            release_resource.set()
            assert resource.finished.wait(2)
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
        replacement.shutdown(route_names=[], relay_anchor_address=None)


@pytest.fixture
def portable_connection(unique_ipc_address, request):
    from tests.integration.test_portable_payload_runtime import Echo, EchoResource

    _ProcessRegistry.reset()
    if hasattr(request, 'param'):
        if isinstance(request.param, dict):
            cc.set_call_execution_limits(**request.param)
        else:
            cc.set_call_execution_limits(retained_input_budget_bytes=request.param)
    server = Server(bind_address=unique_ipc_address)
    server.register_crm(Echo, EchoResource(), name='portable-options')
    server.start()
    connection = cc.connect(Echo, name='portable-options', address=unique_ipc_address)
    try:
        yield connection
    finally:
        cc.close(connection)
        server.shutdown()
        _ProcessRegistry.reset()


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
        assert cc.call_execution_snapshot()['used_operations'] == 0
        assert cc.call_execution_snapshot()['used_retained_bytes'] == 0
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


def test_native_prepared_call_is_single_use_and_writer_stays_on_caller(ipc_connection):
    import pickle

    connection, _ = ipc_connection
    view = cc.with_call_options(connection, timeout=1)
    data = pickle.dumps(('prepared', 0.0), protocol=4)
    caller = threading.get_ident()
    writer_threads = []

    class Plan:
        nbytes = len(data)

        def write_into(self, sink):
            writer_threads.append(threading.get_ident())
            with memoryview(sink) as destination:
                destination[:] = data

    prepared = view.client.begin_call('echo')
    try:
        response = prepared.call_prepared(Plan())
        try:
            assert pickle.loads(memoryview(response)) == 'prepared'
        finally:
            response.release()
        assert writer_threads == [caller]
        with pytest.raises(ValueError, match='consumed'):
            prepared.call(data)
    finally:
        prepared.close()
        cc.close(view)


def test_native_prepared_writer_escaped_view_is_pinned_and_never_dispatched(ipc_connection):
    connection, resource = ipc_connection
    view = cc.with_call_options(connection, timeout=1)
    escaped = []

    class EscapingPlan:
        nbytes = 16

        def write_into(self, sink):
            escaped.append(memoryview(sink))
            escaped[0][:] = b'a' * 16

    prepared = view.client.begin_call('echo')
    try:
        with pytest.raises(BufferError, match='retained an exported'):
            prepared.call_prepared(EscapingPlan())
        assert bytes(escaped[0]) == b'a' * 16
        escaped[0][:] = b'b' * 16
        assert resource.calls == 0
        assert cc.call_execution_snapshot()['used_operations'] == 0
        assert cc.call_execution_snapshot()['used_retained_bytes'] == 0
        assert connection.echo('after') == 'after'
    finally:
        for destination in escaped:
            destination.release()
        prepared.close()
        cc.close(view)


def test_http_stale_retry_keeps_original_deadline_and_later_call_survives():
    """Two real HTTP endpoints produce a proven stale refusal then slow success."""
    import json
    import pickle
    from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer
    from c_two.crm.contract import crm_contract

    expected = crm_contract(TimedContract)
    calls = [0, 0]
    headers_seen = []
    servers = []
    threads = []
    route_name = 'deadline-retry'

    def route(index):
        return {
            'name': route_name, 'relay_url': f'http://127.0.0.1:{servers[index].server_port}',
            'route_uid': f'deadline-retry-{index}', 'route_revision': 1,
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
                    time.sleep(0.1)
                    refusal = {'version': 1, 'code': 704, 'name': 'RouteStale',
                               'message': 'authoritative stale route', 'details': {}}
                    self.reply(409, json.dumps(refusal).encode())
                else:
                    time.sleep(0.2)
                    value, _business_timeout = pickle.loads(body)
                    self.reply(200, pickle.dumps(value))
        return Handler

    connection = None
    finite = None
    unlimited = None
    try:
        for index in range(2):
            server = ThreadingHTTPServer(('127.0.0.1', 0), handler(index))
            server.daemon_threads = True
            servers.append(server)
            thread = threading.Thread(target=server.serve_forever)
            thread.start()
            threads.append(thread)
        anchor = f'http://127.0.0.1:{servers[0].server_port}'
        connection = cc.connect(TimedContract, name=route_name, address=anchor)
        finite = cc.with_call_options(connection, timeout=0.3)
        with pytest.raises(cc.error.CallDeadlineExceeded) as failure:
            finite.echo('retry')
        assert failure.value.transport_phase == 'dispatch_uncertain'
        assert calls == [1, 1]
        unlimited = cc.with_call_options(connection, timeout=None)
        assert unlimited.echo('later') == 'later'
        assert calls[1] == 2
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
        assert cc.call_execution_snapshot()['used_operations'] == 0
        assert cc.call_execution_snapshot()['used_retained_bytes'] == 0
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
        assert cc.call_execution_snapshot()['used_operations'] == 0
        assert cc.call_execution_snapshot()['used_retained_bytes'] == 0
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
        assert cc.call_execution_snapshot()['used_operations'] == 0
        assert cc.call_execution_snapshot()['used_retained_bytes'] == 0
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
    assert cc.call_execution_snapshot()['used_operations'] == 0
    assert cc.call_execution_snapshot()['used_retained_bytes'] == 0
    assert connection.echo('after') == 'after'


def test_prepared_close_gc_and_escaped_writer_failure_refund_only_pre_dispatch(ipc_connection):
    import gc

    connection, resource = ipc_connection
    view = cc.with_call_options(connection, timeout=1)
    try:
        for explicit_close in [True, False]:
            prepared = view.client.begin_call('echo')
            prepared.charge_input(16)
            assert cc.call_execution_snapshot()['used_operations'] == 1
            assert cc.call_execution_snapshot()['used_retained_bytes'] == 16
            if explicit_close:
                prepared.close()
            del prepared
            gc.collect()
            assert cc.call_execution_snapshot()['used_operations'] == 0
            assert cc.call_execution_snapshot()['used_retained_bytes'] == 0

        escaped = []

        class FailingWriter:
            nbytes = 16

            def write_into(self, sink):
                escaped.append(memoryview(sink))
                escaped[0][:] = b'x' * 16
                raise ValueError('writer failed')

        prepared = view.client.begin_call('echo')
        with pytest.raises(ValueError, match='writer failed'):
            prepared.call_prepared(FailingWriter())
        del prepared
        gc.collect()
        assert bytes(escaped[0]) == b'x' * 16
        escaped[0][0] = ord('y')
        escaped[0].release()
        assert cc.call_execution_snapshot()['used_operations'] == 0
        assert cc.call_execution_snapshot()['used_retained_bytes'] == 0
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
