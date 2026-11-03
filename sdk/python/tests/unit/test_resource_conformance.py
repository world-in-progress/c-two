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


@pytest.mark.parametrize(
    'method, message',
    [
        (lambda self, payload: payload, 'parameter name mismatch'),
        (lambda self: '', 'parameter count mismatch'),
        (lambda self, name, *, required: name, 'keyword-only parameters'),
    ],
)
def test_validate_resource_conformance_rejects_parameter_shape_mismatch(method, message):
    class BadShape(GreetingResource):
        greet = method

    with pytest.raises(TypeError, match=message):
        validate_resource_conformance(Greeting, BadShape())
