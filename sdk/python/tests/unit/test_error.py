import copy
import gc
import pickle
import weakref

import pytest
from c_two import _native
import c_two.error as error
from c_two.error import (
    ErrorCode, CCBaseError, CCError, CoreError,
    ResourceExecutionError, SerializationError,
    ResourceNotFound,
    ResourceUnavailable,
    ResourceAlreadyRegistered,
    ResourceClosed, ResourceRemoved, ContractMismatch, IdentityMismatch,
    ProtocolViolation, FallbackDenied,
    CallDeadlineExceeded, UnsupportedCallMode, CallCapacityExceeded, CallCancelled,
    RouteConflict,
)


def test_core_error_copy_and_pickle_preserve_structured_fields():
    with pytest.raises(error.CoreError) as failure:
        _native.ConnectAttempt(timeout_seconds=0)
    original = failure.value
    assert original.name == 'CallDeadlineExceeded'
    assert original.details['operation'] == 'connect'
    for restored in (copy.copy(original), pickle.loads(pickle.dumps(original))):
        assert type(restored) is error.CoreError
        assert restored is not original
        assert restored.args == original.args
        assert str(restored) == str(original)
        assert restored.__dict__ == original.__dict__


class TestErrorCode:
    def test_member_names_match_rust_registry(self):
        expected = {
            "Unknown": 0,
            "ResourceInputDeserializing": 1,
            "ResourceOutputSerializing": 2,
            "ResourceFunctionExecuting": 3,
            "ResourceInputFromBuffer": 4,
            "ClientInputSerializing": 5,
            "ClientOutputDeserializing": 6,
            "ClientCallingResource": 7,
            "ClientOutputFromBuffer": 8,
            "ResourceNotFound": 701,
            "ResourceUnavailable": 702,
            "ResourceAlreadyRegistered": 703,
            "RegistryUnavailable": 705,
            "ResourceClosed": 707,
            "ResourceRemoved": 708,
            "ContractMismatch": 709,
            "IdentityMismatch": 710,
            "RouteWatchUnavailable": 712,
            "ProtocolViolation": 713,
            "FallbackDenied": 714,
            "CallDeadlineExceeded": 715,
            "UnsupportedCallMode": 716,
            "CallCapacityExceeded": 717,
            "CallCancelled": 718,
            "RouteConflict": 719,
        }
        assert {member.name: member.value for member in ErrorCode} == expected

    def test_members_match_native_registry_exactly(self):
        native = _native.error_registry()
        assert len(native) == 25
        assert set(ErrorCode.__members__) == set(native)
        for name, (code, _side, _stage) in native.items():
            assert ErrorCode[name].value == int(code)

    def test_has_exactly_25_members(self):
        assert len(ErrorCode) == 25

    def test_values_are_unique(self):
        values = [member.value for member in ErrorCode]
        assert len(values) == len(set(values))

    def test_is_int_enum(self):
        assert isinstance(ErrorCode.Unknown, int)

    def test_code_meta_classifies_adapter_codes(self):
        assert error._CODE_META[int(ErrorCode.ResourceInputDeserializing)] == ('resource', 'deserialize')
        assert error._CODE_META[int(ErrorCode.ResourceOutputSerializing)] == ('resource', 'serialize')
        assert error._CODE_META[int(ErrorCode.ResourceFunctionExecuting)] == ('resource', 'execute')
        assert error._CODE_META[int(ErrorCode.ResourceInputFromBuffer)] == ('resource', 'from_buffer')
        assert error._CODE_META[int(ErrorCode.ClientInputSerializing)] == ('client', 'serialize')
        assert error._CODE_META[int(ErrorCode.ClientOutputDeserializing)] == ('client', 'deserialize')
        assert error._CODE_META[int(ErrorCode.ClientCallingResource)] == ('client', 'call')
        assert error._CODE_META[int(ErrorCode.ClientOutputFromBuffer)] == ('client', 'from_buffer')
        for code in (ErrorCode.Unknown, ErrorCode.ResourceClosed, ErrorCode.RouteConflict):
            assert error._CODE_META[int(code)] == (None, None)


