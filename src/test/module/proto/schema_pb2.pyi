"""
@generated by mypy-protobuf.  Do not edit manually!
isort:skip_file
"""

import builtins
import collections.abc
import google.protobuf.descriptor
import google.protobuf.internal.containers
import google.protobuf.internal.enum_type_wrapper
import google.protobuf.message
import sys
import typing

if sys.version_info >= (3, 10):
    import typing as typing_extensions
else:
    import typing_extensions

DESCRIPTOR: google.protobuf.descriptor.FileDescriptor

class _Status:
    ValueType = typing.NewType("ValueType", builtins.int)
    V: typing_extensions.TypeAlias = ValueType

class _StatusEnumTypeWrapper(google.protobuf.internal.enum_type_wrapper._EnumTypeWrapper[_Status.ValueType], builtins.type):
    DESCRIPTOR: google.protobuf.descriptor.EnumDescriptor
    UNKNOWN: _Status.ValueType  # 0
    SUCCESS: _Status.ValueType  # 1
    ERROR_INVALID: _Status.ValueType  # 2
    ERROR_TIMEOUT: _Status.ValueType  # 3
    ERROR_UNAVAILABLE: _Status.ValueType  # 4
    BUSY: _Status.ValueType  # 5
    IDLE: _Status.ValueType  # 6
    PENDING: _Status.ValueType  # 7

class Status(_Status, metaclass=_StatusEnumTypeWrapper): ...

UNKNOWN: Status.ValueType  # 0
SUCCESS: Status.ValueType  # 1
ERROR_INVALID: Status.ValueType  # 2
ERROR_TIMEOUT: Status.ValueType  # 3
ERROR_UNAVAILABLE: Status.ValueType  # 4
BUSY: Status.ValueType  # 5
IDLE: Status.ValueType  # 6
PENDING: Status.ValueType  # 7
global___Status = Status

@typing.final
class GridAttribute(google.protobuf.message.Message):
    DESCRIPTOR: google.protobuf.descriptor.Descriptor

    DELETED_FIELD_NUMBER: builtins.int
    ACTIVATE_FIELD_NUMBER: builtins.int
    TYPE_FIELD_NUMBER: builtins.int
    LEVEL_FIELD_NUMBER: builtins.int
    GLOBAL_ID_FIELD_NUMBER: builtins.int
    ELEVATION_FIELD_NUMBER: builtins.int
    MIN_X_FIELD_NUMBER: builtins.int
    MIN_Y_FIELD_NUMBER: builtins.int
    MAX_X_FIELD_NUMBER: builtins.int
    MAX_Y_FIELD_NUMBER: builtins.int
    LOCAL_ID_FIELD_NUMBER: builtins.int
    deleted: builtins.bool
    activate: builtins.bool
    type: builtins.int
    level: builtins.int
    global_id: builtins.int
    elevation: builtins.float
    min_x: builtins.float
    min_y: builtins.float
    max_x: builtins.float
    max_y: builtins.float
    local_id: builtins.int
    def __init__(
        self,
        *,
        deleted: builtins.bool = ...,
        activate: builtins.bool = ...,
        type: builtins.int = ...,
        level: builtins.int = ...,
        global_id: builtins.int = ...,
        elevation: builtins.float = ...,
        min_x: builtins.float | None = ...,
        min_y: builtins.float | None = ...,
        max_x: builtins.float | None = ...,
        max_y: builtins.float | None = ...,
        local_id: builtins.int | None = ...,
    ) -> None: ...
    def HasField(self, field_name: typing.Literal["_local_id", b"_local_id", "_max_x", b"_max_x", "_max_y", b"_max_y", "_min_x", b"_min_x", "_min_y", b"_min_y", "local_id", b"local_id", "max_x", b"max_x", "max_y", b"max_y", "min_x", b"min_x", "min_y", b"min_y"]) -> builtins.bool: ...
    def ClearField(self, field_name: typing.Literal["_local_id", b"_local_id", "_max_x", b"_max_x", "_max_y", b"_max_y", "_min_x", b"_min_x", "_min_y", b"_min_y", "activate", b"activate", "deleted", b"deleted", "elevation", b"elevation", "global_id", b"global_id", "level", b"level", "local_id", b"local_id", "max_x", b"max_x", "max_y", b"max_y", "min_x", b"min_x", "min_y", b"min_y", "type", b"type"]) -> None: ...
    @typing.overload
    def WhichOneof(self, oneof_group: typing.Literal["_local_id", b"_local_id"]) -> typing.Literal["local_id"] | None: ...
    @typing.overload
    def WhichOneof(self, oneof_group: typing.Literal["_max_x", b"_max_x"]) -> typing.Literal["max_x"] | None: ...
    @typing.overload
    def WhichOneof(self, oneof_group: typing.Literal["_max_y", b"_max_y"]) -> typing.Literal["max_y"] | None: ...
    @typing.overload
    def WhichOneof(self, oneof_group: typing.Literal["_min_x", b"_min_x"]) -> typing.Literal["min_x"] | None: ...
    @typing.overload
    def WhichOneof(self, oneof_group: typing.Literal["_min_y", b"_min_y"]) -> typing.Literal["min_y"] | None: ...

