import struct
import pytest
from c_two.rpc.util.encoding import add_length_prefix, parse_message


class TestAddLengthPrefix:
    def test_prepends_8_byte_big_endian_prefix(self):
        data = b"hello"
        result = add_length_prefix(data)
        prefix = result[:8]
        assert struct.unpack(">Q", prefix)[0] == len(data)
        assert result[8:] == data

    def test_output_length(self):
        data = b"abc"
        result = add_length_prefix(data)
        assert len(result) == 8 + len(data)

    def test_empty_input(self):
        result = add_length_prefix(b"")
        assert len(result) == 8
        assert struct.unpack(">Q", result[:8])[0] == 0

    def test_large_payload(self):
        data = b"\xab" * 100_000
        result = add_length_prefix(data)
        assert len(result) == 8 + 100_000
        assert struct.unpack(">Q", result[:8])[0] == 100_000
        assert result[8:] == data


class TestParseMessage:
    def test_single_message(self):
        data = b"hello"
        prefixed = add_length_prefix(data)
        messages = parse_message(prefixed)
        assert len(messages) == 1
        assert bytes(messages[0]) == data

    def test_multiple_chained_messages(self):
        msg1, msg2 = b"first", b"second"
        chained = add_length_prefix(msg1) + add_length_prefix(msg2)
        messages = parse_message(chained)
        assert len(messages) == 2
        assert bytes(messages[0]) == msg1
        assert bytes(messages[1]) == msg2

    def test_returns_memoryview_objects(self):
        messages = parse_message(add_length_prefix(b"x"))
        assert all(isinstance(m, memoryview) for m in messages)

    def test_empty_input(self):
        messages = parse_message(b"")
        assert messages == []

    def test_incomplete_length_prefix_raises(self):
        with pytest.raises(ValueError, match="Incomplete length prefix"):
            parse_message(b"\x00\x00\x00")

    def test_truncated_data_raises(self):
        # Prefix says 100 bytes but only 2 follow
        bad = struct.pack(">Q", 100) + b"\x00\x00"
        with pytest.raises(ValueError, match="exceeds remaining buffer size"):
            parse_message(bad)


class TestRoundTrip:
    def test_single_round_trip(self):
        data = b"round trip test \xff\x00"
        messages = parse_message(add_length_prefix(data))
        assert bytes(messages[0]) == data

    def test_multiple_messages_round_trip(self):
        originals = [b"alpha", b"", b"gamma\x00\xff", b"d" * 10_000]
        chained = b"".join(add_length_prefix(m) for m in originals)
        messages = parse_message(chained)
        assert len(messages) == len(originals)
        for orig, parsed in zip(originals, messages):
            assert bytes(parsed) == orig
