"""Cross-process direct IPC 17 publish/subscribe (ADR 0005 §2–§4).

The server Runtime hosts the route in this process while clients attach
through the real IPC 17 endpoint (``connect(address=...)``), exercising the
wire subscribe records, multi-loan SHM fan-out and terminal outcomes. One
test drives a real separate host process.
"""
from __future__ import annotations

import signal
import subprocess
import sys
import threading
import time

import pytest

import c_two as cc
from c_two.error import (
    CCBaseError,
    CallCancelled,
    ProtocolViolation,
    ResourceClosed,
    SubscriptionLagged,
    UnsupportedCallMode,
)


@cc.crm(namespace='test.pubsub.ipc', version='0.4.0')
class Telemetry:
    @cc.read
    def device(self) -> str: ...

    @cc.topic
    def state(self) -> dict: ...

    @cc.topic
    def burst(self) -> int: ...


class TelemetryImpl:
    def device(self) -> str:
        return 'sensor-ipc'


def _wait_until(predicate, timeout: float = 5.0, interval: float = 0.02):
    deadline = time.monotonic() + timeout
    while time.monotonic() < deadline:
        if predicate():
            return True
        time.sleep(interval)
    return False


@pytest.fixture(autouse=True)
def _clean_env(monkeypatch):
    monkeypatch.setenv('C2_ENV_FILE', '')
    monkeypatch.setenv('C2_RELAY_ANCHOR_ADDRESS', '')


@pytest.fixture()
def server_client():
    server = cc.Runtime()
    client = cc.Runtime()
    yield server, client
    # A subscription holds its client-domain slot through its real terminal,
    # so a test that failed before closing one leaves native cleanup running;
    # bound this caller's wait and let the driver finish in the background.
    client.shutdown(timeout=5.0)
    server.shutdown(timeout=5.0)


def _register(server: cc.Runtime, **kwargs) -> cc.Registration:
    return server.register(Telemetry, TelemetryImpl(), name='sim', **kwargs)


# ------------------------------------------------------------------
# Direct IPC 17 subscribe behavior
# ------------------------------------------------------------------


