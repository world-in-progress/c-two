import time

from c_two import _native


def test_native_buffer_lease_tracker_counts_inline_retained():
    tracker = _native.BufferLeaseTracker()
    lease = tracker.track_retained(
        route_name="grid",
        method_name="subdivide_grids",
        direction="client_response",
        storage="inline",
        bytes=128,
    )
    stats = tracker.stats()
    assert stats["active_holds"] == 1
    assert stats["total_held_bytes"] == 128
    assert stats["by_storage"]["inline"]["active_holds"] == 1
    lease.release()
    assert tracker.stats()["active_holds"] == 0


def test_native_buffer_lease_tracker_sweeps_retained_snapshots():
    tracker = _native.BufferLeaseTracker()
    lease = tracker.track_retained(
        route_name="grid",
        method_name="subdivide_grids",
        direction="client_response",
        storage="shm",
        bytes=8192,
    )
    time.sleep(0.01)
    stale = tracker.sweep_retained(0.001)
    assert len(stale) == 1
    assert stale[0]["route_name"] == "grid"
    assert stale[0]["method_name"] == "subdivide_grids"
    assert stale[0]["direction"] == "client_response"
    assert stale[0]["storage"] == "shm"
    assert stale[0]["bytes"] == 8192
    assert stale[0]["age_seconds"] > 0
    lease.release()


def test_native_buffer_lease_tracker_rejects_invalid_labels():
    tracker = _native.BufferLeaseTracker()
    for kwargs in [
        {"direction": "bad", "storage": "inline"},
        {"direction": "client_response", "storage": "bad"},
    ]:
        try:
            tracker.track_retained(
                route_name="grid",
                method_name="subdivide_grids",
                bytes=1,
                **kwargs,
            )
        except ValueError:
            pass
        else:
            raise AssertionError(f"expected ValueError for {kwargs}")
