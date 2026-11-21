"""Same-process publish/subscribe surface (ADR 0005 §1).

Covers ``@cc.topic`` declaration rules, descriptor and fingerprint
projections, the ``Registration`` return value with registration-scoped
publishers, initial-state semantics and same-process delivery behavior
through the native local topic channel.
"""
from __future__ import annotations

import gc
import json

import pytest

import c_two as cc
from fastdb4py.payload import BuildPolicy, Builder, CompiledSpec, Payload
from c_two.crm.descriptor import (
    build_contract_descriptor,
    build_portable_contract_descriptor,
    diagnostics,
)
from c_two.crm.methods import rpc_method_names
from c_two.crm.topic import topic_names
from c_two.error import (
    CallCancelled,
    ProtocolViolation,
    ResourceClosed,
    SerializationError,
    SubscriptionLagged,
)

FASTDB_SPEC = {
    "schema": "fastdb.payload.v1",
    "profile": "record.v1",
    "entries": [
        {
            "id": "value",
            "cardinality": "one",
            "type": {"kind": "u8", "nullable": False},
        }
    ],
    "components": [],
}


@cc.crm(namespace='test.pubsub', version='0.3.0')
class Telemetry:
    @cc.read
    def device(self) -> str: ...

    @cc.topic
    def state(self) -> dict: ...

    @cc.topic
    def heartbeat(self) -> int: ...


class TelemetryImpl:
    def device(self) -> str:
        return 'sensor-1'


@cc.crm(namespace='test.pubsub.topics_only', version='0.3.0')
class TopicsOnly:
    @cc.topic
    def field(self) -> list: ...


class TopicsOnlyImpl:
    pass


@pytest.fixture(autouse=True)
def _fresh_runtime(monkeypatch):
    monkeypatch.setenv('C2_ENV_FILE', '')
    monkeypatch.setenv('C2_RELAY_ANCHOR_ADDRESS', '')
    runtime = cc.Runtime()
    yield runtime
    runtime.shutdown()


def _client(runtime: cc.Runtime, crm_class, name: str):
    return runtime.connect(crm_class, name=name).client


# ------------------------------------------------------------------
# @cc.topic declaration rules
# ------------------------------------------------------------------


class TestTopicDeclaration:

    def test_topic_takes_no_parameters_besides_self(self):
        with pytest.raises(TypeError, match='no parameters besides self'):

            @cc.crm(namespace='test.pubsub.bad', version='0.1.0')
            class Bad:
                @cc.topic
                def state(self, extra: int) -> dict: ...

    def test_topic_rejects_read_access_decorator_mixing(self):
        with pytest.raises(TypeError, match='access category'):

            @cc.crm(namespace='test.pubsub.bad', version='0.1.0')
            class Bad:
                @cc.read
                @cc.topic
                def state(self) -> dict: ...

    def test_topic_rejects_write_access_decorator_mixing(self):
        with pytest.raises(TypeError, match='access category'):

            @cc.crm(namespace='test.pubsub.bad', version='0.1.0')
            class Bad:
                @cc.write
                @cc.topic
                def state(self) -> dict: ...

    def test_topic_rejects_stream_mixing(self):
        with pytest.raises(TypeError, match='@cc.stream cannot be combined'):

            @cc.crm(namespace='test.pubsub.bad', version='0.1.0')
            class Bad:
                @cc.stream
                @cc.topic
                def state(self) -> dict: ...

    def test_topic_rejects_on_shutdown_mixing(self):
        with pytest.raises(TypeError, match='@cc.on_shutdown cannot be combined'):

            @cc.crm(namespace='test.pubsub.bad', version='0.1.0')
            class Bad:
                @cc.on_shutdown
                @cc.topic
                def state(self) -> dict: ...

    def test_topic_requires_return_annotation(self):
        with pytest.raises(TypeError, match='requires a return annotation'):

            @cc.crm(namespace='test.pubsub.bad', version='0.1.0')
            class Bad:
                @cc.topic
                def state(self): ...

    def test_topic_rejects_transfer_input(self):
        with pytest.raises(TypeError, match='takes no input'):

            @cc.crm(namespace='test.pubsub.bad', version='0.1.0')
            class Bad:
                @cc.transfer(input=FASTDB_SPEC)
                @cc.topic
                def state(self) -> dict: ...

    def test_topics_are_excluded_from_rpc_methods(self):
        assert rpc_method_names(Telemetry) == ['device']
        assert topic_names(Telemetry) == ['heartbeat', 'state']
        assert topic_names(TopicsOnly) == ['field']
        assert rpc_method_names(TopicsOnly) == []

    def test_topic_only_contract_registers_and_publishes(self, _fresh_runtime):
        runtime = _fresh_runtime
        registration = runtime.register(TopicsOnly, TopicsOnlyImpl(), name='fields')
        publisher = registration.publisher('field')
        assert publisher.publish([1, 2, 3]) == 1
        subscription = _client(runtime, TopicsOnly, 'fields').subscribe('field')
        delivery = next(subscription)
        assert delivery.value == [1, 2, 3]
        assert (delivery.seq, delivery.dropped) == (1, 0)
        subscription.close(timeout=5.0)


