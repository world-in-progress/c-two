"""Multi-upstream HTTP relay server for C-Two.

Bridges HTTP requests to multiple IPC ``Server`` processes via
dynamically registered :class:`SharedClient` connections.

CRM resource processes register themselves at runtime via::

    POST /_register      {name, address}
    POST /_unregister    {name}
    GET  /_routes

Data-plane requests follow the REST convention::

    POST /{route_name}/{method_name}   ->  SharedClient.call(method, data, name=route)
    GET  /health                       ->  JSON with status and registered routes

Usage (standalone relay)::

    from c_two.transport.relay.core import Relay

    relay = Relay(bind='0.0.0.0:8080')
    relay.start(blocking=False)
    # CRM processes POST /_register to add routes dynamically
    relay.stop()
"""
from __future__ import annotations

import asyncio
import json
import logging
import threading
import time as _time
from concurrent.futures import ThreadPoolExecutor

import uvicorn
from starlette.applications import Starlette
from starlette.middleware.cors import CORSMiddleware
from starlette.requests import Request
from starlette.responses import Response
from starlette.routing import Route

from ... import error
from ..client.core import SharedClient
from ..ipc.frame import IPCConfig

logger = logging.getLogger(__name__)

_WAIT_TIMEOUT = 5.0
_MAX_WAIT_TIMEOUT = 0.1


def _wait(wait_fn, wait_complete_fn, timeout, spin_cb=None):
    """Spin-wait with timeout."""
    if timeout is None:
        while not wait_complete_fn():
            wait_fn(timeout=_MAX_WAIT_TIMEOUT)
            if spin_cb is not None:
                spin_cb()
        return True
    end = _time.time() + timeout
    while not wait_complete_fn():
        remaining = min(end - _time.time(), _MAX_WAIT_TIMEOUT)
        if remaining < 0:
            return True
        wait_fn(timeout=remaining)
        if spin_cb is not None:
            spin_cb()
    return False


# -- UpstreamPool ---------------------------------------------------------

class _UpstreamEntry:
    """A single upstream IPC connection."""
    __slots__ = ('name', 'address', 'client', 'last_activity')

    def __init__(self, name: str, address: str, client: SharedClient | None):
        self.name = name
        self.address = address
        self.client = client
        self.last_activity = _time.monotonic()


