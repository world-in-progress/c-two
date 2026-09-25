from __future__ import annotations

import inspect
import json
import math
import types
from typing import Any, ForwardRef, Union, get_args, get_origin, get_type_hints

from .contract import crm_contract_identity
from .meta import MethodAccess, get_method_access
from .methods import rpc_method_names
from .payload_plan import DEFAULT_PICKLE_PROTOCOL, PayloadBinding, PayloadPlanKind

_DESCRIPTOR_SCHEMA = "c-two.python.crm.descriptor.v2"
_PORTABLE_CONTRACT_SCHEMA = "c-two.contract.v2"
_ABI_SCHEMA = "c-two.python.crm.abi.v2"
_SIGNATURE_SCHEMA = "c-two.python.crm.signature.v2"
_PICKLE_DEFAULT_REF = {
    "family": "python-pickle-default",
    "kind": "builtin",
    "python_min": "3.10",
    "portable": False,
    "version": f"pickle-protocol-{DEFAULT_PICKLE_PROTOCOL}",
}
_PRIMITIVES = {
    bool: "bool",
    int: "int",
    float: "float",
    str: "str",
    bytes: "bytes",
    memoryview: "memoryview",
    bytearray: "bytearray",
}
_BARE_CONTAINERS = {list, dict, tuple, set, frozenset}


def build_contract_descriptor(
    crm_class: type,
    methods: list[str] | None = None,
    *,
    portable: bool = False,
) -> dict[str, Any]:
    if portable:
        return build_portable_contract_descriptor(crm_class, methods)

    crm_ns, crm_name, crm_ver = crm_contract_identity(crm_class)
    method_names = rpc_method_names(crm_class) if methods is None else list(methods)
    return {
        "crm": {
            "name": crm_name,
            "namespace": crm_ns,
            "version": crm_ver,
        },
        "methods": [
            _runtime_method_descriptor(crm_class, method_name)
            for method_name in method_names
        ],
        "schema": _DESCRIPTOR_SCHEMA,
    }


def build_contract_fingerprints(
    crm_class: type,
    methods: list[str] | None = None,
) -> tuple[str, str]:
    method_names = rpc_method_names(crm_class) if methods is None else list(methods)
    if _is_portable_method_set(crm_class, method_names):
        descriptor = build_portable_contract_descriptor(crm_class, method_names)
        fingerprints = descriptor["fingerprints"]
        return fingerprints["abi_hash"], fingerprints["signature_hash"]

    return _fingerprints_from_descriptor(
        build_contract_descriptor(crm_class, method_names),
    )


def _fingerprints_from_descriptor(
    descriptor: dict[str, Any],
) -> tuple[str, str]:
    abi_descriptor = {
        "crm": descriptor["crm"],
        "methods": [
            {
                "buffer": method["buffer"],
                "input": method["wire"]["input"],
                "name": method["name"],
                "output": method["wire"]["output"],
            }
            for method in descriptor["methods"]
        ],
        "schema": _ABI_SCHEMA,
    }
    signature_descriptor = {
        "crm": descriptor["crm"],
        "methods": [
            {
                "access": method["access"],
                "buffer": method["buffer"],
                "name": method["name"],
                "parameters": method["parameters"],
                "return": method["return"],
                "transfer": method["transfer"],
            }
            for method in descriptor["methods"]
        ],
        "schema": _SIGNATURE_SCHEMA,
    }
    return _hash_descriptor(abi_descriptor), _hash_descriptor(signature_descriptor)


def build_portable_contract_descriptor(
    crm_class: type,
    methods: list[str] | None = None,
) -> dict[str, Any]:
    from c_two._native import (
        canonicalize_portable_contract_descriptor,
        derive_portable_contract_fingerprints,
    )

    crm_ns, crm_name, crm_ver = crm_contract_identity(crm_class)
    method_names = rpc_method_names(crm_class) if methods is None else list(methods)
    candidate = {
        "schema": _PORTABLE_CONTRACT_SCHEMA,
        "crm": {
            "namespace": crm_ns,
            "name": crm_name,
            "version": crm_ver,
        },
        "fingerprints": {
            "abi_hash": "0" * 64,
            "signature_hash": "0" * 64,
        },
        "methods": [
            _portable_method_descriptor(crm_class, method_name)
            for method_name in method_names
        ],
    }
    candidate_bytes = _canonical_json(candidate).encode()
    abi_hash, signature_hash = derive_portable_contract_fingerprints(
        candidate_bytes,
    )
    candidate["fingerprints"] = {
        "abi_hash": abi_hash,
        "signature_hash": signature_hash,
    }
    canonical = canonicalize_portable_contract_descriptor(
        _canonical_json(candidate).encode(),
    )
    return json.loads(canonical)


