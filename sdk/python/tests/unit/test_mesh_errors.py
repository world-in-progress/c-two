from c_two.error import (
    ResourceAlreadyRegistered,
    ResourceNotFound,
    ResourceUnavailable,
    RegistryUnavailable,
    StaleResource,
    WriteConflict,
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

    def test_stale_resource_code(self):
        err = StaleResource("grid stale")
        assert err.ERROR_CODE == 704

    def test_write_conflict_code(self):
        err = WriteConflict("grid conflict")
        assert err.ERROR_CODE == 706

    def test_error_serialize_roundtrip(self):
        err = ResourceNotFound("grid")
        data = ResourceNotFound.serialize(err)
        restored = ResourceNotFound.deserialize(memoryview(data))
        assert restored.message is not None
