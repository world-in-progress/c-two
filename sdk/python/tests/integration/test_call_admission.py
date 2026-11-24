"""Host admission and resource call context across Python transports."""
from __future__ import annotations

import threading
from collections.abc import Iterator
from dataclasses import FrozenInstanceError
from types import MappingProxyType
from weakref import ref

import httpx
import pytest

import c_two as cc
from c_two import error
from c_two.crm.contract import crm_contract
from tests.fixtures.persistent_lifecycle_support import wait_state


ContextSnapshot = tuple[str, str, str, str | None, dict[str, str], str | None]


@cc.crm(namespace='test.call-admission', version='1.0.0')
class Calls:
    @cc.read
    def read_context(self, value: str) -> ContextSnapshot: ...

    @cc.write
    def nested(self) -> tuple[ContextSnapshot, ContextSnapshot, ContextSnapshot]: ...

    @cc.stream
    @cc.read
    def steps(self) -> Iterator[ContextSnapshot]: ...

    @cc.topic
    def state(self) -> int: ...


def snapshot():
    context = cc.current_call_context()
    assert isinstance(context, cc.CallContext)
    assert isinstance(context.metadata, MappingProxyType)
    return (
        context.route_name, context.target, context.operation, context.call_id,
        dict(context.metadata), context.extension,
    )


class Resource:
    def __init__(self, inner=None):
        self.inner = inner
        self.unary = []
        self.stream = []
        self.stream_cleaned = threading.Event()

    def read_context(self, value):
        self.unary.append(value)
        return snapshot()

    def nested(self):
        before = snapshot()
        reply = self.inner.read_context('nested')
        return before, reply, snapshot()

    def steps(self):
        self.stream.append(('start', snapshot()))

        def generate():
            try:
                self.stream.append(('next', snapshot()))
                yield snapshot()
                if self.inner is not None:
                    self.inner.read_context('stream nested')
            finally:
                self.stream.append(('cleanup', snapshot()))
                self.stream_cleaned.set()

        return generate()


@pytest.fixture
def runtimes():
    runtimes = [cc.Runtime(), cc.Runtime()]
    try:
        yield runtimes
    finally:
        for runtime in reversed(runtimes):
            assert runtime.shutdown()['completed']


def connect(host, caller, start_c3_relay, transport, *, hook):
    address = None
    if transport in ('relay', 'mesh'):
        origin = start_c3_relay()
        host.set_relay_anchor(origin.url)
        address = origin.url
    host.set_server(admission_hook=hook)
    resource = Resource()
    registration = host.register(Calls, resource, name='calls', initial_topics={'state': 41})
    if transport == 'mesh':
        peer = start_c3_relay(seeds=[origin.url])
        contract = crm_contract(Calls)
        params = dict(zip(
            ('crm_ns', 'crm_name', 'crm_ver', 'abi_hash', 'signature_hash'),
            contract.native_args(),
        ))
        with httpx.Client(trust_env=False, timeout=2) as http:
            response = wait_state(
                lambda: http.get(f'{peer.url}/_resolve/calls', params=params),
                lambda result: result.status_code == 200 and bool(result.json()),
            )
        assert [route['relay_url'] for route in response.json()] == [origin.url]
        # Ordinary calls use the resolved origin. Topics enter through the
        # anchor, which subscribes to the remote Host through the origin.
        address = peer.url
    if transport == 'ipc':
        address = host.server_address()
    runtime = host if transport == 'thread_local' else caller
    proxy = runtime.connect(Calls, name='calls', address=address)
    assert proxy.client.observed_path == {
        'thread_local': 'thread_local', 'ipc': 'direct_ipc',
        'relay': 'explicit_relay', 'mesh': 'explicit_relay',
    }[transport]
    return proxy, resource, registration