class TestDirectIpcTopics:

    def test_subscribe_deliveries_multiplex_with_unary_calls(self, server_client):
        server, client = server_client
        registration = _register(
            server, topic_capacity={'burst': 2},
            initial_topics={'state': {'boot': True}},
        )
        address = server.server_address()
        assert address is not None

        proxy = client.connect(Telemetry, name='sim', address=address)
        subscription = proxy.client.subscribe('state')
        assert subscription.start_seq == 1

        publisher = registration.publisher('state')
        assert publisher.publish({'n': 1}) == 2

        # Unary calls stay usable on the same connection while the
        # subscription is open.
        assert proxy.device() == 'sensor-ipc'
        delivery = next(subscription)
        assert delivery.value == {'boot': True}
        assert (delivery.seq, delivery.dropped) == (1, 0)

        delivery = next(subscription)
        assert delivery.value == {'n': 1}
        receipt = subscription.close(timeout=5.0)
        assert receipt['completed'] is True
        assert registration.active_subscribers == 0

    def test_multiple_subscribers_share_one_sealed_frame_over_shm(self, server_client):
        server, client = server_client
        # A modest server memory budget keeps the buddy pool small so a 5 MiB
        # payload takes a dedicated sealed SHM block (mirrors the Core proof).
        server.set_server(memory_budget_bytes=16 * 1024 * 1024)
        other_client = cc.Runtime()
        try:
            registration = _register(server, topic_capacity={'state': 1})
            address = server.server_address()

            first = client.connect(Telemetry, name='sim', address=address)
            second = other_client.connect(Telemetry, name='sim', address=address)
            sub_a = first.client.subscribe('state')
            sub_b = second.client.subscribe('state')

            def server_used() -> int:
                memory = server.memory_stats()['server']['memory']
                return memory['used_bytes'] if memory is not None else 0

            # The host seals one dedicated SHM frame and multi-loans it to
            # both subscribers instead of writing one copy per subscriber:
            # one 5 MiB charge, not two, while both are outstanding.
            blob = {'payload': bytes(5 * 1024 * 1024)}
            publisher = registration.publisher('state')
            assert publisher.publish(blob) == 1
            d_a = next(sub_a)
            d_b = next(sub_b)
            assert (d_a.seq, d_b.seq) == (1, 1)
            assert len(d_a.value['payload']) == 5 * 1024 * 1024
            assert d_a.value['payload'] == d_b.value['payload']
            assert server_used() < 2 * 5 * 1024 * 1024

            # A tiny frame replaces the big one in the capacity-1 ring; after
            # both subscribers copied and released their loans, the sealed
            # dedicated block is reclaimed.
            assert publisher.publish({'tiny': True}) == 2
            assert next(sub_a).value == {'tiny': True}
            assert next(sub_b).value == {'tiny': True}

            assert _wait_until(lambda: server_used() < 5 * 1024 * 1024), (
                'sealed dedicated SHM frame was not reclaimed after both peers released'
            )
            assert sub_a.close(timeout=5.0)['completed']
            assert sub_b.close(timeout=5.0)['completed']
        finally:
            other_client.shutdown(timeout=5.0)

    def test_hold_delivery_retains_local_copy_over_ipc(self, server_client):
        server, client = server_client
        registration = _register(server)
        address = server.server_address()

        proxy = client.connect(Telemetry, name='sim', address=address)
        subscription = proxy.client.subscribe('state', hold=True)
        registration.publisher('state').publish({'held': 'over-ipc'})
        delivery = next(subscription)
        assert isinstance(delivery.value, cc.HeldResult)
        assert delivery.value.value == {'held': 'over-ipc'}
        subscription.close(timeout=5.0)
        # The retained local copy survives the subscription's terminal state.
        assert delivery.value.value == {'held': 'over-ipc'}
        delivery.value.release()
        with pytest.raises(RuntimeError):
            _ = delivery.value.value

    def test_unknown_topic_is_713_over_ipc(self, server_client):
        server, client = server_client
        _register(server)
        address = server.server_address()
        proxy = client.connect(Telemetry, name='sim', address=address)
        with pytest.raises(ProtocolViolation) as violation:
            proxy.client.subscribe('nope')
        assert violation.value.code == 713

    def test_on_lag_error_terminates_with_720_over_ipc(self, server_client):
        server, client = server_client
        registration = _register(server, topic_capacity={'burst': 2})
        address = server.server_address()
        proxy = client.connect(Telemetry, name='sim', address=address)

        strict = proxy.client.subscribe('burst', on_lag='error')
        publisher = registration.publisher('burst')
        assert publisher.publish(1) == 1
        assert next(strict).value == 1
        # Let the client channel buffer seq 2 and grant demand, then overflow
        # the capacity-2 ring so the next expected seq is evicted (mirrors the
        # Core IPC 17 lag test's deterministic recipe).
        assert publisher.publish(2) == 2
        time.sleep(0.05)
        for beat in (3, 4, 5, 6, 7):
            publisher.publish(beat)

        deliveries = []
        with pytest.raises(SubscriptionLagged) as lagged:
            for delivery in strict:
                deliveries.append(delivery.seq)
        assert lagged.value.code == 720
        assert lagged.value.details['topic'] == 'burst'
        assert strict.close(timeout=5.0)['completed']

    def test_unregister_ends_718_and_next_subscribe_follows_new_registration(
        self, server_client,
    ):
        server, client = server_client
        registration = _register(
            server, initial_topics={'state': {'generation': 1}},
        )
        address = server.server_address()
        proxy = client.connect(Telemetry, name='sim', address=address)

        subscription = proxy.client.subscribe('state')
        registration.publisher('state').publish({'n': 1})
        assert next(subscription).value == {'generation': 1}
        assert next(subscription).value == {'n': 1}

        server.unregister('sim')
        with pytest.raises(CallCancelled) as cancelled:
            next(subscription)
        assert cancelled.value.code == 718
        assert cancelled.value.details['cause'] == 'route_unregistered'

        # A later subscribe() follows the current registration (ADR 0004):
        # fresh sequence space seeded by the new initial frame.
        second = _register(
            server, initial_topics={'state': {'generation': 2}},
        )
        following = proxy.client.subscribe('state')
        assert following.start_seq == 1
        assert next(following).value == {'generation': 2}
        assert second.publisher('state').publish({'n': 1}) == 2
        assert next(following).value == {'n': 1}
        following.close(timeout=5.0)

    def test_host_shutdown_ends_subscription_with_718(self, server_client):
        server, client = server_client
        registration = _register(server)
        address = server.server_address()
        proxy = client.connect(Telemetry, name='sim', address=address)
        subscription = proxy.client.subscribe('state')

        server.shutdown()
        with pytest.raises(CCBaseError) as ended:
            next(subscription)
        # A stopped host ends the stream between steps with a truthful cause;
        # a transport-ended connection reports its dispatch-uncertain peer end.
        assert isinstance(ended.value, CallCancelled) or ended.value.code in (707, 705)
        if isinstance(ended.value, CallCancelled):
            assert ended.value.details['cause'] in ('host_shutdown', 'route_unregistered')


# ------------------------------------------------------------------
# Real separate host process
# ------------------------------------------------------------------