global___GridAttribute = GridAttribute

@typing.final
class SubdivideRule(google.protobuf.message.Message):
    DESCRIPTOR: google.protobuf.descriptor.Descriptor

    RULE_FIELD_NUMBER: builtins.int
    @property
    def rule(self) -> google.protobuf.internal.containers.RepeatedScalarFieldContainer[builtins.int]: ...
    def __init__(
        self,
        *,
        rule: collections.abc.Iterable[builtins.int] | None = ...,
    ) -> None: ...
    def ClearField(self, field_name: typing.Literal["rule", b"rule"]) -> None: ...

global___SubdivideRule = SubdivideRule

@typing.final
class InitializeRequest(google.protobuf.message.Message):
    DESCRIPTOR: google.protobuf.descriptor.Descriptor

    REDIS_HOST_FIELD_NUMBER: builtins.int
    REDIS_PORT_FIELD_NUMBER: builtins.int
    EPSG_FIELD_NUMBER: builtins.int
    BOUNDS_FIELD_NUMBER: builtins.int
    FIRST_SIZE_FIELD_NUMBER: builtins.int
    SUBDIVIDE_RULES_FIELD_NUMBER: builtins.int
    redis_host: builtins.str
    redis_port: builtins.int
    epsg: builtins.int
    @property
    def bounds(self) -> google.protobuf.internal.containers.RepeatedScalarFieldContainer[builtins.float]:
        """[min_x, min_y, max_x, max_y]"""

    @property
    def first_size(self) -> google.protobuf.internal.containers.RepeatedScalarFieldContainer[builtins.float]:
        """[width, height]"""

    @property
    def subdivide_rules(self) -> google.protobuf.internal.containers.RepeatedCompositeFieldContainer[global___SubdivideRule]: ...
    def __init__(
        self,
        *,
        redis_host: builtins.str = ...,
        redis_port: builtins.int = ...,
        epsg: builtins.int = ...,
        bounds: collections.abc.Iterable[builtins.float] | None = ...,
        first_size: collections.abc.Iterable[builtins.float] | None = ...,
        subdivide_rules: collections.abc.Iterable[global___SubdivideRule] | None = ...,
    ) -> None: ...
    def ClearField(self, field_name: typing.Literal["bounds", b"bounds", "epsg", b"epsg", "first_size", b"first_size", "redis_host", b"redis_host", "redis_port", b"redis_port", "subdivide_rules", b"subdivide_rules"]) -> None: ...

global___InitializeRequest = InitializeRequest

@typing.final
class InitializeResponse(google.protobuf.message.Message):
    DESCRIPTOR: google.protobuf.descriptor.Descriptor

    STATUS_FIELD_NUMBER: builtins.int
    MESSAGE_FIELD_NUMBER: builtins.int
    status: global___Status.ValueType
    message: builtins.str
    def __init__(
        self,
        *,
        status: global___Status.ValueType = ...,
        message: builtins.str = ...,
    ) -> None: ...
    def ClearField(self, field_name: typing.Literal["message", b"message", "status", b"status"]) -> None: ...