@pytest.mark.parametrize('transport', ['thread_local', 'ipc', 'relay', 'mesh'])
def test_admission_and_metadata_on_every_call_surface(runtimes, start_c3_relay, transport):
    seen = []

    def hook(context):
        seen.append(context)
        if context.metadata['trace'] == 'blocked':
            return cc.AdmissionDecision.deny('blocked', details={
                'policy': 'trace', 'dispatch_phase': 'dispatch_uncertain', 'call_id': 'forged',
            })
        return cc.AdmissionDecision.allow(extension='scope')

    proxy, resource, registration = connect(*runtimes, start_c3_relay, transport, hook=hook)
    view = cc.with_call_options(proxy, metadata={'trace': 'allowed'})
    regular = ('calls', 'read_context', 'unary', None, {'trace': 'allowed'}, 'scope')
    assert view.read_context('plain') == regular
    with cc.hold(view.read_context)('held') as held:
        assert held.value == regular
    keyed = (*regular[:3], 'key', *regular[4:])
    assert cc.with_call_id(view.read_context, 'key')('keyed') == keyed
    assert cc.with_call_id(view.read_context, 'key')('keyed') == keyed
    assert cc.recover_call(view.read_context, 'key')() == keyed
    assert cc.query_call(view, 'key').status == 'succeeded'
    assert resource.unary == ['plain', 'held', 'keyed']

    stream_snapshot = ('calls', 'steps', 'server_stream', None, {'trace': 'allowed'}, 'scope')
    with view.steps() as stream:
        assert next(stream) == stream_snapshot
        assert cc.current_call_context() is None
    assert resource.stream_cleaned.wait(3)
    assert resource.stream == [(event, stream_snapshot) for event in ('start', 'next', 'cleanup')]

    with view.state() as first:
        assert next(first).value == 41
        blocked = cc.with_call_options(view, metadata={'trace': 'blocked'})
        for attempt in (
            lambda: blocked.read_context('rejected'),
            lambda: cc.hold(blocked.read_context)('rejected-held'),
            lambda: cc.with_call_id(blocked.read_context, 'key')('keyed'),
            cc.recover_call(blocked.read_context, 'key'),
            lambda: cc.query_call(blocked, 'key'),
            blocked.state,
        ):
            with pytest.raises(error.AdmissionDenied) as refused:
                attempt()
            assert refused.value.transport_phase == 'pre_dispatch'
            assert refused.value.message == 'blocked'
            assert refused.value.details['policy'] == 'trace'
            assert refused.value.details.get('call_id') != 'forged'
        # Relay stream opening consumes the HTTP 200. The denial must arrive
        # as the first stream record, before any resource item was produced.
        opened = blocked.steps() if transport in ('relay', 'mesh') else None
        with pytest.raises(error.AdmissionDenied) as refused:
            with (opened if opened is not None else blocked.steps()) as stream:
                next(stream)
        assert refused.value.transport_phase == 'pre_dispatch'
        with view.client.subscribe('state', start='next', timeout=None) as other:
            assert registration.publisher('state').publish(42) == 2
            assert next(first).value == next(other).value == 42
    assert resource.unary == ['plain', 'held', 'keyed']
    assert len(resource.stream) == 3
    assert cc.current_call_context() is None

    ipc = 'ipc'
    direct = 'thread_local' if transport == 'thread_local' else ipc
    expected = [
        ('unary', 'read_context', 'read', direct, None),
        ('unary', 'read_context', 'read', direct, None),
        ('unary', 'read_context', 'read', ipc, 'key'),
        ('unary', 'read_context', 'read', ipc, 'key'),
        ('recover', 'read_context', 'read', ipc, 'key'),
        ('query_outcome', None, None, ipc, 'key'),
        ('server_stream', 'steps', 'read', direct, None),
        ('subscribe', 'state', None, direct, None),
    ]
    assert [(c.operation, c.target, c.access, c.transport, c.call_id) for c in seen] == [
        *expected, *expected[:3], *expected[4:6], expected[7], expected[6], expected[7],
    ]
    assert [c.metadata['trace'] for c in seen] == ['allowed'] * 8 + ['blocked'] * 7 + ['allowed']
    first = seen[0]
    assert (first.route_name, first.crm_ns, first.crm_name, first.crm_ver) == (
        'calls', 'test.call-admission', 'Calls', '1.0.0',
    )
    with pytest.raises(FrozenInstanceError):
        first.target = None
    with pytest.raises(TypeError):
        first.metadata['trace'] = 'changed'