class TestCCError:
    def test_default_creation(self):
        err = CCError()
        assert err.code == ErrorCode.Unknown
        assert err.message == 'Error occurred when using C-Two.'

    def test_custom_creation(self):
        err = CCError(code=ErrorCode.ResourceFunctionExecuting, message='something broke')
        assert err.code == ErrorCode.ResourceFunctionExecuting
        assert err.message == 'something broke'

    def test_explicit_empty_message_is_preserved(self):
        err = CCError(code=ErrorCode.Unknown, message='')
        assert err.message == ''

    def test_details_creation(self):
        err = CCError(
            code=ErrorCode.ResourceUnavailable,
            message='upstream failed',
            details={'route': 'grid', 'cause_kind': 'TransportIo'},
        )
        assert err.details == {'route': 'grid', 'cause_kind': 'TransportIo'}

    def test_str_format(self):
        err = CCError(code=ErrorCode.ResourceFunctionExecuting, message='oops')
        assert str(err) == 'ResourceFunctionExecuting: oops'

    def test_repr_format(self):
        err = CCError(code=ErrorCode.ResourceFunctionExecuting, message='oops')
        assert repr(err) == 'CCError(code=3, message=oops, details={})'

    def test_is_exception(self):
        err = CCError()
        assert isinstance(err, Exception)
        assert isinstance(err, CCBaseError)

    def test_can_be_raised_and_caught(self):
        with pytest.raises(CCError):
            raise CCError(message='fail')

    def test_side_and_stage_read_registry_meta(self):
        err = CCError(code=ErrorCode.ClientOutputFromBuffer)
        assert err.side == 'client'
        assert err.stage == 'from_buffer'
        assert CCError(code=ErrorCode.ResourceFunctionExecuting).side == 'resource'
        assert CCError(code=ErrorCode.ResourceFunctionExecuting).stage == 'execute'
        assert CCError(code=ErrorCode.RouteConflict).side is None
        assert CCError(code=ErrorCode.RouteConflict).stage is None


class TestTransportPhase:
    def test_phase_from_details_transport_key(self):
        err = CCError(ErrorCode.CallDeadlineExceeded, 'late', {'transport_phase': 'pre_dispatch'})
        assert err.transport_phase == 'pre_dispatch'

    def test_phase_from_details_dispatch_key(self):
        err = CCError(ErrorCode.CallDeadlineExceeded, 'late', {'dispatch_phase': 'dispatch_uncertain'})
        assert err.transport_phase == 'dispatch_uncertain'

    def test_phase_none_without_canonical_detail(self):
        err = CCError(ErrorCode.CallDeadlineExceeded, 'late', {'transport_phase': 'application_detail'})
        assert err.transport_phase is None

    def test_explicit_phase_wins_over_details(self):
        err = CCError(
            ErrorCode.CallDeadlineExceeded, 'late',
            {'transport_phase': 'dispatch_uncertain'},
            transport_phase='pre_dispatch',
        )
        assert err.transport_phase == 'pre_dispatch'

    def test_later_details_edits_do_not_change_phase(self):
        err = CCError(ErrorCode.CallDeadlineExceeded, 'late')
        assert err.transport_phase is None
        err.details['transport_phase'] = 'pre_dispatch'
        assert err.transport_phase is None

    def test_invalid_phase_raises_value_error(self):
        with pytest.raises(ValueError):
            CCError(ErrorCode.CallDeadlineExceeded, 'late', transport_phase='post_dispatch')

    def test_phase_is_read_only_property(self):
        err = CCError(ErrorCode.CallDeadlineExceeded, 'late', transport_phase='pre_dispatch')
        with pytest.raises(AttributeError):
            err.transport_phase = 'dispatch_uncertain'
        assert err.transport_phase == 'pre_dispatch'

    def test_phase_not_inferred_from_message_text(self):
        err = CCError(ErrorCode.CallDeadlineExceeded, 'failed during dispatch_uncertain window')
        assert err.transport_phase is None


class TestCCErrorFromParts:
    def test_known_code_maps_to_public_subclass(self):
        err = CCError._from_parts(707, 'route closed', {'route': 'grid'})
        assert type(err) is ResourceClosed
        assert err.code == ErrorCode.ResourceClosed
        assert err.message == 'route closed'
        assert err.details == {'route': 'grid'}

    def test_unknown_code_degrades_to_unknown_with_detail(self):
        err = CCError._from_parts(9999, 'low-level relay failure', {'route': 'grid'})
        assert type(err) is CCError
        assert err.code == ErrorCode.Unknown
        assert err.message == 'low-level relay failure'
        assert err.details['route'] == 'grid'
        assert err.details['unknown_code'] == '9999'

    def test_unmapped_codes_fall_back_to_base_class(self):
        for code in (0, 7, 705, 712):
            err = CCError._from_parts(code, 'x')
            assert type(err) is CCError

    def test_explicit_phase_is_kept(self):
        err = CCError._from_parts(715, 'late', transport_phase='dispatch_uncertain')
        assert err.transport_phase == 'dispatch_uncertain'

    def test_phase_inferred_from_details_only_when_canonical(self):
        inferred = CCError._from_parts(715, 'late', {'dispatch_phase': 'pre_dispatch'})
        assert inferred.transport_phase == 'pre_dispatch'
        ignored = CCError._from_parts(715, 'late', {'dispatch_phase': 'weird'})
        assert ignored.transport_phase is None