# ------------------------------------------------------------------
# Descriptor and identity projections
# ------------------------------------------------------------------


class TestTopicDescriptors:

    def test_no_topic_contract_descriptor_and_hashes_stay_byte_identical(self):
        from c_two.crm.contract import crm_contract

        @cc.crm(namespace='pubsub.golden', version='1.2.3')
        class Golden:
            @cc.read
            def alpha(self, n: int) -> str: ...

            def beta(self, items: list[int]) -> dict[str, int]: ...

        descriptor = build_contract_descriptor(Golden)
        assert 'topics' not in descriptor
        contract = crm_contract(Golden)
        # Values pinned from the pre-topic code path (verified byte-identical
        # against the commit before "topics"): the root key is purely additive.
        assert contract.abi_hash == (
            'f521cf3bd0bcd9f535a74c5525cf81a57ee624612f6a3fc10ec1cab2d4c06b49'
        )
        assert contract.signature_hash == (
            '559c8b0630e361918e9e047492995bb0a8be6875da0061af4afc0d1d49bed3be'
        )

    def test_portable_descriptor_omits_topics_when_absent(self):
        @cc.crm(namespace='test.pubsub.portable_plain', version='0.1.0')
        class PlainPortable:
            @cc.read
            def ping(self) -> None: ...

        portable = build_portable_contract_descriptor(PlainPortable)
        assert 'topics' not in portable

    def test_runtime_descriptor_lists_topics_in_ascending_name_order(self):
        descriptor = build_contract_descriptor(Telemetry)
        assert [topic['name'] for topic in descriptor['topics']] == ['heartbeat', 'state']
        for topic in descriptor['topics']:
            assert set(topic) == {'name', 'transfer'}
        assert descriptor['topics'][0]['transfer']['output']['family'] == 'python-pickle-default'

    def test_declaring_topics_changes_the_route_identity(self):
        from c_two.crm.contract import crm_contract

        def make(with_topic: bool):
            if with_topic:

                @cc.crm(namespace='test.pubsub.identity', version='0.1.0')
                class Sim:
                    @cc.read
                    def ping(self) -> int: ...

                    @cc.topic
                    def field(self) -> dict: ...
            else:

                @cc.crm(namespace='test.pubsub.identity', version='0.1.0')
                class Sim:  # noqa: F811
                    @cc.read
                    def ping(self) -> int: ...

            return Sim

        base = crm_contract(make(with_topic=False))
        extended = crm_contract(make(with_topic=True))
        assert base.abi_hash != extended.abi_hash
        assert base.signature_hash != extended.signature_hash

    def test_pickle_topic_is_flagged_and_rejects_portable_export(self):
        flagged = diagnostics(Telemetry)
        topic_flags = [item for item in flagged if item.get('position') == 'topic']
        assert {item['topic'] for item in topic_flags} == {'heartbeat', 'state'}
        with pytest.raises(ValueError, match='python-pickle-default'):
            cc.contract.export_descriptor(Telemetry)

    def test_fastdb_topic_projects_into_the_portable_descriptor(self):
        @cc.crm(namespace='test.pubsub.portable', version='0.1.0')
        class Portable:
            @cc.read
            def ping(self) -> None: ...

            @cc.transfer(output=FASTDB_SPEC)
            @cc.topic
            def field(self) -> Payload: ...

        assert diagnostics(Portable) == []
        portable = build_portable_contract_descriptor(Portable)
        (topic,) = portable['topics']
        assert topic['name'] == 'field'
        assert topic['output'] == {'kind': 'payload'}
        assert topic['binding']['kind'] == 'fastdb'
        assert topic['binding']['spec']['entries'][0]['id'] == 'value'


