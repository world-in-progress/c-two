"""Unit tests for heartbeat / ping-pong frame encoding."""
from __future__ import annotations

import pytest

from c_two.transport.ipc.frame import FRAME_STRUCT, encode_frame
from c_two.transport.ipc.msg_type import MsgType, PING_BYTES
from c_two.transport.protocol import FLAG_SIGNAL


# ---------------------------------------------------------------------------
# Client PONG auto-response (protocol-level check)
# ---------------------------------------------------------------------------


class TestClientPongResponse:
    """Verify PING frame encoding for server→client direction."""

    def test_ping_frame_is_valid_signal(self):
        """PING frame has FLAG_SIGNAL and 1-byte MsgType.PING payload."""
        ping_frame = encode_frame(42, FLAG_SIGNAL, PING_BYTES)
        header = ping_frame[:16]
        total_len, request_id, flags = FRAME_STRUCT.unpack(header)
        payload = ping_frame[16:]

        assert flags & FLAG_SIGNAL
        assert len(payload) == 1
        assert payload[0] == MsgType.PING
        assert request_id == 42
