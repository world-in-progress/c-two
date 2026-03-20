import asyncio
import logging
import uvicorn
import threading
from starlette.routing import Route
from starlette.requests import Request
from starlette.responses import Response
from starlette.applications import Starlette
from starlette.middleware.cors import CORSMiddleware

from ..util.wait import wait
from ..base import BaseServer
from ..event import Event, EventTag, EventQueue

logger = logging.getLogger(__name__)

MAXIMUM_WAIT_TIMEOUT = 0.1

class HttpServer(BaseServer):
    def __init__(self, bind_address: str, event_queue: EventQueue | None = None):
        super().__init__(bind_address, event_queue)
        
        self.host, self.port, self.path = self._parse_bind_address(bind_address)
        
        self.app = None
        self.server = None
        self.event_loop = None
        self._server_started = threading.Event()
        self._shutdown_event = threading.Event()
        # Owned exclusively by the event loop thread after start()
        self._response_futures: dict[str, asyncio.Future] = {}
        
    def _parse_bind_address(self, bind_address: str) -> tuple[str, int, str]:
        if bind_address.startswith('http://'):
            address = bind_address[7:]
        else:
            address = bind_address
        
        if '/' in address:
            address_part, path = address.split('/', 1)
            path = '/' + path
        else:
            address_part = address
            path = '/'
            
        if ':' in address_part:
            host, port_str = address_part.rsplit(':', 1)
            port = int(port_str)
        else:
            host = address_part
            port = 8000
            
        return host, port, path
    
    def _create_app(self) -> Starlette:
        async def handle_request(request: Request) -> Response:
            try:
                if self._shutdown_event.is_set():
                    return Response(b'Server is shutting down', status_code=503)
                
                if '?' in self.path:
                    expected_query = self.path.split('?', 1)[1]
                    actual_query = str(request.url.query)
                    if actual_query != expected_query:
                        return Response(
                            f'Query parameters mismatch. Expected: {expected_query}, Got: {actual_query}'.encode(), 
                            status_code=400
                        )
                
                body = await request.body()
                if not body:
                    return Response(b'Empty request body', status_code=400)
                
                event = Event.deserialize(body)
                request_id = f'{threading.current_thread().ident}_{id(request)}'
                event.request_id = request_id
                
                if self.event_loop:
                    future = self.event_loop.create_future()
                    self._response_futures[request_id] = future
                    
                    self.event_queue.put(event)
                    
                    try:
                        response_data = await future
                        return Response(response_data, media_type='application/octet-stream')
                    except asyncio.CancelledError:
                        return Response(b'Request cancelled', status_code=499)
                    finally:
                        self._response_futures.pop(request_id, None)
                else:
                    return Response(b'Server not initialized', status_code=500)

            except Exception as e:
                logger.error(f'Error handling HTTP request: {e}')
                return Response(f'Error: {e}'.encode(), status_code=500)
        
        if '?' in self.path:
            route_path = self.path.split('?')[0]
        else:
            route_path = self.path
            
        routes = [
            Route(route_path, handle_request, methods=['POST']),
        ]
        
        app = Starlette(routes=routes)
        
        app.add_middleware(
            CORSMiddleware,
            allow_origins=["*"],
            allow_credentials=True,
            allow_methods=["*"],
            allow_headers=["*"],
        )
        
        return app

    def _resolve_response(self, request_id: str, response_data: bytes) -> None:
        """Resolve a pending response future. MUST be called on the event loop thread."""
        future = self._response_futures.get(request_id)
        if future is not None and not future.done():
            future.set_result(response_data)

    def _cleanup_futures_on_loop(self) -> None:
        """Cancel all pending futures. MUST be called on the event loop thread."""
        for future in list(self._response_futures.values()):
            if not future.done():
                future.cancel()
        self._response_futures.clear()

    def _serve(self):
        self.event_loop = asyncio.new_event_loop()
        asyncio.set_event_loop(self.event_loop)
        
        try:
            config = uvicorn.Config(
                app=self.app,
                host=self.host,
                port=self.port,
                log_level='error',
                loop=self.event_loop
            )
            self.server = uvicorn.Server(config)

            logger.info(f'Starting HTTP server on {self.host}:{self.port}{self.path}')
            self._server_started.set()
            self.event_loop.run_until_complete(self.server.serve())
            
        except Exception as e:
            if not self._shutdown_event.is_set():
                logger.error(f'HTTP server error: {e}')
            
        finally:
            # Event loop has stopped; no concurrent access possible — direct cleanup is safe
            self._cleanup_futures_on_loop()
            
            if not self.event_loop.is_closed():
                try:
                    pending = asyncio.all_tasks(self.event_loop)
                    if pending:
                        self.event_loop.run_until_complete(asyncio.gather(*pending, return_exceptions=True))
                    self.event_loop.close()
                except Exception as e:
                    if not self._shutdown_event.is_set():
                        logger.error(f'Error closing event loop: {e}')
    
    def start(self):
        self.app = self._create_app()
        
        server_thread = threading.Thread(target=self._serve)
        server_thread.daemon = True
        server_thread.start()
        
        wait(
            self._server_started.wait,
            self._server_started.is_set,
            MAXIMUM_WAIT_TIMEOUT,
        )
        
        logger.info(f'HTTP server thread started for {self.host}:{self.port}{self.path}')
    
    def reply(self, event: Event):
        """Thread-safe: schedules response resolution on the event loop thread."""
        if not event.request_id:
            logger.warning('Reply event missing request_id')
            return

        response_data = event.serialize()

        if self.event_loop and not self.event_loop.is_closed():
            try:
                self.event_loop.call_soon_threadsafe(
                    self._resolve_response, event.request_id, response_data
                )
            except RuntimeError:
                if not self._shutdown_event.is_set():
                    logger.warning('Could not send reply: event loop closed')

    def shutdown(self):
        if self._shutdown_event.is_set():
            return
        
        self._shutdown_event.set()
        
        if self.server and self.event_loop and not self.event_loop.is_closed():
            def shutdown_server():
                try:
                    self._cleanup_futures_on_loop()
                    self.event_queue.put(Event(tag=EventTag.SHUTDOWN_FROM_SERVER))
                    
                    if not self.server.should_exit:
                        self.server.should_exit = True
                        if hasattr(self.server, 'servers') and self.server.servers:
                            for server in self.server.servers:
                                server.close()
                except Exception as e:
                    logger.error(f'Error during server shutdown: {e}')
            
            try:
                self.event_loop.call_soon_threadsafe(shutdown_server)
            except RuntimeError:
                pass
    
    def destroy(self):
        self.shutdown()
    
    def cancel_all_calls(self):
        """Thread-safe: schedules future cancellation on the event loop thread."""
        if self.event_loop and not self.event_loop.is_closed():
            try:
                self.event_loop.call_soon_threadsafe(self._cleanup_futures_on_loop)
            except RuntimeError:
                pass