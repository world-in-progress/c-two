import time

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


def test_native_buffer_local_copy_tracker_sweeps_retained_snapshots():
    tracker = _native.LocalCopyTracker()
    local_copy = tracker.track_retained(
        route_name="grid",
        method_name="subdivide_grids",
        direction="client_response",
        bytes=8192,
    )
    time.sleep(0.01)
    stale = tracker.sweep_retained(0.001)
    assert len(stale) == 1
    assert set(stale[0]) == {
        "id", "route_name", "method_name", "direction", "bytes", "age_seconds"
    }
    assert stale[0]["route_name"] == "grid"
    assert stale[0]["method_name"] == "subdivide_grids"
    assert stale[0]["direction"] == "client_response"
    assert stale[0]["bytes"] == 8192
    assert stale[0]["age_seconds"] > 0
    local_copy.release()


def test_native_buffer_local_copy_tracker_rejects_invalid_labels():
    tracker = _native.LocalCopyTracker()
    import pytest
    with pytest.raises(ValueError, match="invalid local copy direction"):
        tracker.track_retained(
            route_name="grid", method_name="subdivide_grids", direction="bad", bytes=1,
        )