@pytest.mark.parametrize('transport', ['thread_local', 'ipc'])
def test_invalid_hook_results_fail_closed(runtimes, start_c3_relay, transport):
    _, downstream = runtimes
    downstream.set_server(admission_hook=lambda ctx: cc.AdmissionDecision.deny('inner refusal'))
    downstream.register(Calls, Resource(), name='downstream')
    inner = downstream.connect(Calls, name='downstream')

    def raises(exc):
        raise exc

    cases = {
        'none': lambda: None,
        'allowed': lambda: cc.AdmissionDecision.allow(extension='extension'),
        'exception': lambda: raises(ValueError('private details')),
        'downstream': lambda: inner.read_context('denied'),
        'false': lambda: False,
        'number': lambda: 123,
        'text': lambda: 'ok',
        'message': lambda: cc.AdmissionDecision.deny(3),
        'details': lambda: cc.AdmissionDecision.deny(details={'reason': 3}),
        'key': lambda: cc.AdmissionDecision.deny(details={1: 'text'}),
    }
    proxy, resource, _ = connect(
        *runtimes, start_c3_relay, transport, hook=lambda ctx: cases[ctx.metadata['case']](),
    )
    for label in cases:
        view = cc.with_call_options(proxy, metadata={'case': label})
        if label in ('none', 'allowed'):
            context = view.read_context(label)
            assert context[-1] == (None if label == 'none' else 'extension')
            continue
        with pytest.raises(error.AdmissionDenied) as refused:
            view.read_context(label)
        assert refused.value.code == 724
        assert refused.value.message == 'admission hook failed'
        assert refused.value.details['reason'] == 'hook_error'
        assert refused.value.transport_phase == 'pre_dispatch'
    assert resource.unary == ['none', 'allowed']


def test_hook_freeze_restart_and_module_level_configuration(runtimes, monkeypatch):
    from c_two.transport import registry

    host, caller = runtimes
    monkeypatch.setattr(registry, '_default_runtime', host)
    seen = []

    class Hook:
        def __call__(self, context):
            seen.append(context)

        def __eq__(self, other):
            return True

    first, different = Hook(), Hook()
    with pytest.raises(TypeError):
        cc.set_server(admission_hook=False)
    cc.set_server(admission_hook=different)
    host.set_server(admission_hook=None)
    cc.set_server(admission_hook=first)
    for address_mode in ('ipc', 'thread_local'):
        host.register(Calls, Resource(), name='calls')
        cc.set_server()
        cc.set_server(admission_hook=first)
        for replacement in (None, different):
            with pytest.raises(error.CoreError) as frozen:
                host.set_server(admission_hook=replacement)
            assert frozen.value.lifecycle_kind == 'config_frozen'
        if address_mode == 'ipc':
            proxy = caller.connect(Calls, name='calls', address=host.server_address())
        else:
            proxy = host.connect(Calls, name='calls')
        with proxy:
            assert proxy.read_context('accepted')[-2:] == ({}, None)
        assert host.shutdown()['completed']
    assert [context.transport for context in seen] == ['ipc', 'thread_local']


@pytest.mark.parametrize('transport', ['thread_local', 'ipc'])
def test_nested_calls_shadow_and_restore_context_and_isolate_denials(runtimes, transport):
    outer, inner = runtimes
    inner.register(Calls, Resource(), name='inner')
    inner_proxy = inner.connect(Calls, name='inner')
    outer.set_server(admission_hook=lambda ctx: cc.AdmissionDecision.allow(extension='outer'))
    resource = Resource(inner_proxy)
    outer.register(Calls, resource, name='outer')
    if transport == 'ipc':
        proxy = outer.connect(Calls, name='outer', address=outer.server_address())
    else:
        proxy = outer.connect(Calls, name='outer')
    view = cc.with_call_options(proxy, metadata={'trace': 'outer'})
    before, inside, after = view.nested()
    assert before == after == ('outer', 'nested', 'unary', None, {'trace': 'outer'}, 'outer')
    assert inside == ('inner', 'read_context', 'unary', None, {}, None)
    assert cc.current_call_context() is None

    assert inner.shutdown()['completed']
    inner.set_server(admission_hook=lambda ctx: cc.AdmissionDecision.deny('inner refusal'))
    inner.register(Calls, Resource(), name='inner')
    resource.inner = inner.connect(Calls, name='inner')
    def assert_isolated(escaped):
        assert escaped.code == 3
        assert escaped.transport_phase is None
        assert escaped.details['cause_code'] == '724'
        assert escaped.details['cause_message'] == 'inner refusal'

    with pytest.raises(error.ResourceExecutionError) as escaped:
        view.nested()
    assert_isolated(escaped.value)
    with view.steps() as stream:
        assert next(stream) == ('outer', 'steps', 'server_stream', None, {'trace': 'outer'}, 'outer')
        with pytest.raises(error.ResourceExecutionError) as escaped:
            next(stream)
        assert_isolated(escaped.value)
    assert resource.stream_cleaned.wait(3)
    assert cc.current_call_context() is None


