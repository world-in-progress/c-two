import inspect
from collections.abc import Iterator

import pytest

import c_two as cc
from c_two.transport.client.proxy import CRMProxy


@cc.crm(namespace='test.outcome-modifiers', version='1.0.0')
class Contract:
    def step(self, value: int) -> int: ...

    @cc.stream
    def frames(self) -> Iterator[int]: ...

    @cc.topic
    def latest(self) -> int: ...


@pytest.mark.parametrize('call_id', ['', 'a' * 129, 'a b', 'a\n', 'a/b', 'é', 1, None])
def test_invalid_call_id_is_rejected_before_invocation(call_id):
    for wrap in (cc.with_call_id, cc.recover_call):
        with pytest.raises(ValueError, match='call_id'):
            wrap(Contract().step, call_id)
    with pytest.raises(ValueError, match='call_id'):
        cc.query_call(Contract(), call_id)


@pytest.mark.parametrize('name', ['frames', 'latest'])
@pytest.mark.parametrize('wrap', [cc.with_call_id, cc.recover_call])
def test_stream_and_topic_reject_keys_even_through_hold(name, wrap):
    method = getattr(Contract(), name)
    for bound in (method, cc.hold(method)):
        with pytest.raises(TypeError, match='unary CRM'):
            wrap(bound, 'valid_1-2')
    with pytest.raises(TypeError, match='unary CRM'):
        cc.hold(wrap(method, 'valid_1-2'))


@pytest.mark.parametrize('wrap', [cc.with_call_id, cc.recover_call])
def test_keyed_modifier_requires_a_bound_crm_method(wrap):
    class Other:
        def step(self): ...

    for method in (Contract.step, Other().step, object()):
        with pytest.raises(TypeError, match='bound'):
            wrap(method, 'valid')


def test_submit_and_recover_are_mutually_exclusive():
    method = Contract().step
    with pytest.raises(TypeError, match='cannot be combined'):
        cc.with_call_id(cc.recover_call(method, 'a'), 'a')
    with pytest.raises(TypeError, match='cannot be combined'):
        cc.recover_call(cc.hold(cc.with_call_id(method, 'a')), 'a')
    recovery = cc.hold(cc.recover_call(method, 'a' * 128))
    assert len(inspect.signature(recovery).parameters) == 0
    with pytest.raises(TypeError):
        recovery(1)
    with pytest.raises(TypeError):
        recovery(value=1)


@pytest.mark.parametrize('name', [None, 'manual'])
def test_manual_thread_local_outcome_operations_refuse_before_dispatch(name):
    class Resource:
        def step(self, value):
            pytest.fail('the manual route must not execute')

    proxy = Contract()
    proxy.client = CRMProxy.thread_local(Resource(), name=name)
    for invoke in (
        lambda: cc.with_call_id(proxy.step, 'manual')(1),
        cc.recover_call(proxy.step, 'manual'),
        lambda: cc.query_call(proxy, 'manual'),
    ):
        with pytest.raises(cc.error.UnsupportedCallMode, match='registered Runtime route') as exc:
            invoke()
        assert exc.value.transport_phase == 'pre_dispatch'
        assert exc.value.call_id is None
