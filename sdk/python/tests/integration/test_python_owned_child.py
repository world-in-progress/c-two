"""Python controller, opaque stdio transfer and real child lifecycle."""
from __future__ import annotations

import sys
import time

import c_two as cc


def test_python_controller_policy_configuration_then_owner_eof_reaps_child(tmp_path):
    ready = tmp_path / 'ready'
    finished = tmp_path / 'finished'
    script = '''
import c_two as cc
from pathlib import Path
import sys
@cc.crm(namespace='test.python.controller', version='0.1.0')
class Echo:
    def ping(self) -> str: ...
    @cc.on_shutdown
    def done(self) -> None: ...
class Resource:
    def ping(self): return 'pong'
    def done(self): Path(sys.argv[2]).write_text('one hook')
receiver = cc.adopt_owner_stdin()
cc.set_server(lifecycle=cc.LifecycleConfig.owner_bound(0), owner_control=receiver)
cc.set_transport_policy(shm_threshold=8192)
cc.register(Echo, Resource(), name='child-echo')
Path(sys.argv[1]).write_text(cc.server_address())
cc.serve()
'''
    keepalive, receiver = cc.owner_control_pair()
    child = cc.spawn_owned_child(receiver, sys.executable, ['-c', script, str(ready), str(finished)])
    try:
        assert not receiver.is_available
        deadline = time.monotonic() + 10
        while not ready.exists():
            assert child.poll() is None, 'child exited before readiness'
            assert time.monotonic() < deadline, 'child never became ready'
            time.sleep(.01)
        assert ready.read_text().startswith('ipc://')
        keepalive.shutdown()
        assert child.wait(timeout=10) == 0
        assert finished.read_text() == 'one hook'
        assert child.poll() == 0
    finally:
        keepalive.shutdown()
        if child.poll() is None:
            child.kill()
        child.wait(timeout=10)
        child.close()