def contract_descriptor_diagnostics(
    crm_class: type,
    methods: list[str] | None = None,
) -> list[dict[str, Any]]:
    method_names = rpc_method_names(crm_class) if methods is None else list(methods)
    diagnostics: list[dict[str, Any]] = []
    for method_name in method_names:
        method = getattr(crm_class, method_name)
        for position, binding in (
            ("input", getattr(method, "_input_payload_binding", None)),
            ("output", getattr(method, "_output_payload_binding", None)),
        ):
            if (
                isinstance(binding, PayloadBinding)
                and binding.kind is PayloadPlanKind.PYTHON_PICKLE
            ):
                diagnostics.append(
                    {
                        "code": "python_only_pickle",
                        "message": (
                            f"{method_name}.{position} uses "
                            "python-pickle-default; it can run only between "
                            "Python peers and cannot be exported as a "
                            "portable c-two.contract.v2 method."
                        ),
                        "method": method_name,
                        "position": position,
                        "severity": "warning",
                    },
                )
    return diagnostics


def export_contract_descriptor(
    crm_class: type,
    methods: list[str] | None = None,
    *,
    pretty: bool = False,
) -> str:
    descriptor = build_portable_contract_descriptor(crm_class, methods)
    compact = _canonical_json(descriptor)
    return _pretty_json(compact) if pretty else compact


def export_contract_release_ref(
    crm_class: type,
    methods: list[str] | None = None,
    *,
    pretty: bool = False,
) -> str:
    from c_two._native import contract_release_ref_json

    descriptor = export_contract_descriptor(crm_class, methods)
    compact = contract_release_ref_json(descriptor.encode())
    return _pretty_json(compact) if pretty else compact


def _runtime_method_descriptor(
    crm_class: type,
    method_name: str,
) -> dict[str, Any]:
    method = getattr(crm_class, method_name)
    target = inspect.unwrap(method)
    signature = inspect.signature(target)
    type_hints = _resolved_type_hints(target, method_name)
    input_binding = _binding(method, "_input_payload_binding")
    output_binding = _binding(method, "_output_payload_binding")
    parameters = _signature_parameters(signature)

    parameter_descriptors: list[dict[str, Any]] = []
    for parameter in parameters:
        _validate_parameter(parameter, type_hints, method_name)
        parameter_descriptors.append(
            {
                "annotation": _annotation_descriptor(
                    type_hints[parameter.name],
                    method_name,
                ),
                "default": _default_descriptor(
                    parameter.default,
                    method_name,
                    parameter.name,
                ),
                "kind": parameter.kind.name,
                "name": parameter.name,
            },
        )

    if "return" not in type_hints:
        raise TypeError(f"{method_name} is missing a return annotation.")
    return_annotation = type_hints["return"]
    access = get_method_access(method)
    return {
        "access": "read" if access is MethodAccess.READ else "write",
        "buffer": getattr(method, "_input_buffer_mode", "view"),
        "name": method_name,
        "parameters": parameter_descriptors,
        "return": _annotation_descriptor(return_annotation, method_name),
        "transfer": {
            "input": _runtime_binding_descriptor(input_binding),
            "output": _runtime_binding_descriptor(output_binding),
        },
        "wire": {
            "input": _runtime_wire_ref(input_binding, bool(parameters)),
            "output": _runtime_wire_ref(
                output_binding,
                return_annotation not in (None, type(None)),
            ),
        },
    }


