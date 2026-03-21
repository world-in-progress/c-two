import pytest
import threading
from c_two.rpc.event import Event, EventTag, EventQueue, CompletionType
from c_two.error import EventSerializeError, EventDeserializeError


class TestEventTag:
    def test_all_values_exist(self):
        expected = {
            "PING": "ping",
            "PONG": "pong",
            "EMPTY": "empty",
            "CRM_CALL": "crm_call",
            "CRM_REPLY": "crm_reply",
            "SHUTDOWN_ACK": "shutdown_ack",
            "SHUTDOWN_FROM_SERVER": "shutdown_from_server",
            "SHUTDOWN_FROM_CLIENT": "shutdown_from_client",
        }
        for name, value in expected.items():
            member = EventTag[name]
            assert member.value == value

    def test_member_count(self):
        assert len(EventTag) == 8

    def test_unique_values(self):
        values = [t.value for t in EventTag]
        assert len(values) == len(set(values))


class TestCompletionType:
    def test_all_values_exist(self):
        expected = {
            "OP_REQUEST": "op_request",
            "OP_TIMEOUT": "op_timeout",
            "OP_COMPLETE": "op_complete",
        }
        for name, value in expected.items():
            member = CompletionType[name]
            assert member.value == value

    def test_member_count(self):
        assert len(CompletionType) == 3


class TestEvent:
    def test_default_attributes(self):
        event = Event(tag=EventTag.PING)
        assert event.tag == EventTag.PING
        assert event.data is None
        assert event.completion_type == CompletionType.OP_REQUEST
        assert event.request_id is None

    def test_custom_attributes(self):
        event = Event(
            tag=EventTag.CRM_CALL,
            data=b"hello",
            completion_type=CompletionType.OP_COMPLETE,
            request_id="req-123",
        )
        assert event.tag == EventTag.CRM_CALL
        assert event.data == b"hello"
        assert event.completion_type == CompletionType.OP_COMPLETE
        assert event.request_id == "req-123"

    def test_data_defaults_to_none(self):
        event = Event(tag=EventTag.PONG)
        assert event.data is None

    def test_is_dataclass_with_equality(self):
        a = Event(tag=EventTag.PING, data=b"x")
        b = Event(tag=EventTag.PING, data=b"x")
        assert a == b

    def test_inequality(self):
        a = Event(tag=EventTag.PING)
        b = Event(tag=EventTag.PONG)
        assert a != b


class TestEventSerialization:
    @pytest.mark.parametrize("tag", list(EventTag))
    def test_round_trip_all_tags_no_data(self, tag):
        original = Event(tag=tag)
        restored = Event.deserialize(original.serialize())
        assert restored.tag == tag
        # deserialize produces empty bytes for data when original had None
        assert restored.data.tobytes() == b""

    @pytest.mark.parametrize("tag", list(EventTag))
    def test_round_trip_all_tags_with_data(self, tag):
        original = Event(tag=tag, data=b"payload-data")
        restored = Event.deserialize(original.serialize())
        assert restored.tag == tag
        assert restored.data.tobytes() == b"payload-data"

    def test_round_trip_preserves_binary_data(self):
        raw = bytes(range(256))
        original = Event(tag=EventTag.CRM_CALL, data=raw)
        restored = Event.deserialize(original.serialize())
        assert restored.data.tobytes() == raw

    def test_round_trip_empty_bytes_data(self):
        original = Event(tag=EventTag.PING, data=b"")
        restored = Event.deserialize(original.serialize())
        assert restored.data.tobytes() == b""

    def test_serialize_produces_bytes(self):
        event = Event(tag=EventTag.PING)
        assert isinstance(event.serialize(), bytes)

    def test_deserialize_does_not_restore_completion_type(self):
        """deserialize only restores tag and data; completion_type defaults."""
        original = Event(tag=EventTag.PING, completion_type=CompletionType.OP_COMPLETE)
        restored = Event.deserialize(original.serialize())
        assert restored.completion_type == CompletionType.OP_REQUEST

    def test_deserialize_does_not_restore_request_id(self):
        original = Event(tag=EventTag.PING, request_id="abc")
        restored = Event.deserialize(original.serialize())
        assert restored.request_id is None

    def test_deserialize_empty_bytes_raises(self):
        with pytest.raises(EventDeserializeError):
            Event.deserialize(b"")

    def test_deserialize_garbage_raises(self):
        with pytest.raises(EventDeserializeError):
            Event.deserialize(b"\x00\x01\x02")

    def test_round_trip_with_request_id_event(self):
        """request_id is not serialized but event still round-trips for tag/data."""
        original = Event(tag=EventTag.CRM_REPLY, data=b"resp", request_id="r1")
        restored = Event.deserialize(original.serialize())
        assert restored.tag == EventTag.CRM_REPLY
        assert restored.data.tobytes() == b"resp"


class TestEventQueue:
    def test_put_then_poll(self):
        q = EventQueue()
        event = Event(tag=EventTag.PING, data=b"hi")
        q.put(event)
        result = q.poll(timeout=1.0)
        assert result.tag == EventTag.PING
        assert result.data == b"hi"
        assert result.completion_type == CompletionType.OP_COMPLETE

    def test_poll_timeout_returns_empty_event(self):
        q = EventQueue()
        result = q.poll(timeout=0.05)
        assert result.tag == EventTag.EMPTY
        assert result.completion_type == CompletionType.OP_TIMEOUT
        assert result.data is None

    def test_fifo_order(self):
        q = EventQueue()
        tags = [EventTag.PING, EventTag.PONG, EventTag.CRM_CALL]
        for tag in tags:
            q.put(Event(tag=tag))
        for tag in tags:
            result = q.poll(timeout=1.0)
            assert result.tag == tag

    def test_shutdown_returns_shutdown_event(self):
        q = EventQueue()
        q.shutdown()
        result = q.poll(timeout=1.0)
        assert result.tag == EventTag.SHUTDOWN_FROM_SERVER
        assert result.completion_type == CompletionType.OP_COMPLETE

    def test_put_after_shutdown_is_ignored(self):
        q = EventQueue()
        q.shutdown()
        q.put(Event(tag=EventTag.PING))
        # poll should still return shutdown, not the put'd event
        result = q.poll(timeout=0.05)
        assert result.tag == EventTag.SHUTDOWN_FROM_SERVER

    def test_shutdown_clears_pending_events(self):
        q = EventQueue()
        q.put(Event(tag=EventTag.PING))
        q.put(Event(tag=EventTag.PONG))
        q.shutdown()
        result = q.poll(timeout=0.05)
        assert result.tag == EventTag.SHUTDOWN_FROM_SERVER

    def test_poll_blocks_until_put_from_another_thread(self):
        q = EventQueue()
        result_holder = []

        def producer():
            q.put(Event(tag=EventTag.CRM_REPLY, data=b"async"))

        t = threading.Timer(0.1, producer)
        t.start()
        result = q.poll(timeout=2.0)
        t.join()
        assert result.tag == EventTag.CRM_REPLY
        assert result.data == b"async"
        assert result.completion_type == CompletionType.OP_COMPLETE

    def test_multiple_poll_timeout(self):
        q = EventQueue()
        for _ in range(3):
            result = q.poll(timeout=0.01)
            assert result.tag == EventTag.EMPTY
            assert result.completion_type == CompletionType.OP_TIMEOUT