# ------------------------------------------------------------------
# Registration and publisher semantics
# ------------------------------------------------------------------


class TestRegistration:

    def test_register_returns_registration_with_name_and_publisher(self, _fresh_runtime):
        runtime = _fresh_runtime
        registration = runtime.register(Telemetry, TelemetryImpl(), name='sim')
        assert isinstance(registration, cc.Registration)
        assert registration.name == 'sim'
        assert registration.is_closed is False
        publisher = registration.publisher('state')
        assert isinstance(publisher, cc.Publisher)
        assert publisher.topic_name == 'state'
        assert publisher.head_seq == 0

    def test_module_level_register_returns_registration(self, monkeypatch):
        from c_two.transport.registry import _get_default_runtime, _reset_default_runtime

        monkeypatch.setenv('C2_ENV_FILE', '')
        monkeypatch.setenv('C2_RELAY_ANCHOR_ADDRESS', '')
        _reset_default_runtime()
        try:
            registration = cc.register(Telemetry, TelemetryImpl(), name='module-sim')
            assert isinstance(registration, cc.Registration)
            assert registration.name == 'module-sim'
            assert _get_default_runtime().names == ['module-sim']
        finally:
            _reset_default_runtime()

    def test_dropping_registration_never_unregisters(self, _fresh_runtime):
        runtime = _fresh_runtime
        registration = runtime.register(Telemetry, TelemetryImpl(), name='sim')
        publisher = registration.publisher('state')
        del registration
        gc.collect()
        assert runtime.names == ['sim']
        assert publisher.publish({'alive': True}) == 1
        assert _client(runtime, Telemetry, 'sim').call_direct('device', ()) == 'sensor-1'

    def test_publisher_unknown_topic_is_a_python_contract_error(self, _fresh_runtime):
        runtime = _fresh_runtime
        registration = runtime.register(Telemetry, TelemetryImpl(), name='sim')
        with pytest.raises(ProtocolViolation):
            registration.publisher('nope')

    def test_publisher_refuses_after_unregister_with_707(self, _fresh_runtime):
        runtime = _fresh_runtime
        registration = runtime.register(Telemetry, TelemetryImpl(), name='sim')
        publisher = registration.publisher('state')
        assert publisher.is_closed is False
        runtime.unregister('sim')
        assert registration.is_closed is True
        assert publisher.is_closed is True
        with pytest.raises(ResourceClosed) as closed:
            publisher.publish({'late': True})
        assert closed.value.code == 707
        with pytest.raises(ResourceClosed):
            registration.publisher('state')

    def test_old_publisher_never_writes_a_later_same_name_registration(self, _fresh_runtime):
        runtime = _fresh_runtime
        first = runtime.register(
            Telemetry, TelemetryImpl(), name='sim',
            initial_topics={'state': {'generation': 1}},
        )
        old_publisher = first.publisher('state')
        runtime.unregister('sim')
        second = runtime.register(
            Telemetry, TelemetryImpl(), name='sim',
            initial_topics={'state': {'generation': 2}},
        )
        new_publisher = second.publisher('state')
        # The old handle is sealed: it must not write into the new registration.
        with pytest.raises(ResourceClosed):
            old_publisher.publish({'poison': True})
        assert old_publisher.head_seq == 1
        assert new_publisher.head_seq == 1

        subscription = _client(runtime, Telemetry, 'sim').subscribe('state', start='next')
        assert new_publisher.publish({'generation': 2, 'seq': 2}) == 2
        delivery = next(subscription)
        assert delivery.value == {'generation': 2, 'seq': 2}
        subscription.close(timeout=5.0)