def _portable_method_descriptor(
    crm_class: type,
    method_name: str,
) -> dict[str, Any]:
    method = getattr(crm_class, method_name)
    target = inspect.unwrap(method)
    signature = inspect.signature(target)
    type_hints = _resolved_type_hints(target, method_name)
    parameters = _signature_parameters(signature)
    input_binding = _binding(method, "_input_payload_binding")
    output_binding = _binding(method, "_output_payload_binding")
    payload_type = _payload_type()

    if input_binding.kind is PayloadPlanKind.PYTHON_PICKLE:
        raise ValueError(
            f"{method_name} cannot be exported as a portable contract: "
            "input uses python-pickle-default.",
        )
    if output_binding.kind is PayloadPlanKind.PYTHON_PICKLE:
        raise ValueError(
            f"{method_name} cannot be exported as a portable contract: "
            "output uses python-pickle-default.",
        )

    if input_binding.kind is PayloadPlanKind.FASTDB:
        if len(parameters) != 1:
            raise TypeError(
                f"{method_name} portable input requires exactly one Payload "
                "parameter.",
            )
        parameter = parameters[0]
        _validate_parameter(parameter, type_hints, method_name)
        if type_hints[parameter.name] is not payload_type:
            raise TypeError(
                f"{method_name}.{parameter.name} must be annotated as "
                "fastdb4py.payload.Payload.",
            )
        if parameter.default is not inspect.Parameter.empty:
            raise TypeError(
                f"{method_name}.{parameter.name} cannot define a default.",
            )
        parameter_descriptors = [
            {
                "name": parameter.name,
                "kind": parameter.kind.name,
                "default": {"kind": "missing"},
                "type": {"kind": "payload"},
            },
        ]
    else:
        if parameters:
            raise TypeError(
                f"{method_name} has parameters without an explicit portable "
                "input binding.",
            )
        parameter_descriptors = []

    if "return" not in type_hints:
        raise TypeError(f"{method_name} is missing a return annotation.")
    return_annotation = type_hints["return"]
    if output_binding.kind is PayloadPlanKind.FASTDB:
        if return_annotation is not payload_type:
            raise TypeError(
                f"{method_name} must return fastdb4py.payload.Payload.",
            )
        return_descriptor = {"kind": "payload"}
    else:
        if return_annotation not in (None, type(None)):
            raise TypeError(
                f"{method_name} returns a value without an explicit portable "
                "output binding.",
            )
        return_descriptor = {"kind": "none"}

    access = get_method_access(method)
    return {
        "access": "read" if access is MethodAccess.READ else "write",
        "name": method_name,
        "parameters": parameter_descriptors,
        "return": return_descriptor,
        "bindings": {
            "input": _portable_binding_descriptor(input_binding),
            "output": _portable_binding_descriptor(output_binding),
        },
    }


def _portable_binding_descriptor(
    binding: PayloadBinding,
) -> dict[str, Any] | None:
    if binding.kind is PayloadPlanKind.NO_PAYLOAD:
        return None
    if binding.kind is not PayloadPlanKind.FASTDB:
        raise ValueError("Python pickle bindings are not portable.")
    return {
        "kind": "fastdb",
        "spec": json.loads(_canonical_json(binding.spec)),
    }


def _runtime_binding_descriptor(
    binding: PayloadBinding,
) -> dict[str, Any] | None:
    if binding.kind is PayloadPlanKind.NO_PAYLOAD:
        return None
    if binding.kind is PayloadPlanKind.PYTHON_PICKLE:
        return dict(_PICKLE_DEFAULT_REF)
    assert binding.spec_sha256 is not None
    return {
        "kind": "fastdb",
        "spec_sha256": binding.spec_sha256.hex(),
    }


def _runtime_wire_ref(
    binding: PayloadBinding,
    has_value: bool,
) -> dict[str, Any] | None:
    if binding.kind is PayloadPlanKind.NO_PAYLOAD:
        return dict(_PICKLE_DEFAULT_REF) if has_value else None
    return _runtime_binding_descriptor(binding)


def _binding(method: object, attribute: str) -> PayloadBinding:
    binding = getattr(method, attribute, None)
    if not isinstance(binding, PayloadBinding):
        raise TypeError(f"{attribute} is not a C-Two payload binding.")
    return binding


def _is_portable_method_set(
    crm_class: type,
    method_names: list[str],
) -> bool:
    for method_name in method_names:
        method = getattr(crm_class, method_name)
        for attribute in ("_input_payload_binding", "_output_payload_binding"):
            binding = _binding(method, attribute)
            if binding.kind is PayloadPlanKind.PYTHON_PICKLE:
                return False
    return True


def _signature_parameters(
    signature: inspect.Signature,
) -> list[inspect.Parameter]:
    parameters = list(signature.parameters.values())
    if parameters and parameters[0].name in {"self", "cls"}:
        parameters = parameters[1:]
    return parameters


def _validate_parameter(
    parameter: inspect.Parameter,
    type_hints: dict[str, Any],
    method_name: str,
) -> None:
    if parameter.kind in {
        inspect.Parameter.VAR_POSITIONAL,
        inspect.Parameter.VAR_KEYWORD,
    }:
        raise TypeError(
            f"{method_name} uses varargs parameter {parameter.name!r}; "
            "CRM RPC methods must have explicit parameters.",
        )
    if parameter.name not in type_hints:
        raise TypeError(
            f"{method_name}.{parameter.name} is missing a type annotation.",
        )