class UpstreamPool:
    """Thread-safe pool of upstream IPC connections, keyed by route name."""

    def __init__(self, ipc_config: IPCConfig | None = None, idle_timeout: float = 300.0):
        self._entries: dict[str, _UpstreamEntry] = {}
        self._lock = threading.Lock()
        self._ipc_config = ipc_config
        self._idle_timeout = idle_timeout
        self._sweeper_timer: threading.Timer | None = None
        if idle_timeout > 0:
            self._start_sweeper()

    def add(self, name: str, address: str) -> None:
        """Register a new upstream. Connects a SharedClient immediately.

        Raises
        ------
        ValueError
            If *name* is already registered.
        ConnectionError
            If the IPC connection fails.
        """
        with self._lock:
            if name in self._entries:
                raise ValueError(f'Route name already registered: {name!r}')

            client = SharedClient(address, self._ipc_config)
            try:
                client.connect()
            except Exception as exc:
                raise ConnectionError(
                    f'Failed to connect upstream {name!r} at {address}: {exc}',
                ) from exc

            self._entries[name] = _UpstreamEntry(name, address, client)
            logger.info('Upstream registered: %s -> %s', name, address)

    def remove(self, name: str) -> None:
        """Unregister an upstream. Terminates the SharedClient.

        Raises
        ------
        KeyError
            If *name* is not registered.
        """
        with self._lock:
            entry = self._entries.pop(name, None)
            if entry is None:
                raise KeyError(f'Route name not registered: {name!r}')

        if entry.client is not None:
            try:
                entry.client.terminate()
            except Exception:
                logger.warning('Error terminating upstream %s', name, exc_info=True)
        logger.info('Upstream unregistered: %s', name)

    def get(self, name: str) -> SharedClient | None:
        """Look up a SharedClient by route name.

        If the client was evicted by the idle sweeper, attempts a lazy
        reconnect before returning ``None``.
        """
        entry = self._entries.get(name)
        if entry is None:
            return None
        if entry.client is not None:
            return entry.client
        # Client was evicted — try lazy reconnect
        return self._try_reconnect(entry)

    def touch(self, name: str) -> None:
        """Update last-activity timestamp for *name*."""
        entry = self._entries.get(name)
        if entry is not None:
            entry.last_activity = _time.monotonic()

    def _try_reconnect(self, entry: _UpstreamEntry) -> SharedClient | None:
        with self._lock:
            # Re-check under lock (another thread may have reconnected)
            if entry.client is not None:
                return entry.client
            try:
                client = SharedClient(entry.address, self._ipc_config)
                client.connect()
                entry.client = client
                entry.last_activity = _time.monotonic()
                logger.info('Reconnected upstream %s at %s', entry.name, entry.address)
                return client
            except Exception as exc:
                logger.warning('Failed to reconnect upstream %s: %s', entry.name, exc)
                return None

    def list_routes(self) -> list[dict[str, str]]:
        """Return a list of registered routes."""
        with self._lock:
            return [
                {'name': e.name, 'address': e.address}
                for e in self._entries.values()
            ]

    def has(self, name: str) -> bool:
        return name in self._entries

    def shutdown(self) -> None:
        """Terminate all upstream connections and stop the sweeper."""
        if self._sweeper_timer is not None:
            self._sweeper_timer.cancel()
            self._sweeper_timer = None
        with self._lock:
            entries = list(self._entries.values())
            self._entries.clear()
        if entries:
            names = ', '.join(e.name for e in entries)
            logger.info(
                'Relay shutting down — disconnecting %d upstream(s): %s. '
                'CRM processes should handle relay absence gracefully.',
                len(entries), names,
            )
        for entry in entries:
            if entry.client is None:
                continue
            try:
                entry.client.terminate()
            except Exception:
                logger.warning('Error terminating upstream %s', entry.name, exc_info=True)

    # -- Sweeper -----------------------------------------------------------

    def _start_sweeper(self) -> None:
        interval = max(self._idle_timeout / 10, 5.0)
        self._sweeper_timer = threading.Timer(interval, self._sweep_idle)
        self._sweeper_timer.daemon = True
        self._sweeper_timer.start()

    def _sweep_idle(self) -> None:
        now = _time.monotonic()
        to_terminate: list[SharedClient] = []
        with self._lock:
            for entry in self._entries.values():
                if entry.client is not None and (now - entry.last_activity) >= self._idle_timeout:
                    to_terminate.append(entry.client)
                    entry.client = None
                    logger.info('Evicted idle upstream %s (idle %.0fs)', entry.name, now - entry.last_activity)
        # Terminate outside lock
        for client in to_terminate:
            try:
                client.terminate()
            except Exception:
                pass
        # Reschedule if pool is still alive
        if self._idle_timeout > 0 and self._sweeper_timer is not None:
            self._start_sweeper()


# -- Relay --------------------------------------------------------------