class TestCCErrorCopyAndPickle:
    ROUND_TRIP_CASES = [
        CCError(ErrorCode.Unknown, 'generic'),
        CCError(ErrorCode.ClientCallingResource, 'call failed', {'route': 'grid'}),
        SerializationError(ErrorCode.ClientInputSerializing, 'bad input', transport_phase='pre_dispatch'),
        SerializationError(ErrorCode.ClientOutputFromBuffer, 'bad view', {'cause_owner': 'fastdb'}),
        ResourceExecutionError('resource raised'),
        ResourceClosed('closed', {'route': 'grid'}),
        CallDeadlineExceeded('late', {'transport_phase': 'dispatch_uncertain'}),
    ]

    @pytest.mark.parametrize("original", ROUND_TRIP_CASES, ids=lambda c: type(c).__name__)
    @pytest.mark.parametrize("round_trip", [copy.copy, lambda e: pickle.loads(pickle.dumps(e))],
                             ids=['copy', 'pickle'])
    def test_round_trip_preserves_everything(self, original, round_trip):
        restored = round_trip(original)
        assert type(restored) is type(original)
        assert restored is not original
        assert restored.code == original.code
        assert restored.message == original.message
        assert restored.details == original.details
        assert restored.transport_phase == original.transport_phase
        assert str(restored) == str(original)

    def test_deeply_nested_subclass_round_trip(self):
        original = RouteConflict('duplicate', details={'registrations': '[]'})
        restored = pickle.loads(pickle.dumps(original))
        assert isinstance(restored, RouteConflict)
        assert restored.code == ErrorCode.RouteConflict
        assert restored.details == {'registrations': '[]'}


class TestSerializationError:
    @pytest.mark.parametrize("code", [
        ErrorCode.ResourceInputDeserializing,
        ErrorCode.ResourceOutputSerializing,
        ErrorCode.ResourceInputFromBuffer,
        ErrorCode.ClientInputSerializing,
        ErrorCode.ClientOutputDeserializing,
        ErrorCode.ClientOutputFromBuffer,
    ])
    def test_accepts_the_six_codec_codes(self, code):
        err = SerializationError(code, 'detail')
        assert err.code == code
        assert err.stage in {'serialize', 'deserialize', 'from_buffer'}

    @pytest.mark.parametrize("code", [0, 3, 7, 701, 703, 707, 713, 715, 717, 719])
    def test_rejects_non_codec_codes(self, code):
        with pytest.raises(ValueError):
            SerializationError(code)

    def test_side_and_stage_follow_code(self):
        assert SerializationError(ErrorCode.ResourceInputFromBuffer).side == 'resource'
        assert SerializationError(ErrorCode.ResourceInputFromBuffer).stage == 'from_buffer'
        assert SerializationError(ErrorCode.ClientOutputDeserializing).stage == 'deserialize'
        assert SerializationError(ErrorCode.ClientInputSerializing).stage == 'serialize'


class TestResourceExecutionError:
    def test_fixed_code(self):
        err = ResourceExecutionError('boom')
        assert err.code == ErrorCode.ResourceFunctionExecuting
        assert int(err.code) == 3
        assert err.side == 'resource'
        assert err.stage == 'execute'

    def test_default_message(self):
        assert ResourceExecutionError().message


ROUTE_SUBCLASS_PARAMS = [
    (ResourceNotFound, 701, 'Resource not found'),
    (ResourceUnavailable, 702, 'Resource unavailable'),
    (ResourceAlreadyRegistered, 703, 'Resource already registered'),
    (ResourceClosed, 707, 'Resource closed'),
    (ResourceRemoved, 708, 'Resource removed'),
    (ContractMismatch, 709, 'Contract mismatch'),
    (IdentityMismatch, 710, 'Identity mismatch'),
    (ProtocolViolation, 713, 'Protocol violation'),
    (FallbackDenied, 714, 'Fallback denied'),
    (CallDeadlineExceeded, 715, 'Call deadline exceeded'),
    (UnsupportedCallMode, 716, 'Unsupported call mode'),
    (CallCapacityExceeded, 717, 'Call capacity exceeded'),
    (CallCancelled, 718, 'Call cancelled'),
    (RouteConflict, 719, 'Route conflict'),
]