@pytest.mark.parametrize('operation', ['unary', 'server_stream', 'subscribe'])
def test_thread_call_rechecks_admission_after_same_name_reregistration(runtimes, operation):
    host, _ = runtimes
    old = Resource()
    new = Resource()
    checked = []
    released, weak = [], []

    class Extension:
        def __del__(self):
            released.append('old registration released')

    def hook(context):
        checked.append(context)
        if len(checked) == 1:
            extension = Extension()
            weak.append(ref(extension))
            host.unregister('calls')
            host.register(Calls, new, name='calls', initial_topics={'state': 2})
            return cc.AdmissionDecision.allow(extension=extension)
        assert weak[0]() is None
        return cc.AdmissionDecision.allow(extension='new registration')

    host.set_server(admission_hook=hook)
    host.register(Calls, old, name='calls', initial_topics={'state': 1})
    proxy = cc.with_call_options(host.connect(Calls, name='calls'), metadata={'trace': 'follow'})
    if operation == 'unary':
        assert proxy.read_context('new')[-1] == 'new registration'
        assert new.unary == ['new']
    elif operation == 'server_stream':
        with proxy.steps() as stream:
            assert next(stream)[-1] == 'new registration'
    else:
        with proxy.state() as subscription:
            assert next(subscription).value == 2
    assert not old.unary and not old.stream
    assert [c.operation for c in checked] == [operation, operation]
    assert [dict(c.metadata) for c in checked] == [{'trace': 'follow'}] * 2
    assert released == ['old registration released']


def test_outcome_timeout_override_preserves_metadata_and_rejects_before_connect(runtimes):
    host, _ = runtimes
    seen = []
    host.set_server(admission_hook=lambda ctx: seen.append(ctx))
    host.register(Calls, Resource(), name='calls')
    proxy = host.connect(Calls, name='calls')
    expired = cc.with_call_options(proxy, timeout=0, metadata={'trace': 'budget'})
    for attempt in (
        lambda: cc.with_call_id(expired.read_context, 'key')('call'),
        cc.recover_call(expired.read_context, 'key'),
        lambda: cc.query_call(expired, 'key'),
    ):
        with pytest.raises(error.CallDeadlineExceeded) as refused:
            attempt()
        assert refused.value.transport_phase == 'pre_dispatch'
    assert not seen
    assert not host._runtime_session.client_config_frozen
    assert cc.query_call(expired, 'key', timeout=...).status == 'unknown'
    assert cc.query_call(expired, 'key', timeout=2).status == 'unknown'
    bounded = cc.with_call_options(expired, timeout=2)
    assert cc.with_call_id(bounded.read_context, 'key')('call')[-2] == {'trace': 'budget'}
    assert cc.recover_call(bounded.read_context, 'key')()[-2] == {'trace': 'budget'}
    assert cc.query_call(expired, 'key', timeout=None).status == 'succeeded'
    assert all(c.transport == 'ipc' and c.metadata == {'trace': 'budget'} for c in seen)


def test_replaced_callable_can_reenter_server_configuration_from_its_destructor(runtimes):
    host, _ = runtimes
    destroyed = []

    class Hook:
        def __call__(self, context):
            return None

        def __del__(self):
            try:
                host.set_server(server_id='configured-by-destructor')
            except BaseException as exc:
                destroyed.append(exc)
            else:
                destroyed.append('finished')

    hook = Hook()
    observed = ref(hook)
    host.set_server(admission_hook=hook)
    del hook
    assert observed() is not None
    host.set_server(admission_hook=None)
    assert observed() is None
    assert destroyed == ['finished']


@pytest.mark.parametrize('transport', ['thread_local', 'ipc'])
def test_unused_extensions_are_released_without_locking_out_python(runtimes, start_c3_relay, transport):
    from queue import Queue

    host, _ = runtimes
    released = Queue()
    admitted_keys = set()

    class Extension:
        def __init__(self, operation):
            self.operation = operation

        def __del__(self):
            try:
                host.set_server()
            except BaseException as exc:
                released.put(exc)
            else:
                released.put(self.operation)

    def hook(context):
        if context.operation != 'unary':
            return cc.AdmissionDecision.allow(extension=Extension(context.operation))
        if context.call_id in admitted_keys:
            return cc.AdmissionDecision.allow(extension=Extension('duplicate'))
        admitted_keys.add(context.call_id)

    proxy, _, _ = connect(*runtimes, start_c3_relay, transport, hook=hook)
    first = cc.with_call_id(proxy.read_context, 'key')('call')
    assert first[-1] is None
    assert cc.with_call_id(proxy.read_context, 'key')('call') == first
    assert released.get(timeout=3) == 'duplicate'
    assert cc.recover_call(proxy.read_context, 'key')()[-1] is None
    assert released.get(timeout=3) == 'recover'
    assert cc.query_call(proxy, 'key').status == 'succeeded'
    assert released.get(timeout=3) == 'query_outcome'
    with proxy.state():
        assert released.get(timeout=3) == 'subscribe'