class Relay:
    """Multi-upstream HTTP relay server.

    Starts as an empty relay with no upstream connections. CRM resource
    processes register dynamically via ``POST /_register``.

    Parameters
    ----------
    bind:
        HTTP listen address (e.g. ``'0.0.0.0:8080'``).
    max_workers:
        Thread pool size for relay calls.
    ipc_config:
        Optional IPC config for upstream SharedClients.
    idle_timeout:
        Seconds of inactivity before an upstream connection is evicted.
        Set to ``0`` to disable idle sweeping.
    """

    def __init__(
        self,
        bind: str,
        *,
        max_workers: int = 32,
        ipc_config: IPCConfig | None = None,
        idle_timeout: float = 300.0,
    ):
        self._host, self._port = self._parse_bind(bind)
        self._ipc_config = ipc_config
        self._max_workers = max_workers

        self._pool = UpstreamPool(ipc_config, idle_timeout=idle_timeout)
        self._app: Starlette | None = None
        self._server: uvicorn.Server | None = None
        self._event_loop: asyncio.AbstractEventLoop | None = None
        self._relay_thread_pool: ThreadPoolExecutor | None = None

        self._server_started = threading.Event()
        self._shutdown_event = threading.Event()
        self._serve_done = threading.Event()
        self._serve_done.set()

    # ------------------------------------------------------------------
    # Lifecycle
    # ------------------------------------------------------------------

    def start(self, blocking: bool = False) -> None:
        """Start the relay HTTP server."""
        self._relay_thread_pool = ThreadPoolExecutor(
            max_workers=self._max_workers,
            thread_name_prefix='c2_relay',
        )
        self._app = self._create_app()

        serve_thread = threading.Thread(target=self._serve, daemon=True)
        serve_thread.start()

        _wait(
            self._server_started.wait,
            self._server_started.is_set,
            _WAIT_TIMEOUT,
        )
        logger.info('Relay started on %s:%d', self._host, self._port)

        if blocking:
            try:
                threading.Event().wait()
            except KeyboardInterrupt:
                pass
            finally:
                self.stop()

    def stop(self) -> None:
        """Stop the relay and disconnect all upstreams."""
        if self._shutdown_event.is_set():
            return
        self._shutdown_event.set()

        if self._server and self._event_loop and not self._event_loop.is_closed():
            try:
                self._event_loop.call_soon_threadsafe(self._initiate_shutdown)
            except RuntimeError:
                pass

        self._serve_done.wait(timeout=10.0)

        if self._relay_thread_pool is not None:
            self._relay_thread_pool.shutdown(wait=False)
            self._relay_thread_pool = None

        self._pool.shutdown()
        logger.info('Relay stopped.')

    @property
    def url(self) -> str:
        """HTTP base URL of the relay."""
        host = '127.0.0.1' if self._host in ('0.0.0.0', '::') else self._host
        return f'http://{host}:{self._port}'

    @property
    def upstream_pool(self) -> UpstreamPool:
        """Access the upstream pool (for programmatic registration)."""
        return self._pool

    # ------------------------------------------------------------------
    # Starlette app
    # ------------------------------------------------------------------

    def _create_app(self) -> Starlette:
        relay = self

        # -- Control-plane endpoints -----------------------------------

        async def handle_register(request: Request) -> Response:
            """POST /_register -- register a new CRM upstream."""
            try:
                body = await request.json()
            except Exception:
                return Response(content=b'Invalid JSON', status_code=400)

            name = body.get('name')
            address = body.get('address')
            if not name or not address:
                return Response(
                    content=b'Missing "name" or "address"',
                    status_code=400,
                )

            try:
                relay._pool.add(name, address)
            except ValueError as exc:
                return Response(
                    content=json.dumps({'error': str(exc)}).encode(),
                    status_code=409,
                    media_type='application/json',
                )
            except ConnectionError as exc:
                return Response(
                    content=json.dumps({'error': str(exc)}).encode(),
                    status_code=502,
                    media_type='application/json',
                )

            return Response(
                content=json.dumps({'registered': name}).encode(),
                status_code=201,
                media_type='application/json',
            )

        async def handle_unregister(request: Request) -> Response:
            """POST /_unregister -- remove a CRM upstream."""
            try:
                body = await request.json()
            except Exception:
                return Response(content=b'Invalid JSON', status_code=400)

            name = body.get('name')
            if not name:
                return Response(content=b'Missing "name"', status_code=400)

            try:
                relay._pool.remove(name)
            except KeyError as exc:
                return Response(
                    content=json.dumps({'error': str(exc)}).encode(),
                    status_code=404,
                    media_type='application/json',
                )

            return Response(
                content=json.dumps({'unregistered': name}).encode(),
                status_code=200,
                media_type='application/json',
            )

        async def handle_routes(request: Request) -> Response:
            """GET /_routes -- list all registered routes."""
            routes_data = relay._pool.list_routes()
            return Response(
                content=json.dumps({'routes': routes_data}).encode(),
                media_type='application/json',
            )

        # -- Data-plane endpoints --------------------------------------

        async def handle_call(request: Request) -> Response:
            """POST /{route_name}/{method_name} -> SharedClient.call()."""
            route_name = request.path_params.get('route_name', '')
            method_name = request.path_params.get('method_name', '')

            if not route_name or not method_name:
                return Response(content=b'Missing route_name or method_name', status_code=400)

            client = relay._pool.get(route_name)
            if client is None:
                if relay._pool.has(route_name):
                    return Response(
                        content=json.dumps({
                            'error': f'Upstream {route_name!r} is registered but unreachable',
                        }).encode(),
                        status_code=502,
                        media_type='application/json',
                    )
                return Response(
                    content=json.dumps({
                        'error': f'No upstream registered for route: {route_name!r}',
                    }).encode(),
                    status_code=404,
                    media_type='application/json',
                )

            body = await request.body()

            loop = asyncio.get_running_loop()
            try:
                result = await loop.run_in_executor(
                    relay._relay_thread_pool,
                    relay._do_call,
                    client,
                    route_name,
                    method_name,
                    body,
                )
                relay._pool.touch(route_name)
                return Response(
                    content=bytes(result) if not isinstance(result, bytes) else result,
                    media_type='application/octet-stream',
                )
            except error.CCError as exc:
                relay._pool.touch(route_name)
                return Response(
                    content=error.CCError.serialize(exc),
                    status_code=500,
                    media_type='application/octet-stream',
                )
            except error.CCBaseError as exc:
                return Response(
                    content=str(exc).encode('utf-8'),
                    status_code=502,
                )
            except Exception as exc:
                logger.error('Relay error: %s/%s: %s', route_name, method_name, exc)
                return Response(
                    content=str(exc).encode('utf-8'),
                    status_code=502,
                )

        async def handle_health(request: Request) -> Response:
            """GET /health -> JSON status."""
            routes_data = relay._pool.list_routes()
            payload = json.dumps({
                'status': 'ok',
                'routes': [r['name'] for r in routes_data],
            })
            return Response(content=payload.encode(), media_type='application/json')

        app_routes = [
            Route('/_register', handle_register, methods=['POST']),
            Route('/_unregister', handle_unregister, methods=['POST']),
            Route('/_routes', handle_routes, methods=['GET']),
            Route('/health', handle_health, methods=['GET']),
            Route('/{route_name}/{method_name}', handle_call, methods=['POST']),
        ]

        app = Starlette(routes=app_routes)
        app.add_middleware(
            CORSMiddleware,
            allow_origins=['*'],
            allow_credentials=True,
            allow_methods=['*'],
            allow_headers=['*'],
        )
        return app

    @staticmethod
    def _do_call(
        client: SharedClient,
        route_name: str,
        method_name: str,
        data: bytes,
    ) -> bytes:
        """Execute a CRM call via a specific SharedClient (runs in thread pool)."""
        return client.call(method_name, data or None, name=route_name)

    # ------------------------------------------------------------------
    # Internal HTTP server
    # ------------------------------------------------------------------

    def _serve(self) -> None:
        self._serve_done.clear()
        self._event_loop = asyncio.new_event_loop()
        asyncio.set_event_loop(self._event_loop)

        try:
            config = uvicorn.Config(
                app=self._app,
                host=self._host,
                port=self._port,
                log_level='error',
            )
            self._server = uvicorn.Server(config)

            _original_startup = self._server.startup

            async def _patched_startup(sockets=None):
                await _original_startup(sockets)
                self._server_started.set()

            self._server.startup = _patched_startup

            self._event_loop.run_until_complete(self._server.serve())
        except Exception as exc:
            if not self._shutdown_event.is_set():
                logger.error('Relay HTTP server error: %s', exc)
            self._server_started.set()
        finally:
            if not self._event_loop.is_closed():
                try:
                    pending = asyncio.all_tasks(self._event_loop)
                    if pending:
                        self._event_loop.run_until_complete(
                            asyncio.gather(*pending, return_exceptions=True)
                        )
                    self._event_loop.close()
                except Exception:
                    pass
            self._serve_done.set()

    def _initiate_shutdown(self) -> None:
        if self._server and not self._server.should_exit:
            self._server.should_exit = True
            if hasattr(self._server, 'servers') and self._server.servers:
                for srv in self._server.servers:
                    srv.close()

    # ------------------------------------------------------------------
    # Address parsing
    # ------------------------------------------------------------------

    @staticmethod
    def _parse_bind(bind: str) -> tuple[str, int]:
        addr = bind
        if addr.startswith('http://'):
            addr = addr[7:]
        if '/' in addr:
            addr = addr.split('/', 1)[0]
        if ':' in addr:
            host, port_str = addr.rsplit(':', 1)
            return host, int(port_str)
        return addr, 8080