HOST_SOURCE = r'''
import time

import c_two as cc


# Must declare the identical contract (methods and topics) as the test
# module's Telemetry: topics participate in the route identity.
@cc.crm(namespace='test.pubsub.ipc', version='0.4.0')
class Telemetry:
    @cc.read
    def device(self) -> str: ...

    @cc.topic
    def state(self) -> dict: ...

    @cc.topic
    def burst(self) -> int: ...


class Impl:
    def device(self) -> str:
        return 'sensor-host'


registration = cc.register(
    Telemetry, Impl(), name='sim', topic_capacity={'state': 8},
    initial_topics={'state': {'boot': 0}},
)
publisher = registration.publisher('state')
print(cc.server_address(), flush=True)
counter = 0
try:
    while True:
        counter += 1
        publisher.publish({'n': counter})
        time.sleep(0.02)
except Exception:
    pass
'''


# ------------------------------------------------------------------
# Natural topic call face over IPC 17
# ------------------------------------------------------------------


class TestNaturalTopicCalls:

    def test_topic_members_hold_and_call_options_over_ipc(self, server_client):
        server, client = server_client
        registration = _register(
            server, topic_capacity={'burst': 4},
            initial_topics={'state': {'boot': 1}},
        )
        address = server.server_address()
        publisher = registration.publisher('state')

        telemetry = client.connect(Telemetry, name='sim', address=address)
        # The declared member attaches a subscription with default options;
        # `latest` sees the initial frame committed before the route opened.
        subscription = telemetry.state()
        assert subscription.topic_name == 'state'
        assert subscription.start_seq == 1
        assert next(subscription).value == {'boot': 1}
        publisher.publish({'n': 1})
        assert next(subscription).value == {'n': 1}
        subscription.close(timeout=5.0)

        # cc.hold reuses the shared hold wrapper: retained local copies whose
        # .value owns the release, still valid after the subscription ends.
        # `latest` first replays the currently retained frame, then the newly
        # published one.
        held_stream = cc.hold(telemetry.state)()
        publisher.publish({'n': 2})
        retained = next(held_stream)
        assert isinstance(retained.value, cc.HeldResult)
        assert retained.value.value == {'n': 1}
        assert next(held_stream).value.value == {'n': 2}
        held_stream.close(timeout=5.0)
        assert retained.value.value == {'n': 1}
        retained.value.release()

        # with_call_options views carry the subscription policy: an unlimited
        # view subscribes exactly like the base connection.
        unlimited_view = cc.with_call_options(telemetry, timeout=None)
        following = unlimited_view.state(start='next')
        publisher.publish({'n': 3})
        assert next(following).value == {'n': 3}
        following.close(timeout=5.0)
        cc.close(telemetry)

    def test_with_call_options_deadline_bounds_a_remote_subscription(self, server_client):
        server, client = server_client
        _register(server)
        address = server.server_address()
        telemetry = client.connect(Telemetry, name='sim', address=address)

        # A finite view timeout bounds the subscription: with nothing ever
        # published, the next delivery expires against the view's deadline
        # instead of waiting forever.
        from c_two.error import CallDeadlineExceeded

        bounded = cc.with_call_options(telemetry, timeout=0.5).state(start='next')
        started = time.monotonic()
        with pytest.raises(CallDeadlineExceeded):
            next(bounded)
        assert time.monotonic() - started < 5.0
        bounded.close(timeout=5.0)
        cc.close(telemetry)

    def test_finite_explicit_timeout_on_thread_view_is_refused(self, server_client):
        server, _client = server_client
        _register(server)
        telemetry = server.connect(Telemetry, name='sim')
        with pytest.raises(UnsupportedCallMode):
            telemetry.client.subscribe('state', timeout=1.0)
        with pytest.raises(UnsupportedCallMode):
            cc.with_call_options(telemetry, timeout=1.0).state()
        # Unlimited and inherited views stay usable on the same-process path.
        subscription = cc.with_call_options(telemetry, timeout=None).state()
        subscription.close(timeout=5.0)

    def test_topic_only_contract_reregistration_follows_over_ipc(self, server_client):
        """A 0-method, topic-only contract follows re-registration on IPC too."""

        @cc.crm(namespace='test.pubsub.ipc.topics_only', version='0.4.0')
        class Fields:
            @cc.topic
            def field(self) -> list: ...

        class FieldsImpl:
            pass

        server, client = server_client
        server.register(
            Fields, FieldsImpl(), name='fields', initial_topics={'field': [1]},
        )
        address = server.server_address()
        crm = client.connect(Fields, name='fields', address=address)

        subscription = crm.field()
        assert subscription.start_seq == 1
        assert next(subscription).value == [1]

        server.unregister('fields')
        with pytest.raises(CallCancelled):
            next(subscription)
        subscription.close(timeout=5.0)

        second = server.register(
            Fields, FieldsImpl(), name='fields', initial_topics={'field': [2]},
        )
        # The same CRM's next member call follows the new registration.
        following = crm.field()
        assert following.start_seq == 1
        assert next(following).value == [2]
        assert second.publisher('field').publish([3]) == 2
        assert next(following).value == [3]
        following.close(timeout=5.0)
        cc.close(crm)


