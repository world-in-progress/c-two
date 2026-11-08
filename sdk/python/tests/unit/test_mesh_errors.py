from c_two.error import (
    ResourceAlreadyRegistered,
    ResourceNotFound,
    ResourceUnavailable,
    RegistryUnavailable,
    RouteConflict,
    RouteWatchUnavailable,
)


class TestMeshErrors:
    def test_resource_not_found_code(self):
        err = ResourceNotFound("grid")
        assert err.ERROR_CODE == 701

    def test_resource_unavailable_code(self):
        err = ResourceUnavailable("grid", "connection refused")
        assert err.ERROR_CODE == 702

    def test_resource_already_registered_code(self):
        err = ResourceAlreadyRegistered("grid")
        assert err.ERROR_CODE == 703

    def test_registry_unavailable_code(self):
        err = RegistryUnavailable("no relay configured")
        assert err.ERROR_CODE == 705

    def test_route_watch_unavailable_code(self):
        err = RouteWatchUnavailable("watch unavailable")
        assert err.ERROR_CODE == 712

    def test_route_conflict_code(self):
        err = RouteConflict("grid", details={"route": "grid", "dispatch_phase": "pre_dispatch"})
        assert err.ERROR_CODE == 719
        assert err.details == {"route": "grid", "dispatch_phase": "pre_dispatch"}

    def test_error_serialize_roundtrip(self):
        err = ResourceNotFound("grid")
        data = ResourceNotFound.serialize(err)
        restored = ResourceNotFound.deserialize(memoryview(data))
        assert restored.message is not None
