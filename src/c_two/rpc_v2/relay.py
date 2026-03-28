"""Multi-upstream HTTP relay server for C-Two.

Bridges HTTP requests to multiple IPC v3 ``ServerV2`` processes via
dynamically registered :class:`SharedClient` connections.

CRM resource processes register themselves at runtime via::

    POST /_register      {name, address}
    POST /_unregister    {name}
    GET  /_routes

Data-plane requests follow the REST convention::

    POST /{route_name}/{method_name}   ->  SharedClient.call(method, data, name=route)
    GET  /health                       ->  JSON with status and registered routes

Usage (standalone relay)::

    from c_two.rpc_v2.relay import RelayV2

    relay = RelayV2(bind='0.0.0.0:8080')
    relay.start(blocking=False)
    # CRM processes POST /_register to add routes dynamically
    relay.stop()
"""
from __future__ import annotations

import asyncio
import json
import logging
import threading
from concurrent.futures import ThreadPoolExecutor

import uvicorn
from starlette.applications import Starlette
from starlette.middleware.cors import CORSMiddleware
from starlette.requests import Request
from starlette.responses import Response
from starlette.routing import Route

from .. import error
from ..rpc.util.wait import wait
from .client import SharedClient
from ..rpc.ipc.ipc_protocol import IPCConfig

logger = logging.getLogger(__name__)

_WAIT_TIMEOUT = 5.0


# -- UpstreamPool ---------------------------------------------------------

class _UpstreamEntry:
    """A single upstream IPC connection."""
    __slots__ = ('name', 'address', 'client')

    def __init__(self, name: str, address: str, client: SharedClient):
        self.name = name
        self.address = address
        self.client = client


class UpstreamPool:
    """Thread-safe pool of upstream IPC connections, keyed by route name."""

    def __init__(self, ipc_config: IPCConfig | None = None):
        self._entries: dict[str, _UpstreamEntry] = {}
        self._lock = threading.Lock()
        self._ipc_config = ipc_config

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

            client = SharedClient(address, self._ipc_config, try_v2=True)
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

        try:
            entry.client.terminate()
        except Exception:
            logger.warning('Error terminating upstream %s', name, exc_info=True)
        logger.info('Upstream unregistered: %s', name)

    def get(self, name: str) -> SharedClient | None:
        """Look up a SharedClient by route name (lock-free read)."""
        entry = self._entries.get(name)
        return entry.client if entry else None

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
        """Terminate all upstream connections."""
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
            try:
                entry.client.terminate()
            except Exception:
                logger.warning('Error terminating upstream %s', entry.name, exc_info=True)


# -- RelayV2 --------------------------------------------------------------

class RelayV2:
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
    """

    def __init__(
        self,
        bind: str,
        *,
        max_workers: int = 32,
        ipc_config: IPCConfig | None = None,
    ):
        self._host, self._port = self._parse_bind(bind)
        self._ipc_config = ipc_config
        self._max_workers = max_workers

        self._pool = UpstreamPool(ipc_config)
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
            thread_name_prefix='c2_relay_v2',
        )
        self._app = self._create_app()

        serve_thread = threading.Thread(target=self._serve, daemon=True)
        serve_thread.start()

        wait(
            self._server_started.wait,
            self._server_started.is_set,
            _WAIT_TIMEOUT,
        )
        logger.info('RelayV2 started on %s:%d', self._host, self._port)

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
        logger.info('RelayV2 stopped.')

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
                return Response(
                    content=bytes(result) if not isinstance(result, bytes) else result,
                    media_type='application/octet-stream',
                )
            except error.CCError as exc:
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
                logger.error('RelayV2 HTTP server error: %s', exc)
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