# ------------------------------------------------------------------
# Initial state
# ------------------------------------------------------------------


class TestInitialTopics:

    def test_initial_state_is_readable_at_route_open(self, _fresh_runtime):
        runtime = _fresh_runtime
        registration = runtime.register(
            Telemetry, TelemetryImpl(), name='sim',
            initial_topics={'state': {'v': 0}, 'heartbeat': 9},
        )
        assert registration.publisher('state').head_seq == 1
        assert registration.publisher('heartbeat').head_seq == 1

        latest = _client(runtime, Telemetry, 'sim').subscribe('state')
        following = _client(runtime, Telemetry, 'sim').subscribe('state', start='next')
        assert latest.start_seq == 1
        assert following.start_seq == 2
        delivery = next(latest)
        assert delivery.value == {'v': 0}
        assert delivery.seq == 1
        latest.close(timeout=5.0)
        following.close(timeout=5.0)

    def test_initial_topics_rejects_unknown_topic_names(self, _fresh_runtime):
        runtime = _fresh_runtime
        with pytest.raises(ValueError, match='not declared'):
            runtime.register(
                Telemetry, TelemetryImpl(), name='sim',
                initial_topics={'nope': 1},
            )
        with pytest.raises(ValueError, match='not declared'):
            runtime.register(
                Telemetry, TelemetryImpl(), name='sim',
                topic_capacity={'nope': 4},
            )
        assert runtime.names == []

    def test_initial_state_rolls_back_when_budget_refuses_the_frame(self, _fresh_runtime):
        from c_two.error import CallCapacityExceeded

        runtime = _fresh_runtime
        runtime.set_server(memory_budget_bytes=128)
        with pytest.raises(CallCapacityExceeded) as failure:
            runtime.register(
                Telemetry, TelemetryImpl(), name='sim',
                initial_topics={'state': {'blob': bytes(300)}},
            )
        assert failure.value.code == 717
        # The route was never opened and the staged frame left no charge.
        assert runtime.names == []
        assert runtime.memory_stats()['server']['memory']['used_bytes'] == 0

        # A later fitting registration on the same host succeeds cleanly.
        registration = runtime.register(
            Telemetry, TelemetryImpl(), name='sim',
            initial_topics={'state': {'v': 1}},
        )
        assert registration.publisher('state').head_seq == 1

    def test_unserializable_initial_value_never_touches_the_server(self, _fresh_runtime):
        runtime = _fresh_runtime

        class Unserializable:
            def __reduce__(self):
                raise RuntimeError('no pickle for you')

        with pytest.raises(SerializationError, match='no pickle for you'):
            runtime.register(
                Telemetry, TelemetryImpl(), name='sim',
                initial_topics={'state': Unserializable()},
            )
        assert runtime.names == []
        assert runtime.server_address() is None


# ------------------------------------------------------------------
# Same-process delivery semantics
# ------------------------------------------------------------------