global___InitializeResponse = InitializeResponse

@typing.final
class GetLocalIdsRequest(google.protobuf.message.Message):
    DESCRIPTOR: google.protobuf.descriptor.Descriptor

    LEVEL_FIELD_NUMBER: builtins.int
    GLOBAL_IDS_FIELD_NUMBER: builtins.int
    level: builtins.int
    @property
    def global_ids(self) -> google.protobuf.internal.containers.RepeatedScalarFieldContainer[builtins.int]: ...
    def __init__(
        self,
        *,
        level: builtins.int = ...,
        global_ids: collections.abc.Iterable[builtins.int] | None = ...,
    ) -> None: ...
    def ClearField(self, field_name: typing.Literal["global_ids", b"global_ids", "level", b"level"]) -> None: ...

global___GetLocalIdsRequest = GetLocalIdsRequest

@typing.final
class GetLocalIdsResponse(google.protobuf.message.Message):
    DESCRIPTOR: google.protobuf.descriptor.Descriptor

    STATUS_FIELD_NUMBER: builtins.int
    MESSAGE_FIELD_NUMBER: builtins.int
    LOCAL_IDS_FIELD_NUMBER: builtins.int
    status: global___Status.ValueType
    message: builtins.str
    @property
    def local_ids(self) -> google.protobuf.internal.containers.RepeatedScalarFieldContainer[builtins.int]: ...
    def __init__(
        self,
        *,
        status: global___Status.ValueType = ...,
        message: builtins.str = ...,
        local_ids: collections.abc.Iterable[builtins.int] | None = ...,
    ) -> None: ...
    def ClearField(self, field_name: typing.Literal["local_ids", b"local_ids", "message", b"message", "status", b"status"]) -> None: ...

global___GetLocalIdsResponse = GetLocalIdsResponse

@typing.final
class GetCoordinatesRequest(google.protobuf.message.Message):
    DESCRIPTOR: google.protobuf.descriptor.Descriptor

    LEVEL_FIELD_NUMBER: builtins.int
    GLOBAL_IDS_FIELD_NUMBER: builtins.int
    level: builtins.int
    @property
    def global_ids(self) -> google.protobuf.internal.containers.RepeatedScalarFieldContainer[builtins.int]: ...
    def __init__(
        self,
        *,
        level: builtins.int = ...,
        global_ids: collections.abc.Iterable[builtins.int] | None = ...,
    ) -> None: ...
    def ClearField(self, field_name: typing.Literal["global_ids", b"global_ids", "level", b"level"]) -> None: ...

global___GetCoordinatesRequest = GetCoordinatesRequest

@typing.final
class GetCoordinatesResponse(google.protobuf.message.Message):
    DESCRIPTOR: google.protobuf.descriptor.Descriptor

    STATUS_FIELD_NUMBER: builtins.int
    MIN_XS_FIELD_NUMBER: builtins.int
    MIN_YS_FIELD_NUMBER: builtins.int
    MAX_XS_FIELD_NUMBER: builtins.int
    MAX_YS_FIELD_NUMBER: builtins.int
    MESSAGE_FIELD_NUMBER: builtins.int
    status: global___Status.ValueType
    message: builtins.str
    @property
    def min_xs(self) -> google.protobuf.internal.containers.RepeatedScalarFieldContainer[builtins.float]: ...
    @property
    def min_ys(self) -> google.protobuf.internal.containers.RepeatedScalarFieldContainer[builtins.float]: ...
    @property
    def max_xs(self) -> google.protobuf.internal.containers.RepeatedScalarFieldContainer[builtins.float]: ...
    @property
    def max_ys(self) -> google.protobuf.internal.containers.RepeatedScalarFieldContainer[builtins.float]: ...
    def __init__(
        self,
        *,
        status: global___Status.ValueType = ...,
        min_xs: collections.abc.Iterable[builtins.float] | None = ...,
        min_ys: collections.abc.Iterable[builtins.float] | None = ...,
        max_xs: collections.abc.Iterable[builtins.float] | None = ...,
        max_ys: collections.abc.Iterable[builtins.float] | None = ...,
        message: builtins.str = ...,
    ) -> None: ...
    def ClearField(self, field_name: typing.Literal["max_xs", b"max_xs", "max_ys", b"max_ys", "message", b"message", "min_xs", b"min_xs", "min_ys", b"min_ys", "status", b"status"]) -> None: ...

