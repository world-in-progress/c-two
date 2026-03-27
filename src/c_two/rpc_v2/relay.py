"""Minimal HTTP-to-IPC v3 relay server (Python, stand-in for Rust relay).

Bridges HTTP requests to a :class:`ServerV2` via :class:`SharedClient`.
Uses the v2 REST API convention::

    POST /{route_name}/{method_name}   →  SharedClient.call(method, data, name=route)
    GET  /health                       →  JSON with status and routes

This relay serves as:

1. A functional HTTP gateway for development and testing.
2. A reference implementation for the future Rust ``c2-relay`` binary.

Usage::

    from c_two.rpc_v2.relay import RelayV2

    relay = RelayV2(bind='0.0.0.0:8080', upstream='ipc-v3://my_server')
    relay.start(blocking=False)
    # ...
    relay.stop()

Integrated with :func:`cc.relay.start`::

    relay = cc.relay.start(bind='0.0.0.0:8080', upstream=cc.server_address())
    # ...
    cc.relay.stop(relay)
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


class RelayV2:
    """HTTP relay server that forwards requests to an IPC v3 ServerV2.

    Parameters
    ----------
    bind:
        HTTP listen address (e.g. ``'0.0.0.0:8080'`` or
        ``'http://localhost:8080'``).
    upstream:
        IPC v3 server address (e.g. ``'ipc-v3://my_server'``).
    max_workers:
        Thread pool size for relay calls.
    ipc_config:
        Optional IPC config for the SharedClient.
    """

    def __init__(
        self,
        bind: str,
        upstream: str,
        *,
        max_workers: int = 32,
        ipc_config: IPCConfig | None = None,
    ):
        self._host, self._port = self._parse_bind(bind)
        self._upstream = upstream
        self._ipc_config = ipc_config
        self._max_workers = max_workers

        self._client: SharedClient | None = None
        self._app: Starlette | None = None
        self._server: uvicorn.Server | None = None
        self._event_loop: asyncio.AbstractEventLoop | None = None
        self._relay_pool: ThreadPoolExecutor | None = None

        self._server_started = threading.Event()
        self._shutdown_event = threading.Event()
        self._serve_done = threading.Event()
        self._serve_done.set()

    # ------------------------------------------------------------------
    # Lifecycle
    # ------------------------------------------------------------------

    def start(self, blocking: bool = False) -> None:
        """Start the relay server.

        Parameters
        ----------
        blocking:
            If ``True``, blocks the calling thread until interrupted.
        """
        # Connect to upstream ServerV2.
        self._client = SharedClient(self._upstream, self._ipc_config, try_v2=True)
        self._client.connect()

        self._relay_pool = ThreadPoolExecutor(
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
        logger.info(
            'RelayV2 started on %s:%d → %s',
            self._host, self._port, self._upstream,
        )

        if blocking:
            try:
                threading.Event().wait()
            except KeyboardInterrupt:
                pass
            finally:
                self.stop()

    def stop(self) -> None:
        """Stop the relay server and disconnect from upstream."""
        if self._shutdown_event.is_set():
            return
        self._shutdown_event.set()

        if self._server and self._event_loop and not self._event_loop.is_closed():
            try:
                self._event_loop.call_soon_threadsafe(self._initiate_shutdown)
            except RuntimeError:
                pass

        self._serve_done.wait(timeout=10.0)

        if self._relay_pool is not None:
            self._relay_pool.shutdown(wait=False)
            self._relay_pool = None

        if self._client is not None:
            try:
                self._client.terminate()
            except Exception:
                logger.warning('Failed to terminate relay upstream client', exc_info=True)
            self._client = None

        logger.info('RelayV2 stopped.')

    @property
    def url(self) -> str:
        """HTTP base URL of the relay.

        Uses ``127.0.0.1`` when bound to ``0.0.0.0`` so that local
        clients can actually connect.
        """
        host = '127.0.0.1' if self._host in ('0.0.0.0', '::') else self._host
        return f'http://{host}:{self._port}'

    # ------------------------------------------------------------------
    # Starlette app
    # ------------------------------------------------------------------

    def _create_app(self) -> Starlette:
        relay_ref = self  # closure capture

        async def handle_call(request: Request) -> Response:
            """POST /{route_name}/{method_name} → SharedClient.call()."""
            route_name = request.path_params.get('route_name', '')
            method_name = request.path_params.get('method_name', '')

            if not route_name or not method_name:
                return Response(
                    content=b'Missing route_name or method_name',
                    status_code=400,
                )

            body = await request.body()

            loop = asyncio.get_running_loop()
            try:
                result = await loop.run_in_executor(
                    relay_ref._relay_pool,
                    relay_ref._do_call,
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
            """GET /health → JSON status."""
            payload = json.dumps({
                'status': 'ok',
                'upstream': relay_ref._upstream,
            })
            return Response(content=payload.encode(), media_type='application/json')

        routes = [
            Route('/health', handle_health, methods=['GET']),
            Route('/{route_name}/{method_name}', handle_call, methods=['POST']),
        ]

        app = Starlette(routes=routes)
        app.add_middleware(
            CORSMiddleware,
            allow_origins=['*'],
            allow_credentials=True,
            allow_methods=['*'],
            allow_headers=['*'],
        )
        return app

    def _do_call(
        self,
        route_name: str,
        method_name: str,
        data: bytes,
    ) -> bytes:
        """Execute a CRM call via SharedClient (runs in thread pool)."""
        return self._client.call(method_name, data or None, name=route_name)

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

            # Monkey-patch startup_complete to signal readiness.
            _original_startup = self._server.startup

            async def _patched_startup(sockets=None):
                await _original_startup(sockets)
                self._server_started.set()

            self._server.startup = _patched_startup

            self._event_loop.run_until_complete(self._server.serve())
        except Exception as exc:
            if not self._shutdown_event.is_set():
                logger.error('RelayV2 HTTP server error: %s', exc)
            self._server_started.set()  # unblock waiters on error
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
        # Strip trailing path if any.
        if '/' in addr:
            addr = addr.split('/', 1)[0]
        if ':' in addr:
            host, port_str = addr.rsplit(':', 1)
            return host, int(port_str)
        return addr, 8080