# ------------------------------------------------------------------
# Registration handle liveness during a blocking drain
# ------------------------------------------------------------------


class TestRegistrationHandleDuringDrain:

    @pytest.mark.platform('posix')
    def test_unregister_drain_never_deadlocks_the_registration_handle(self, server_client):
        """Handle reads stay non-blocking while unregister drains a stalled subscriber.

        The unregistering close must take the inner Registration out under the
        lock and drain strictly outside it: while ``wait_routes_drained`` waits
        for a subscriber stuck mid-frame, ``is_closed`` reports True and
        ``publisher()`` refuses with 707 immediately instead of queueing on
        the same mutex.
        """
        server, _client = server_client
        registration = _register(server, topic_capacity={'state': 4})
        publisher = registration.publisher('state')
        address = server.server_address()

        subscriber = subprocess.Popen(
            [sys.executable, '-c', _STALL_SUBSCRIBER_SOURCE, address],
            stdout=subprocess.PIPE, stderr=subprocess.STDOUT, text=True,
        )
        try:
            assert subscriber.stdout.readline().strip() == 'ATTACHED'
            time.sleep(0.3)
            # Freeze the subscriber's socket: the in-flight large frame can
            # no longer drain, so the registration's drain barrier must wait
            # for the stalled delivery to finish after the signal resumes.
            subscriber.send_signal(signal.SIGSTOP)
            time.sleep(0.2)
            assert publisher.publish({'blob': bytes(32 * 1024 * 1024)}) == 1

            outcome = {}

            def run_unregister() -> None:
                try:
                    server.unregister('sim')
                    outcome['ok'] = True
                except BaseException as exc:  # pragma: no cover - reported below
                    outcome['error'] = repr(exc)

            unregister = threading.Thread(target=run_unregister)
            unregister.start()
            try:
                assert _wait_until(lambda: registration.is_closed), (
                    'registration handle never sealed during the drain'
                )
                started = time.monotonic()
                with pytest.raises(ResourceClosed) as closed:
                    registration.publisher('state')
                assert closed.value.code == 707
                assert time.monotonic() - started < 1.0, (
                    'publisher() blocked behind the unregister drain'
                )
                started = time.monotonic()
                registration.active_subscribers
                assert time.monotonic() - started < 1.0
                assert unregister.is_alive(), 'drain finished before the stall ended'

                # Resuming the subscriber lets the stalled frame finish, the
                # subscription end with its terminal and the drain complete.
                subscriber.send_signal(signal.SIGCONT)
            finally:
                unregister.join(timeout=20.0)
            assert outcome.get('ok'), outcome
        finally:
            if subscriber.poll() is None:
                subscriber.send_signal(signal.SIGCONT)
                subscriber.terminate()
            subscriber.wait(timeout=10)


_STALL_SUBSCRIBER_SOURCE = '''
import sys
import time

import c_two as cc


@cc.crm(namespace='test.pubsub.ipc', version='0.4.0')
class Telemetry:
    @cc.read
    def device(self) -> str: ...

    @cc.topic
    def state(self) -> dict: ...

    @cc.topic
    def burst(self) -> int: ...


client = cc.Runtime()
subscription = client.connect(Telemetry, name='sim', address=sys.argv[1]).state(
    start='next',
)
print('ATTACHED', flush=True)
time.sleep(60)
'''


# ------------------------------------------------------------------
# Real separate host process
# ------------------------------------------------------------------


class TestSeparateHostProcess:

    def test_cross_process_subscription_until_host_exit(self):
        host = subprocess.Popen(
            [sys.executable, '-c', HOST_SOURCE],
            stdout=subprocess.PIPE, stderr=subprocess.STDOUT, text=True,
        )
        client = cc.Runtime()
        try:
            address = host.stdout.readline().strip()
            assert address.startswith('ipc://')
            proxy = client.connect(Telemetry, name='sim', address=address)
            subscription = proxy.client.subscribe('state', start='next')
            assert subscription.start_seq >= 1

            seen = []
            for _ in range(5):
                delivery = next(subscription)
                seen.append((delivery.seq, delivery.value['n']))
            seqs = [seq for seq, _ in seen]
            assert seqs == sorted(seqs)
            assert len({seq for seq in seqs}) == len(seqs)
            assert seen[0][1] >= 1

            # Killing the host process ends the subscription via peer end.
            host.terminate()
            host.wait(timeout=10)
            with pytest.raises(CCBaseError):
                next(subscription)
        finally:
            client.shutdown()
            if host.poll() is None:
                host.terminate()
                host.wait(timeout=10)