global___GetCoordinatesResponse = GetCoordinatesResponse

@typing.final
class GetGridInfosRequest(google.protobuf.message.Message):
    DESCRIPTOR: google.protobuf.descriptor.Descriptor

    LEVEL_FIELD_NUMBER: builtins.int
    GLOBAL_IDS_FIELD_NUMBER: builtins.int
    level: builtins.int
    @property
    def global_ids(self) -> google.protobuf.internal.containers.RepeatedScalarFieldContainer[builtins.int]: ...
    def __init__(
        self,
        *,
        level: builtins.int = ...,
        global_ids: collections.abc.Iterable[builtins.int] | None = ...,
    ) -> None: ...
    def ClearField(self, field_name: typing.Literal["global_ids", b"global_ids", "level", b"level"]) -> None: ...

global___GetGridInfosRequest = GetGridInfosRequest

@typing.final
class GetGridInfosResponse(google.protobuf.message.Message):
    DESCRIPTOR: google.protobuf.descriptor.Descriptor

    STATUS_FIELD_NUMBER: builtins.int
    GRID_INFOS_FIELD_NUMBER: builtins.int
    MESSAGE_FIELD_NUMBER: builtins.int
    status: global___Status.ValueType
    message: builtins.str
    @property
    def grid_infos(self) -> google.protobuf.internal.containers.RepeatedCompositeFieldContainer[global___GridAttribute]: ...
    def __init__(
        self,
        *,
        status: global___Status.ValueType = ...,
        grid_infos: collections.abc.Iterable[global___GridAttribute] | None = ...,
        message: builtins.str = ...,
    ) -> None: ...
    def ClearField(self, field_name: typing.Literal["grid_infos", b"grid_infos", "message", b"message", "status", b"status"]) -> None: ...

global___GetGridInfosResponse = GetGridInfosResponse

@typing.final
class SubdivideGridsRequest(google.protobuf.message.Message):
    DESCRIPTOR: google.protobuf.descriptor.Descriptor

    LEVELS_FIELD_NUMBER: builtins.int
    GLOBAL_IDS_FIELD_NUMBER: builtins.int
    BATCH_SIZE_FIELD_NUMBER: builtins.int
    batch_size: builtins.int
    @property
    def levels(self) -> google.protobuf.internal.containers.RepeatedScalarFieldContainer[builtins.int]: ...
    @property
    def global_ids(self) -> google.protobuf.internal.containers.RepeatedScalarFieldContainer[builtins.int]: ...
    def __init__(
        self,
        *,
        levels: collections.abc.Iterable[builtins.int] | None = ...,
        global_ids: collections.abc.Iterable[builtins.int] | None = ...,
        batch_size: builtins.int = ...,
    ) -> None: ...
    def ClearField(self, field_name: typing.Literal["batch_size", b"batch_size", "global_ids", b"global_ids", "levels", b"levels"]) -> None: ...

global___SubdivideGridsRequest = SubdivideGridsRequest

@typing.final
class SubdivideGridsResponse(google.protobuf.message.Message):
    DESCRIPTOR: google.protobuf.descriptor.Descriptor

    STATUS_FIELD_NUMBER: builtins.int
    GRID_KEYS_FIELD_NUMBER: builtins.int
    MESSAGE_FIELD_NUMBER: builtins.int
    status: global___Status.ValueType
    message: builtins.str
    @property
    def grid_keys(self) -> google.protobuf.internal.containers.RepeatedScalarFieldContainer[builtins.str]: ...
    def __init__(
        self,
        *,
        status: global___Status.ValueType = ...,
        grid_keys: collections.abc.Iterable[builtins.str] | None = ...,
        message: builtins.str = ...,
    ) -> None: ...
    def ClearField(self, field_name: typing.Literal["grid_keys", b"grid_keys", "message", b"message", "status", b"status"]) -> None: ...

global___SubdivideGridsResponse = SubdivideGridsResponse