def test_relay_stream_http_403_preserves_admission_denial(runtimes):
    import json
    from http.server import BaseHTTPRequestHandler

    from tests.fixtures.http_server import LoopbackHTTPServer

    contract = crm_contract(Calls)
    seen = []

    class Relay(BaseHTTPRequestHandler):
        def log_message(self, *_args):
            pass

        def reply(self, status, data):
            self.send_response(status)
            self.send_header('Content-Length', str(len(data)))
            self.send_header('Content-Type', 'application/json')
            self.end_headers()
            self.wfile.write(data)

        def do_GET(self):
            if self.path.startswith('/_probe/'):
                self.reply(200, b'')
                return
            assert self.path.startswith('/_resolve/calls?')
            self.reply(200, json.dumps([{
                'name': 'calls',
                'relay_url': f'http://127.0.0.1:{self.server.server_port}',
                'crm_ns': contract.crm_ns, 'crm_name': contract.crm_name,
                'crm_ver': contract.crm_ver, 'abi_hash': contract.abi_hash,
                'signature_hash': contract.signature_hash, 'max_payload_size': 65536,
            }]).encode())

        def do_POST(self):
            self.rfile.read(int(self.headers.get('Content-Length', '0')))
            seen.append((self.path, self.headers.get('x-c2-meta-trace')))
            self.reply(403, json.dumps({
                'version': 1, 'code': 724, 'name': 'AdmissionDenied',
                'message': 'early refusal', 'details': {'dispatch_phase': 'pre_dispatch'},
            }).encode())

    _, caller = runtimes
    with LoopbackHTTPServer(('127.0.0.1', 0), Relay) as relay:
        worker = threading.Thread(target=relay.serve_forever)
        worker.start()
        try:
            address = f'http://127.0.0.1:{relay.server_port}'
            proxy = caller.connect(Calls, name='calls', address=address)
            view = cc.with_call_options(proxy, metadata={'trace': 'early'})
            with pytest.raises(error.AdmissionDenied, match='early refusal') as refused:
                view.steps()
            assert refused.value.transport_phase == 'pre_dispatch'
        finally:
            relay.shutdown()
            worker.join()
    assert seen == [('/_c2/calls/call?route=calls&method=steps', 'early')]


def test_start_and_hook_configuration_publish_a_single_frozen_hook(runtimes):
    from concurrent.futures import ThreadPoolExecutor

    host, _ = runtimes
    barrier = threading.Barrier(2)

    def old(context):
        return cc.AdmissionDecision.allow(extension='old')

    def new(context):
        return cc.AdmissionDecision.allow(extension='new')

    def start():
        barrier.wait(timeout=3)
        host.start()

    def configure():
        barrier.wait(timeout=3)
        try:
            host.set_server(admission_hook=new)
        except error.CoreError as frozen:
            assert frozen.lifecycle_kind == 'config_frozen'
            return old, 'old'
        return new, 'new'

    host.set_server(admission_hook=old)
    with ThreadPoolExecutor(max_workers=2) as pool:
        startup = pool.submit(start)
        configured = pool.submit(configure)
        startup.result(timeout=5)
        hook, label = configured.result(timeout=5)
    host.register(Calls, Resource(), name='calls')
    host.set_server(admission_hook=hook)
    proxy = host.connect(Calls, name='calls')
    assert proxy.read_context('call')[-1] == label


def test_failed_thread_resource_call_releases_context_with_error_still_live(runtimes):
    host, _ = runtimes
    owners = []

    class Extension:
        pass

    def hook(context):
        extension = Extension()
        owners.append(ref(extension))
        return cc.AdmissionDecision.allow(extension=extension)

    class FailingResource(Resource):
        def read_context(self, value):
            assert isinstance(cc.current_call_context().extension, Extension)
            raise RuntimeError('resource failed')

    host.set_server(admission_hook=hook)
    host.register(Calls, FailingResource(), name='calls')
    proxy = host.connect(Calls, name='calls')
    with pytest.raises(error.ResourceExecutionError) as failure:
        proxy.read_context('call')
    assert failure.value.__traceback__ is not None
    assert len(owners) == 1
    assert owners[0]() is None
    assert cc.current_call_context() is None
