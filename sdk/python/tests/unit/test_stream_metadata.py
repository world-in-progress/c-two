from collections.abc import Iterator

import pytest

import c_two as cc
from c_two.crm.descriptor import build_contract_descriptor, build_contract_fingerprints


def test_stream_mode_is_orthogonal_to_method_access_and_enters_fingerprints():
    @cc.crm(namespace='test.stream.metadata', version='0.1.0')
    class Values:
        @cc.read
        @cc.stream
        def values(self) -> Iterator[int]: ...

    descriptor = build_contract_descriptor(Values)
    method, = descriptor['methods']
    assert method['mode'] == 'server_stream'
    assert method['access'] == 'read'
    assert method['return'] == {'kind': 'primitive', 'name': 'int'}
    before = build_contract_fingerprints(Values)

    class UnaryValues:
        @cc.read
        def values(self) -> int: ...

    UnaryValues.__name__ = 'Values'
    UnaryValues = cc.crm(namespace='test.stream.metadata', version='0.1.0')(UnaryValues)
    unary_method, = build_contract_descriptor(UnaryValues)['methods']
    assert {key: value for key, value in method.items() if key != 'mode'} == unary_method
    unary = build_contract_fingerprints(UnaryValues)
    assert before[0] != unary[0]
    assert before[1] != unary[1]
    assert build_contract_fingerprints(Values) == before


def test_stream_requires_an_item_annotation():
    with pytest.raises(TypeError, match='Iterator'):
        @cc.crm(namespace='test.stream.bad_annotation', version='0.1.0')
        class Invalid:
            @cc.stream
            def values(self) -> int: ...


def test_unary_descriptor_does_not_acquire_a_mode_key():
    @cc.crm(namespace='test.stream.unary', version='0.1.0')
    class Unary:
        def value(self) -> int: ...

    assert 'mode' not in build_contract_descriptor(Unary)['methods'][0]
