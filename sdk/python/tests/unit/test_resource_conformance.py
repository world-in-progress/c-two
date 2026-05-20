import pytest

import c_two as cc
from c_two.crm.conformance import validate_resource_conformance


@cc.crm(namespace='test.resource-conformance', version='0.1.0')
class Greeting:
    def greet(self, name: str) -> str:
        ...

    def add(self, left: int, right: int) -> int:
        ...


class GreetingResource:
    def greet(self, name: str) -> str:
        return f'Hello, {name}!'

    def add(self, left: int, right: int) -> int:
        return left + right


def test_validate_resource_conformance_accepts_matching_resource():
    validate_resource_conformance(Greeting, GreetingResource())


def test_validate_resource_conformance_allows_missing_resource_annotations():
    class LooseResource:
        def greet(self, name):
            return f'Hello, {name}!'

        def add(self, left, right):
            return left + right

    validate_resource_conformance(Greeting, LooseResource())


def test_validate_resource_conformance_rejects_missing_method():
    class MissingMethod:
        def greet(self, name: str) -> str:
            return name

    with pytest.raises(TypeError, match='missing method.*add'):
        validate_resource_conformance(Greeting, MissingMethod())


def test_validate_resource_conformance_rejects_required_parameter_mismatch():
    class BadParams:
        def greet(self, name: str, punctuation: str) -> str:
            return name + punctuation

        def add(self, left: int, right: int) -> int:
            return left + right

    with pytest.raises(TypeError, match='greet.*parameter count'):
        validate_resource_conformance(Greeting, BadParams())


def test_validate_resource_conformance_rejects_parameter_annotation_mismatch():
    class BadAnnotation:
        def greet(self, name: bytes) -> str:
            return name.decode()

        def add(self, left: int, right: int) -> int:
            return left + right

    with pytest.raises(TypeError, match='greet.name.*str.*bytes'):
        validate_resource_conformance(Greeting, BadAnnotation())


def test_validate_resource_conformance_rejects_return_annotation_mismatch():
    class BadReturn:
        def greet(self, name: str) -> bytes:
            return name.encode()

        def add(self, left: int, right: int) -> int:
            return left + right

    with pytest.raises(TypeError, match='greet.*return.*str.*bytes'):
        validate_resource_conformance(Greeting, BadReturn())


def test_validate_resource_conformance_allows_extra_optional_resource_params():
    class ExtraOptional:
        def greet(self, name: str, punctuation: str = '!') -> str:
            return f'Hello, {name}{punctuation}'

        def add(self, left: int, right: int, base: int = 0) -> int:
            return base + left + right

    validate_resource_conformance(Greeting, ExtraOptional())


def test_validate_resource_conformance_accepts_mismatched_method_with_bridge():
    class BytesGreeting:
        def greet(self, payload: bytes) -> bytes:
            return payload

        def add(self, left: int, right: int) -> int:
            return left + right

    validate_resource_conformance(
        Greeting,
        BytesGreeting(),
        bridge={
            'greet': cc.bridge(
                input=lambda name: (name.encode(),),
                output=lambda payload: payload.decode(),
            ),
        },
    )


def test_validate_resource_conformance_rejects_unknown_bridge_method():
    with pytest.raises(TypeError, match='unknown bridge method.*missing'):
        validate_resource_conformance(
            Greeting,
            GreetingResource(),
            bridge={'missing': cc.bridge()},
        )


def test_empty_bridge_does_not_bypass_resource_annotation_validation():
    class BadAnnotation:
        def greet(self, name: bytes) -> str:
            return name.decode()

        def add(self, left: int, right: int) -> int:
            return left + right

    with pytest.raises(TypeError, match='greet.name.*str.*bytes'):
        validate_resource_conformance(
            Greeting,
            BadAnnotation(),
            bridge={'greet': cc.bridge()},
        )


def test_output_only_bridge_does_not_bypass_resource_parameter_validation():
    class BadParameter:
        def greet(self, name: bytes) -> bytes:
            return name

        def add(self, left: int, right: int) -> int:
            return left + right

    with pytest.raises(TypeError, match='greet.name.*str.*bytes'):
        validate_resource_conformance(
            Greeting,
            BadParameter(),
            bridge={'greet': cc.bridge(output=lambda payload: payload.decode())},
        )


def test_resource_bridge_rejects_non_callable_hooks():
    with pytest.raises(TypeError, match='bridge input must be callable'):
        cc.ResourceBridge(input='not-callable')

    with pytest.raises(TypeError, match='bridge output must be callable'):
        cc.ResourceBridge(output='not-callable')