def _resolved_type_hints(
    func: object,
    method_name: str,
) -> dict[str, Any]:
    try:
        return get_type_hints(func, include_extras=True)
    except NameError as exc:
        raise ValueError(
            f"{method_name} contains an unresolved forward reference: {exc}",
        ) from exc
    except TypeError as exc:
        raise TypeError(
            f"{method_name} contains an unsupported annotation: {exc}",
        ) from exc


def _annotation_descriptor(
    annotation: Any,
    method_name: str,
) -> dict[str, Any]:
    if annotation is inspect.Signature.empty:
        raise TypeError(f"{method_name} contains a missing annotation.")
    if annotation is Any:
        raise TypeError(f"{method_name} uses Any, which is not a stable RPC ABI.")
    if isinstance(annotation, (str, ForwardRef)):
        raise ValueError(
            f"{method_name} contains an unresolved forward reference.",
        )
    if annotation is None or annotation is type(None):
        return {"kind": "none"}
    if annotation in _PRIMITIVES:
        return {"kind": "primitive", "name": _PRIMITIVES[annotation]}
    if annotation in _BARE_CONTAINERS:
        raise TypeError(f"{method_name} uses a bare container annotation.")
    if annotation is _payload_type():
        return {"kind": "payload"}

    origin = get_origin(annotation)
    args = get_args(annotation)
    if origin in {Union, types.UnionType}:
        items = [
            _annotation_descriptor(item, method_name)
            for item in args
        ]
        items.sort(key=_canonical_json)
        return {"items": items, "kind": "union"}
    if origin is list:
        if not args:
            raise TypeError(f"{method_name} uses a bare container annotation.")
        return {
            "item": _annotation_descriptor(args[0], method_name),
            "kind": "list",
        }
    if origin is dict:
        if len(args) != 2:
            raise TypeError(f"{method_name} uses a bare container annotation.")
        return {
            "key": _annotation_descriptor(args[0], method_name),
            "kind": "dict",
            "value": _annotation_descriptor(args[1], method_name),
        }
    if origin is tuple:
        if not args:
            raise TypeError(f"{method_name} uses a bare container annotation.")
        if len(args) == 2 and args[1] is Ellipsis:
            return {
                "item": _annotation_descriptor(args[0], method_name),
                "kind": "tuple_variadic",
            }
        return {
            "items": [
                _annotation_descriptor(item, method_name)
                for item in args
            ],
            "kind": "tuple",
        }
    if isinstance(annotation, type):
        module = getattr(annotation, "__module__", None)
        name = getattr(annotation, "__name__", None)
        return {
            "kind": "python_type",
            "module": module if isinstance(module, str) and module else "<unknown>",
            "name": name if isinstance(name, str) and name else "<anonymous>",
        }
    raise TypeError(
        f"{method_name} contains an unsupported annotation {annotation!r}.",
    )


def _default_descriptor(
    value: object,
    method_name: str,
    parameter_name: str,
) -> dict[str, Any]:
    if value is inspect.Parameter.empty:
        return {"kind": "missing"}
    if value is None:
        return {"kind": "json_scalar", "value": None}
    if isinstance(value, bool):
        return {"kind": "json_scalar", "value": value}
    if isinstance(value, int) and not isinstance(value, bool):
        return {"kind": "json_scalar", "value": value}
    if isinstance(value, float):
        if not math.isfinite(value):
            raise ValueError(
                f"{method_name}.{parameter_name} has a non-finite default.",
            )
        return {"kind": "json_scalar", "value": value}
    if isinstance(value, str):
        return {"kind": "json_scalar", "value": value}
    raise TypeError(
        f"{method_name}.{parameter_name} has unsupported default value "
        f"{value!r}; only JSON scalar defaults are allowed.",
    )


def _hash_descriptor(descriptor: dict[str, Any]) -> str:
    from c_two._native import contract_descriptor_sha256_hex

    return contract_descriptor_sha256_hex(_canonical_json(descriptor).encode())


def _pretty_json(compact: str) -> str:
    return json.dumps(json.loads(compact), sort_keys=True, indent=2) + "\n"


def _canonical_json(value: object) -> str:
    return json.dumps(
        value,
        ensure_ascii=False,
        allow_nan=False,
        sort_keys=True,
        separators=(",", ":"),
    )


def _payload_type():
    from fastdb4py.payload import Payload

    return Payload
