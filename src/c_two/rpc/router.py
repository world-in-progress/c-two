import asyncio
import json
import logging
import threading
from concurrent.futures import ThreadPoolExecutor
from dataclasses import dataclass

import uvicorn
from starlette.applications import Starlette
from starlette.middleware.cors import CORSMiddleware
from starlette.requests import Request
from starlette.responses import Response
from starlette.routing import Route

from .client import Client
from .util.wait import wait

logger = logging.getLogger(__name__)

MAXIMUM_WAIT_TIMEOUT = 0.1


@dataclass(frozen=True)
class WorkerHandle:
    namespace: str
    address: str
    version: str = ''


@dataclass
class RouterConfig:
    bind_address: str
    max_relay_workers: int = 16


class Router:
    """
    Single-entry-point HTTP gateway that routes CRM calls to registered Workers.

    Workers register with a namespace; incoming requests at ``/{namespace}``
    are forwarded to the corresponding Worker address via ``Client.relay()``.
    """

    def __init__(self, config: RouterConfig | str):
        if isinstance(config, str):
            config = RouterConfig(bind_address=config)

        self._host, self._port, self._base_path = self._parse_address(config.bind_address)
        self._registry: dict[str, WorkerHandle] = {}
        self._lock = threading.RLock()

        self._app: Starlette | None = None
        self._server: uvicorn.Server | None = None
        self._event_loop: asyncio.AbstractEventLoop | None = None
        self._relay_pool = ThreadPoolExecutor(
            max_workers=config.max_relay_workers,
            thread_name_prefix='c_two_router_relay',
        )
        self._server_started = threading.Event()
        self._shutdown_event = threading.Event()
        self._serve_done = threading.Event()
        self._serve_done.set()

    # ------------------------------------------------------------------
    # Worker registration
    # ------------------------------------------------------------------

    def register(self, namespace: str, address: str, version: str = '') -> None:
        with self._lock:
            self._registry[namespace] = WorkerHandle(
                namespace=namespace, address=address, version=version,
            )
            logger.info('Registered worker: namespace=%s address=%s', namespace, address)

    def unregister(self, namespace: str) -> None:
        with self._lock:
            removed = self._registry.pop(namespace, None)
            if removed:
                logger.info('Unregistered worker: namespace=%s', namespace)

    def attach(self, server) -> None:
        """
        Attach a co-located ``Server`` instance.  Reads the ICRM ``__tag__``
        to extract namespace and registers the Worker's bind address.
        """
        tag: str = getattr(server._state.icrm, '__tag__', '')
        if not tag:
            raise ValueError('Cannot attach server: ICRM has no __tag__ attribute.')

        parts = tag.split('/')
        namespace = parts[0] if parts else tag
        version = parts[2] if len(parts) >= 3 else ''
        address = server._state.server.bind_address
        self.register(namespace, address, version)

    @property
    def workers(self) -> dict[str, WorkerHandle]:
        with self._lock:
            return dict(self._registry)

    # ------------------------------------------------------------------
    # Lifecycle
    # ------------------------------------------------------------------

    def start(self, blocking: bool = True) -> None:
        self._app = self._create_app()

        serve_thread = threading.Thread(target=self._serve, daemon=True)
        serve_thread.start()

        wait(
            self._server_started.wait,
            self._server_started.is_set,
            MAXIMUM_WAIT_TIMEOUT,
        )
        logger.info('Router started on %s:%d%s', self._host, self._port, self._base_path)

        if blocking:
            try:
                threading.Event().wait()
            except KeyboardInterrupt:
                pass
            finally:
                self.stop()

    def stop(self) -> None:
        if self._shutdown_event.is_set():
            return
        self._shutdown_event.set()

        if self._server and self._event_loop and not self._event_loop.is_closed():
            try:
                self._event_loop.call_soon_threadsafe(self._initiate_shutdown)
            except RuntimeError:
                pass

        self._serve_done.wait(timeout=10.0)
        self._relay_pool.shutdown(wait=False)
        logger.info('Router stopped.')

    # ------------------------------------------------------------------
    # Internal HTTP server
    # ------------------------------------------------------------------

    def _create_app(self) -> Starlette:
        async def handle_route(request: Request) -> Response:
            namespace = request.path_params.get('namespace', '').strip('/')

            with self._lock:
                worker = self._registry.get(namespace)

            if worker is None:
                return Response(
                    content=f'No worker registered for namespace: {namespace}'.encode(),
                    status_code=404,
                )

            body = await request.body()
            if not body:
                return Response(content=b'Empty request body', status_code=400)

            loop = asyncio.get_running_loop()
            try:
                response_bytes = await loop.run_in_executor(
                    self._relay_pool, self._relay, worker.address, body,
                )
                return Response(content=response_bytes, media_type='application/octet-stream')
            except Exception as exc:
                logger.error('Router relay error for namespace=%s: %s', namespace, exc)
                return Response(content=str(exc).encode(), status_code=502)

        async def handle_health(request: Request) -> Response:
            with self._lock:
                namespaces = list(self._registry.keys())
            payload = json.dumps({'status': 'ok', 'workers': namespaces})
            return Response(
                content=payload.encode(),
                media_type='application/json',
            )

        routes = [
            Route('/health', handle_health, methods=['GET']),
            Route('/{namespace:path}', handle_route, methods=['POST']),
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
                loop=self._event_loop,
            )
            self._server = uvicorn.Server(config)
            self._server_started.set()
            self._event_loop.run_until_complete(self._server.serve())
        except Exception as exc:
            if not self._shutdown_event.is_set():
                logger.error('Router HTTP server error: %s', exc)
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
    # Relay
    # ------------------------------------------------------------------

    @staticmethod
    def _relay(worker_address: str, event_bytes: bytes) -> bytes:
        client = Client(worker_address)
        try:
            return client.relay(event_bytes)
        finally:
            client.terminate()

    # ------------------------------------------------------------------
    # Address parsing
    # ------------------------------------------------------------------

    @staticmethod
    def _parse_address(bind_address: str) -> tuple[str, int, str]:
        addr = bind_address
        if addr.startswith('http://'):
            addr = addr[7:]

        if '/' in addr:
            addr_part, path = addr.split('/', 1)
            path = '/' + path
        else:
            addr_part = addr
            path = '/'

        if ':' in addr_part:
            host, port_str = addr_part.rsplit(':', 1)
            port = int(port_str)
        else:
            host = addr_part
            port = 8080

        return host, port, path