class TestLocalDelivery:

    def test_publish_takes_an_immutable_snapshot_of_mutable_values(self, _fresh_runtime):
        runtime = _fresh_runtime
        registration = runtime.register(Telemetry, TelemetryImpl(), name='sim')
        publisher = registration.publisher('state')

        first = _client(runtime, Telemetry, 'sim').subscribe('state')
        second = _client(runtime, Telemetry, 'sim').subscribe('state')

        payload = {'items': [1, 2, 3]}
        assert publisher.publish(payload) == 1
        # Publisher mutates in place after publish: deliveries must not see it.
        payload['items'].append(999)
        payload['extra'] = 'late'

        d1 = next(first)
        d2 = next(second)
        assert d1.value == {'items': [1, 2, 3]}
        assert d2.value == {'items': [1, 2, 3]}
        # Each delivery decoded its own immutable copy.
        assert d1.value is not d2.value
        d1.value['items'].append(-1)
        assert d2.value['items'] == [1, 2, 3]
        first.close(timeout=5.0)
        second.close(timeout=5.0)

    def test_start_latest_and_next_semantics(self, _fresh_runtime):
        runtime = _fresh_runtime
        registration = runtime.register(Telemetry, TelemetryImpl(), name='sim')
        publisher = registration.publisher('heartbeat')
        publisher.publish(1)
        publisher.publish(2)

        proxy = _client(runtime, Telemetry, 'sim')
        latest = proxy.subscribe('heartbeat')
        following = proxy.subscribe('heartbeat', start='next')
        assert latest.start_seq == 2
        assert following.start_seq == 3

        assert next(latest).value == 2
        assert publisher.publish(3) == 3
        assert next(following).value == 3
        latest.close(timeout=5.0)
        following.close(timeout=5.0)

    def test_on_lag_skip_reports_dropped_count(self, _fresh_runtime):
        runtime = _fresh_runtime
        registration = runtime.register(
            Telemetry, TelemetryImpl(), name='sim',
            topic_capacity={'heartbeat': 2},
        )
        publisher = registration.publisher('heartbeat')
        proxy = _client(runtime, Telemetry, 'sim')

        skipper = proxy.subscribe('heartbeat', on_lag='skip')
        assert publisher.publish(1) == 1
        assert next(skipper).value == 1
        # Overflow the capacity-2 ring while the local cursor still expects
        # seq 2: the next select must jump to the newest retained frame.
        for beat in (2, 3, 4, 5, 6):
            publisher.publish(beat)

        delivery = next(skipper)
        assert delivery.value == 6
        assert delivery.seq == 6
        assert delivery.dropped == 6 - 2
        skipper.close(timeout=5.0)

    def test_on_lag_error_terminates_with_720(self, _fresh_runtime):
        runtime = _fresh_runtime
        registration = runtime.register(
            Telemetry, TelemetryImpl(), name='sim',
            topic_capacity={'heartbeat': 2},
        )
        publisher = registration.publisher('heartbeat')
        proxy = _client(runtime, Telemetry, 'sim')

        strict = proxy.subscribe('heartbeat', on_lag='error')
        assert publisher.publish(1) == 1
        assert next(strict).value == 1
        for beat in (2, 3, 4, 5, 6, 7):
            publisher.publish(beat)

        with pytest.raises(SubscriptionLagged) as lagged:
            for delivery in strict:  # noqa: B007
                assert delivery.dropped == 0
        assert lagged.value.code == 720
        assert lagged.value.details['topic'] == 'heartbeat'
        assert 'dropped' in lagged.value.details
        receipt = strict.close(timeout=5.0)
        assert receipt['completed']

    def test_unknown_topic_on_proxy_reports_713_not_701(self, _fresh_runtime):
        runtime = _fresh_runtime
        runtime.register(Telemetry, TelemetryImpl(), name='sim')
        proxy = _client(runtime, Telemetry, 'sim')
        with pytest.raises(ProtocolViolation) as violation:
            proxy.subscribe('nope')
        assert violation.value.code == 713

    def test_unregister_ends_subscription_with_718_cause(self, _fresh_runtime):
        runtime = _fresh_runtime
        registration = runtime.register(Telemetry, TelemetryImpl(), name='sim')
        publisher = registration.publisher('state')
        proxy = _client(runtime, Telemetry, 'sim')
        subscription = proxy.subscribe('state')
        publisher.publish({'n': 1})
        assert next(subscription).value == {'n': 1}

        runtime.unregister('sim')
        with pytest.raises(CallCancelled) as cancelled:
            next(subscription)
        assert cancelled.value.code == 718
        assert cancelled.value.details['cause'] == 'route_unregistered'
        receipt = subscription.close(timeout=5.0)
        assert receipt['completed']

    def test_cancel_ends_iteration_cleanly_and_close_reports_receipt(self, _fresh_runtime):
        runtime = _fresh_runtime
        registration = runtime.register(Telemetry, TelemetryImpl(), name='sim')
        publisher = registration.publisher('state')
        proxy = _client(runtime, Telemetry, 'sim')
        subscription = proxy.subscribe('state')

        subscription.cancel()
        publisher.publish({'n': 1})
        with pytest.raises(StopIteration):
            next(subscription)
        receipt = subscription.close(timeout=5.0)
        assert receipt['completed'] is True
        assert isinstance(receipt['error'], CallCancelled)
        assert receipt['error'].details['cause'] == 'consumer_cancelled'

    def test_subscription_context_manager_and_metadata(self, _fresh_runtime):
        runtime = _fresh_runtime
        registration = runtime.register(Telemetry, TelemetryImpl(), name='sim')
        publisher = registration.publisher('state')
        proxy = _client(runtime, Telemetry, 'sim')

        with proxy.subscribe('state') as subscription:
            assert subscription.topic_name == 'state'
            assert subscription.route_name == 'sim'
            publisher.publish({'ctx': True})
            delivery = next(subscription)
        assert delivery.value == {'ctx': True}
        # The closed subscription released its subscriber slot.
        assert registration.active_subscribers == 0

    def test_hold_delivery_keeps_value_valid_after_close(self, _fresh_runtime):
        runtime = _fresh_runtime
        registration = runtime.register(Telemetry, TelemetryImpl(), name='sim')
        publisher = registration.publisher('state')
        proxy = _client(runtime, Telemetry, 'sim')

        subscription = proxy.subscribe('state', hold=True)
        publisher.publish({'held': 1})
        delivery = next(subscription)
        assert isinstance(delivery.value, cc.HeldResult)
        assert delivery.value.value == {'held': 1}
        subscription.close(timeout=5.0)
        # Closing the subscription does not invalidate delivered values.
        assert delivery.value.value == {'held': 1}
        delivery.value.release()
        with pytest.raises(RuntimeError):
            _ = delivery.value.value

    def test_publish_serialization_failure_is_pre_dispatch(self, _fresh_runtime):
        runtime = _fresh_runtime
        registration = runtime.register(Telemetry, TelemetryImpl(), name='sim')

        class Unserializable:
            def __reduce__(self):
                raise RuntimeError('cannot publish me')

        with pytest.raises(SerializationError) as failure:
            registration.publisher('state').publish(Unserializable())
        assert failure.value.code == 2
        assert failure.value.transport_phase == 'pre_dispatch'
        # A failed publish consumed no seq and left the ring untouched.
        assert registration.publisher('state').head_seq == 0

    def test_thread_proxy_without_registered_route_refuses_subscribe(self):
        from c_two.error import UnsupportedCallMode
        from c_two.transport.client.proxy import CRMProxy

        proxy = CRMProxy.thread_local(TelemetryImpl(), name='orphan')
        with pytest.raises(UnsupportedCallMode):
            proxy.subscribe('state')

    def test_fastdb_topic_publish_and_delivery_same_process(self, _fresh_runtime):
        @cc.crm(namespace='test.pubsub.fastdb', version='0.1.0')
        class FastdbSim:
            @cc.transfer(output=FASTDB_SPEC)
            @cc.topic
            def field(self) -> Payload: ...

        class FastdbImpl:
            pass

        def build(value: int) -> Payload:
            spec = CompiledSpec.compile(
                json.dumps(FASTDB_SPEC, separators=(',', ':')).encode(),
            )
            builder = Builder.create(spec)
            entry = builder.entry_begin(0, 1)
            entry.value_u8(value)
            plan = builder.freeze()
            builder.close()
            try:
                return plan.execute(BuildPolicy.ALLOW_STAGING).payload
            finally:
                plan.close()
                spec.close()

        runtime = _fresh_runtime
        registration = runtime.register(FastdbSim, FastdbImpl(), name='fastdb')
        publisher = registration.publisher('field')

        proxy = _client(runtime, FastdbSim, 'fastdb')
        subscription = proxy.subscribe('field')
        publisher.publish(build(7))
        delivery = next(subscription)
        with delivery.value.entry_view(0) as values:
            with values.at(0) as value:
                assert value.get_u8() == 7
        delivery.value.close()
        subscription.close(timeout=5.0)