class TestRouteSubclasses:
    @pytest.mark.parametrize("cls,code,default_message", ROUTE_SUBCLASS_PARAMS)
    def test_fixed_code_and_default_message(self, cls, code, default_message):
        err = cls()
        assert int(err.code) == code
        assert err.message == default_message
        assert err.transport_phase is None

    @pytest.mark.parametrize("cls,code,default_message", ROUTE_SUBCLASS_PARAMS)
    def test_custom_message_and_details(self, cls, code, default_message):
        err = cls('detail', details={'route': 'grid'})
        assert int(err.code) == code
        assert err.message == 'detail'
        assert err.details == {'route': 'grid'}

    @pytest.mark.parametrize("cls,code,default_message", ROUTE_SUBCLASS_PARAMS)
    def test_explicit_empty_message_is_kept(self, cls, code, default_message):
        err = cls('')
        assert err.message == ''

    @pytest.mark.parametrize("cls,code,default_message", ROUTE_SUBCLASS_PARAMS)
    def test_is_cc_error(self, cls, code, default_message):
        err = cls()
        assert isinstance(err, CCError)
        assert isinstance(err, CCBaseError)
        assert isinstance(err, Exception)

    def test_resource_unavailable_detail_parameter(self):
        err = ResourceUnavailable(detail='relay left the mesh')
        assert err.message == 'Resource unavailable: relay left the mesh'


class TestPublicSurface:
    def test_all_exports_the_standard_surface(self):
        expected = {
            'ErrorCode', 'CCBaseError', 'CoreError', 'CCError',
            'ResourceExecutionError', 'SerializationError',
            'ResourceNotFound', 'ResourceUnavailable', 'ResourceAlreadyRegistered',
            'ResourceClosed', 'ResourceRemoved', 'ContractMismatch', 'IdentityMismatch',
            'ProtocolViolation', 'FallbackDenied', 'CallDeadlineExceeded',
            'UnsupportedCallMode', 'CallCapacityExceeded', 'CallCancelled', 'RouteConflict',
        }
        assert set(error.__all__) == expected

    def test_code_to_class_covers_mapped_codes(self):
        mapped = {int(code) for code in ErrorCode} - {0, 7, 705, 712}
        assert set(error._CODE_TO_CLASS) == mapped
        assert error._CODE_TO_CLASS[int(ErrorCode.ResourceFunctionExecuting)] is ResourceExecutionError
        assert error._CODE_TO_CLASS[int(ErrorCode.ResourceInputFromBuffer)] is SerializationError
        assert error._CODE_TO_CLASS[int(ErrorCode.ResourceClosed)] is ResourceClosed


class TestCoreErrorSemantic:
    def _semantic_core(self, **kwargs):
        core = CoreError('semantic', 'outer diagnostic')
        core.code = kwargs.get('code', 715)
        core.name = 'CallDeadlineExceeded'
        core.details = kwargs.get('details', {'route': 'grid'})
        core.semantic_message = kwargs.get('semantic_message')
        core._transport_phase = kwargs.get('phase')
        core._semantic_snapshot = (
            core.code, core.semantic_message or core.message, tuple(core.details.items()),
            core.transport_phase,
        )
        return core

    def test_none_code_has_no_semantic(self):
        assert CoreError('transport', 'boom').semantic is None

    def test_semantic_builds_fresh_instance_per_call(self):
        core = self._semantic_core(semantic_message='inner message')
        first = core.semantic
        second = core.semantic
        assert isinstance(first, CallDeadlineExceeded)
        assert first is not second
        assert first.message == 'inner message'
        assert first.details == {'route': 'grid'}

    def test_semantic_carries_core_phase(self):
        core = self._semantic_core(semantic_message='inner', phase='dispatch_uncertain')
        assert core.semantic.transport_phase == 'dispatch_uncertain'

    def test_semantic_not_cached_in_core_dict(self):
        core = self._semantic_core(semantic_message='inner')
        _ = core.semantic
        assert 'semantic' not in core.__dict__
        assert not any(
            isinstance(value, CCError) for value in core.__dict__.values()
        )

    def test_semantic_unknown_code_degrades(self):
        core = self._semantic_core(code=9999)
        projected = core.semantic
        assert type(projected) is CCError
        assert projected.code == ErrorCode.Unknown
        assert projected.details['unknown_code'] == '9999'

