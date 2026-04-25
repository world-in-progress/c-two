from c_two.error import ResourceNotFound, ResourceUnavailable, RegistryUnavailable


class TestMeshErrors:
    def test_resource_not_found_code(self):
        err = ResourceNotFound("grid")
        assert err.ERROR_CODE == 701

    def test_resource_unavailable_code(self):
        err = ResourceUnavailable("grid", "connection refused")
        assert err.ERROR_CODE == 702

    def test_registry_unavailable_code(self):
        err = RegistryUnavailable("no relay configured")
        assert err.ERROR_CODE == 705

    def test_error_serialize_roundtrip(self):
        err = ResourceNotFound("grid")
        data = ResourceNotFound.serialize(err)
        restored = ResourceNotFound.deserialize(memoryview(data))
        assert restored.message is not None