# ------------------------------------------------------------------
# Natural topic members (telemetry.state()) on the same-process path
# ------------------------------------------------------------------


class TestNaturalTopicMembers:

    def test_topic_member_attaches_and_holds(self, _fresh_runtime):
        runtime = _fresh_runtime
        registration = runtime.register(
            Telemetry, TelemetryImpl(), name='sim',
            initial_topics={'state': {'boot': 0}},
        )
        publisher = registration.publisher('state')

        telemetry = runtime.connect(Telemetry, name='sim')
        subscription = telemetry.state()
        assert subscription.topic_name == 'state'
        assert subscription.route_name == 'sim'
        assert subscription.start_seq == 1
        assert next(subscription).value == {'boot': 0}
        assert publisher.publish({'n': 1}) == 2
        assert next(subscription).value == {'n': 1}
        subscription.close(timeout=5.0)

        # cc.hold reuses the shared unary/stream hold wrapper.
        held_stream = cc.hold(telemetry.state)(start='next')
        assert publisher.publish({'n': 2}) == 3
        retained = next(held_stream)
        assert isinstance(retained.value, cc.HeldResult)
        assert retained.value.value == {'n': 2}
        held_stream.close(timeout=5.0)
        assert retained.value.value == {'n': 2}
        retained.value.release()
        with pytest.raises(RuntimeError):
            _ = retained.value.value

    def test_topic_member_rejects_positional_arguments(self, _fresh_runtime):
        runtime = _fresh_runtime
        runtime.register(Telemetry, TelemetryImpl(), name='sim')
        telemetry = runtime.connect(Telemetry, name='sim')
        with pytest.raises(TypeError, match='no positional arguments'):
            telemetry.state('latest')

    def test_with_call_options_views_validate_the_thread_policy(self, _fresh_runtime):
        from c_two.error import UnsupportedCallMode

        runtime = _fresh_runtime
        runtime.register(Telemetry, TelemetryImpl(), name='sim')
        telemetry = runtime.connect(Telemetry, name='sim')
        with pytest.raises(UnsupportedCallMode):
            cc.with_call_options(telemetry, timeout=1.0).state()
        with pytest.raises(UnsupportedCallMode):
            telemetry.client.subscribe('state', timeout=1.0)
        # Unlimited and inherited views subscribe on the same-process path.
        subscription = cc.with_call_options(telemetry, timeout=None).state()
        subscription.close(timeout=5.0)

    def test_server_side_topic_member_has_no_call_surface(self):
        from c_two.transport.server.native import NativeServerBridge

        bridge = NativeServerBridge(_DummySession())
        instance = bridge._create_crm_instance(Telemetry, TelemetryImpl())
        with pytest.raises(TypeError, match='no resource-side call surface'):
            instance.state()

    def test_wrapped_members_keep_discovery_behaviour(self):
        # The wrapper keeps the declaration markers, so topic discovery, RPC
        # method discovery and conformance behave exactly like the plain stub.
        assert topic_names(Telemetry) == ['heartbeat', 'state']
        assert rpc_method_names(Telemetry) == ['device']
        from c_two.crm.conformance import validate_resource_conformance

        validate_resource_conformance(Telemetry, TelemetryImpl())
        from c_two.crm.descriptor import build_contract_descriptor

        descriptor = build_contract_descriptor(Telemetry)
        assert [topic['name'] for topic in descriptor['topics']] == ['heartbeat', 'state']

    def test_topic_only_contract_reregistration_follows_on_thread_proxy(self, _fresh_runtime):
        runtime = _fresh_runtime
        runtime.register(
            TopicsOnly, TopicsOnlyImpl(), name='fields',
            initial_topics={'field': [1]},
        )
        crm = runtime.connect(TopicsOnly, name='fields')
        subscription = crm.field()
        assert subscription.start_seq == 1
        assert next(subscription).value == [1]

        runtime.unregister('fields')
        with pytest.raises(CallCancelled):
            next(subscription)
        subscription.close(timeout=5.0)

        second = runtime.register(
            TopicsOnly, TopicsOnlyImpl(), name='fields',
            initial_topics={'field': [2]},
        )
        # The same proxy's next member call follows the new registration:
        # fresh sequence space seeded by the new initial frame.
        following = crm.field()
        assert following.start_seq == 1
        assert next(following).value == [2]
        assert second.publisher('field').publish([3]) == 2
        assert next(following).value == [3]
        following.close(timeout=5.0)


class _DummySession:
    """Minimal RuntimeSession stand-in for bridge instance creation."""

    def __getattr__(self, name):
        raise AssertionError(f'unexpected RuntimeSession use: {name}')