class TestCoreErrorPhaseAuthority:
    def test_phase_is_read_only_and_native_written(self):
        core = CoreError('transport', 'boom')
        assert core.transport_phase is None
        core._transport_phase = 'pre_dispatch'
        assert core.transport_phase == 'pre_dispatch'
        with pytest.raises(AttributeError):
            core.transport_phase = None

    def test_native_failure_projects_phase(self):
        with pytest.raises(CoreError) as failure:
            _native.ConnectAttempt(timeout_seconds=0)
        assert failure.value.transport_phase == 'pre_dispatch'
        assert failure.value.semantic.transport_phase == 'pre_dispatch'


class _BusinessError(CCError):
    def __init__(self, *, record):
        super().__init__(ErrorCode.ResourceClosed, 'business')
        self.record = record


class _BusinessCoreError(CoreError):
    __slots__ = ('record',)

    def __init__(self, *, record):
        super().__init__('lifecycle', 'business')
        self.record = record


@pytest.mark.parametrize('error_class', [_BusinessError, _BusinessCoreError])
@pytest.mark.parametrize('round_trip', [
    copy.copy, copy.deepcopy, lambda value: pickle.loads(pickle.dumps(value)),
])
def test_copy_and_pickle_keep_custom_subclass_state(error_class, round_trip):
    original = error_class(record={'id': 42})
    original.args = ('caller changed args', 3)
    original.__notes__ = ['caller note']
    original.details['route'] = 'grid'

    restored = round_trip(original)

    assert type(restored) is error_class
    assert restored.args == original.args
    assert restored.__notes__ == original.__notes__
    assert restored.record == original.record
    assert restored.details == original.details
    assert restored.details is not original.details


@pytest.mark.parametrize('error_class', [CCError, SerializationError])
@pytest.mark.parametrize('invalid_code', [5.9, True, '5'])
def test_error_constructors_reject_non_integer_codes(error_class, invalid_code):
    with pytest.raises((TypeError, ValueError)):
        error_class(invalid_code)


def test_canonical_transport_phase_key_has_precedence():
    details = {'dispatch_phase': 'pre_dispatch', 'transport_phase': 'PRE_DISPATCH'}
    assert CCError(ErrorCode.ResourceClosed, details=details).transport_phase is None
    assert CCError._from_parts(707, details=details).transport_phase is None
    del details['transport_phase']
    assert CCError(ErrorCode.ResourceClosed, details=details).transport_phase == 'pre_dispatch'


def test_native_snapshot_survives_changes_to_public_diagnostics():
    with pytest.raises(CoreError) as failure:
        _native.ConnectAttempt(timeout_seconds=0)
    native = failure.value
    expected = native.semantic
    native.code = int(ErrorCode.ResourceClosed)
    native.semantic_message = 'overwritten'
    native.details.clear()
    native.args = ('changed native args',)
    native.__notes__ = ['keep this note']

    for restored in (native, copy.copy(native), copy.deepcopy(native), pickle.loads(pickle.dumps(native))):
        assert restored.args == native.args
        assert restored.__notes__ == native.__notes__
        assert isinstance(restored.semantic, CallDeadlineExceeded)
        assert restored.semantic.message == expected.message
        assert restored.semantic.details == expected.details
        assert restored.semantic.transport_phase == 'pre_dispatch'


@pytest.mark.parametrize('operation', ['native_call', 'connect'])
def test_real_native_failure_projection_releases_both_errors_without_gc(operation):
    from c_two import connect
    from c_two.transport.client.proxy import _native_call

    def scenario():
        try:
            if operation == 'native_call':
                _native_call(_native.ConnectAttempt, timeout_seconds=0)
            else:
                connect(object, name='deadline', timeout=0)
        except CallDeadlineExceeded as exc:
            assert isinstance(exc.__cause__, CoreError)
            return weakref.ref(exc), weakref.ref(exc.__cause__)
        raise AssertionError('expected real native deadline refusal')

    gc_was_enabled = gc.isenabled()
    gc.disable()
    try:
        projected_ref, cause_ref = scenario()
        assert projected_ref() is None
        assert cause_ref() is None
    finally:
        if gc_was_enabled:
            gc.enable()
