"""Test-only TCP link gate and real c3 process for mesh partitions."""

from __future__ import annotations

import os
from pathlib import Path
import select
import socket
import socketserver
import subprocess
import threading
import time

import httpx

from tests.conftest import c3_binary, free_tcp_port, wait_for_relay


class LinkGate:
    """Forward a relay's advertised URL while retaining its process state."""

    def __init__(self, target_port: int):
        self.target_port = target_port
        self.enabled = threading.Event()
        self.enabled.set()
        self.lock = threading.Lock()
        self.connections: set[socket.socket] = set()
        gate = self

        class Handler(socketserver.BaseRequestHandler):
            def handle(self):
                if not gate.enabled.is_set():
                    return
                try:
                    upstream = socket.create_connection(("127.0.0.1", gate.target_port), timeout=2)
                except OSError:
                    return
                with upstream:
                    with gate.lock:
                        gate.connections.update((self.request, upstream))
                    try:
                        self.request.setblocking(False)
                        upstream.setblocking(False)
                        while gate.enabled.is_set():
                            readable, _, _ = select.select((self.request, upstream), (), (), 0.1)
                            for source in readable:
                                target = upstream if source is self.request else self.request
                                try:
                                    payload = source.recv(65536)
                                    if not payload:
                                        return
                                    target.sendall(payload)
                                except OSError:
                                    return
                    finally:
                        with gate.lock:
                            gate.connections.difference_update((self.request, upstream))

        class Server(socketserver.ThreadingTCPServer):
            allow_reuse_address = True
            daemon_threads = True

        self.server = Server(("127.0.0.1", 0), Handler)
        self.url = f"http://127.0.0.1:{self.server.server_address[1]}"
        self.thread = threading.Thread(target=self.server.serve_forever, daemon=True)
        self.thread.start()

    def block(self) -> None:
        self.enabled.clear()
        with self.lock:
            for connection in tuple(self.connections):
                try:
                    connection.shutdown(socket.SHUT_RDWR)
                except OSError:
                    pass

    def restore(self) -> None:
        self.enabled.set()

    def close(self) -> None:
        self.block()
        self.server.shutdown()
        self.server.server_close()
        self.thread.join(timeout=5)
        assert not self.thread.is_alive()


class GatedRelay:
    def __init__(self, relay_id: str, log_dir: Path, seeds: list[str] = ()):
        self.port = free_tcp_port()
        self.url = f"http://127.0.0.1:{self.port}"
        self.gate = LinkGate(self.port)
        self.relay_id = relay_id
        self.stdout_path = log_dir / f"{relay_id}.stdout"
        self.stderr_path = log_dir / f"{relay_id}.stderr"
        self.stdout = self.stdout_path.open("w+", encoding="utf-8")
        self.stderr = self.stderr_path.open("w+", encoding="utf-8")
        args = [str(c3_binary()), "relay", "--bind", f"127.0.0.1:{self.port}",
                "--advertise-url", self.gate.url, "--relay-id", relay_id]
        if seeds:
            args.extend(("--seeds", ",".join(seeds)))
        self.args = args
        self._start()

    def _start(self) -> None:
        env = {**os.environ, "C2_ENV_FILE": "", "NO_PROXY": "127.0.0.1,localhost",
               "no_proxy": "127.0.0.1,localhost"}
        self.process = subprocess.Popen(self.args, stdout=self.stdout, stderr=self.stderr,
                                        text=True, env=env)
        try:
            wait_for_relay(self.url, timeout=10, proc=self.process)
        except BaseException:
            self.close()
            raise

    def restart(self) -> None:
        assert self.process.poll() is not None
        self._start()

    def log(self) -> str:
        return self.stderr_path.read_text(encoding="utf-8")

    def close(self) -> None:
        try:
            if self.process.poll() is None:
                self.process.terminate()
                try:
                    self.process.wait(timeout=10)
                except subprocess.TimeoutExpired:
                    self.process.kill()
                    self.process.wait(timeout=5)
        finally:
            self.gate.close()
            self.stdout.close()
            self.stderr.close()


def wait_state(observe, predicate, *, timeout: float = 35):
    deadline = time.monotonic() + timeout
    last = None
    while time.monotonic() < deadline:
        try:
            last = observe()
            if predicate(last):
                return last
        except (httpx.TransportError, httpx.HTTPStatusError) as error:
            last = repr(error)
        threading.Event().wait(0.1)
    raise AssertionError(f"state did not converge: {last!r}")
