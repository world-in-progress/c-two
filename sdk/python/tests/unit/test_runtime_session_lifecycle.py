"""Ordinary Host lifecycle, credential and terminal receipt projections."""
import sys

import c_two as cc
import c_two.endpoint


def test_host_started_reports_the_real_native_running_state(tmp_path) -> None:
    from c_two._native import RuntimeSession

    session = RuntimeSession(server_id='unit-host-started')
    if sys.platform != 'win32':
        session.set_local_endpoint(root=str(tmp_path))
    assert session.host_started is False
    assert session.server_credential is None
    assert session.native_lifecycle_snapshot() is None
    session.ensure_host_started()
    assert session.host_started is True
    credential = session.server_credential
    assert type(credential) is cc.endpoint.EndpointCredential  # No route was registered.
    assert credential.address == session.server_address
    snapshot = session.native_lifecycle_snapshot()
    assert set(snapshot) == {'finished', 'listener_closed', 'work_drained'}
    assert snapshot['finished'] is False
    assert snapshot['listener_closed'] is False
    # Observing the terminal state never initiates or consumes shutdown.
    assert session.native_terminal_outcome() is None
    session.shutdown(timeout_seconds=5.0)
    # A completed shutdown releases the session's host handle.
    assert session.host_started is False
    assert session.server_credential is None
    assert session.native_lifecycle_snapshot()['finished'] is True
    assert session.native_terminal_outcome() == session.shutdown(timeout_seconds=0)
