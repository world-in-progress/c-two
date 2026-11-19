import pickle

from c_two.error import (
    CCError,
    ErrorCode,
    ResourceAlreadyRegistered,
    ResourceNotFound,
    ResourceUnavailable,
    RouteConflict,
)


class TestMeshErrors:
    def test_resource_not_found_code(self):
        err = ResourceNotFound("grid")
        assert err.code == ErrorCode.ResourceNotFound
        assert int(err.code) == 701

    def test_resource_unavailable_code(self):
        err = ResourceUnavailable("grid", "connection refused")
        assert err.code == ErrorCode.ResourceUnavailable
        assert int(err.code) == 702
        assert err.message == 'grid: connection refused'

    def test_resource_already_registered_code(self):
        err = ResourceAlreadyRegistered("grid")
        assert err.code == ErrorCode.ResourceAlreadyRegistered
        assert int(err.code) == 703

    def test_registry_unavailable_is_base_class_only(self):
        err = CCError(ErrorCode.RegistryUnavailable, "no relay configured")
        assert type(err) is CCError
        assert int(err.code) == 705

    def test_route_watch_unavailable_is_base_class_only(self):
        err = CCError(ErrorCode.RouteWatchUnavailable, "watch unavailable")
        assert type(err) is CCError
        assert int(err.code) == 712

    def test_route_conflict_code(self):
        err = RouteConflict("grid", details={"route": "grid", "dispatch_phase": "pre_dispatch"})
        assert err.code == ErrorCode.RouteConflict
        assert int(err.code) == 719
        assert err.details == {"route": "grid", "dispatch_phase": "pre_dispatch"}
        assert err.transport_phase == 'pre_dispatch'

    def test_error_pickle_roundtrip(self):
        err = ResourceNotFound("grid")
        restored = pickle.loads(pickle.dumps(err))
        assert type(restored) is ResourceNotFound
        assert restored.code == ErrorCode.ResourceNotFound
        assert restored.message == err.message
        assert str(restored) == 'ResourceNotFound: grid'
