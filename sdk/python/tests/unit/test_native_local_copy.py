from c_two import _native


def test_native_buffer_local_copy_tracker_counts_local_bytes():
    tracker = _native.LocalCopyTracker()
    local_copy = tracker.track_retained(
        route_name="grid",
        method_name="subdivide_grids",
        direction="client_response",
        bytes=128,
    )
    stats = tracker.stats()
    assert set(stats) == {
        "active_holds", "total_held_bytes", "oldest_hold_seconds", "by_direction"
    }
    assert stats["active_holds"] == 1
    assert stats["total_held_bytes"] == 128
    assert set(stats["by_direction"]["client_response"]) == {"active_holds", "total_held_bytes"}
    assert stats["by_direction"]["client_response"]["active_holds"] == 1
    local_copy.release()
    assert tracker.stats()["active_holds"] == 0


def test_native_buffer_local_copy_tracker_rejects_invalid_labels():
    tracker = _native.LocalCopyTracker()
    import pytest
    with pytest.raises(ValueError, match="invalid local copy direction"):
        tracker.track_retained(
            route_name="grid", method_name="subdivide_grids", direction="bad", bytes=1,
        )
