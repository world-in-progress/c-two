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
        
        # Parse bind address (e.g., "http://0.0.0.0:8000/path" -> host="0.0.0.0", port=8000, path="/path")
        self.host, self.port, self.path = self._parse_bind_address(bind_address)
        
        self.app = None
        self.server = None
        self.event_loop = None
        self._server_started = threading.Event()
        self._shutdown_event = threading.Event()
        self.response_futures: dict[str, asyncio.Future] = {}
        
    def _parse_bind_address(self, bind_address: str) -> tuple[str, int, str]:
        """Parse HTTP bind address like 'http://0.0.0.0:8000/path?query=value'"""
        if bind_address.startswith('http://'):
            address = bind_address[7:]  # remove 'http://'
        else:
            address = bind_address
        
        # Split address and path (including query parameters)
        if '/' in address:
            address_part, path = address.split('/', 1)
            path = '/' + path  # ensure path starts with /
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
        """Create Starlette application with single POST handler"""
        
        async def handle_request(request: Request) -> Response:
            """Handle all POST requests by deserializing to Event and queuing"""
            try:
                # Check if shutdown is requested
                if self._shutdown_event.is_set():
                    return Response(b'Server is shutting down', status_code=503)
                
                # Validate query parameters if specified in bind_address
                if '?' in self.path:
                    expected_query = self.path.split('?', 1)[1]
                    actual_query = str(request.url.query)
                    if actual_query != expected_query:
                        return Response(
                            f'Query parameters mismatch. Expected: {expected_query}, Got: {actual_query}'.encode(), 
                            status_code=400
                        )
                
                # Get request body as bytes
                body = await request.body()
                if not body:
                    return Response(b'Empty request body', status_code=400)
                
                # Deserialize to Event
                event = Event.deserialize(body)
                
                # Generate request ID for tracking responses
                request_id = f'{threading.current_thread().ident}_{id(request)}'
                event.request_id = request_id
                
                # Create future for response (no timeout)
                if self.event_loop:
                    future = self.event_loop.create_future()
                    self.response_futures[request_id] = future
                    
                    # Put event in queue for server thread to process
                    self.event_queue.put(event)
                    
                    # Wait for response indefinitely - no timeout for compute-intensive tasks
                    try:
                        response_data = await future
                        return Response(response_data, media_type='application/octet-stream')
                    except asyncio.CancelledError:
                        return Response(b'Request cancelled', status_code=499)
                    finally:
                        self.response_futures.pop(request_id, None)
                else:
                    return Response(b'Server not initialized', status_code=500)

            except Exception as e:
                logger.error(f'Error handling HTTP request: {e}')
                return Response(f'Error: {e}'.encode(), status_code=500)
        
        # Create route that matches the exact path (including query parameters)
        # Split path and query for route configuration
        if '?' in self.path:
            route_path = self.path.split('?')[0]  # Use only the path part for routing
        else:
            route_path = self.path
            
        routes = [
            Route(route_path, handle_request, methods=['POST']),
        ]
        
        app = Starlette(routes=routes)
        
        # Add CORS middleware
        app.add_middleware(
            CORSMiddleware,
            allow_origins=["*"],
            allow_credentials=True,
            allow_methods=["*"],
            allow_headers=["*"],
        )
        
        return app
    
    def _cleanup_futures(self) -> int:
        """Cancel all pending futures and clear the response futures dictionary"""
        cancelled_count = 0
        
        for future in list(self.response_futures.values()):
            if not future.done():
                future.cancel()
                cancelled_count += 1
        self.response_futures.clear()
        
        return cancelled_count
    
    def _serve(self):
        """Run the HTTP server, similar to TcpServer._serve"""
        # Create new event loop for this thread
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
            
            # Signal that server is ready
            self._server_started.set()
            
            # Run server
            self.event_loop.run_until_complete(self.server.serve())
            
        except Exception as e:
            # Only log error if it's not due to shutdown
            if not self._shutdown_event.is_set():
                logger.error(f'HTTP server error: {e}')
            
        finally:
            self._cleanup_futures()
            
            # Close event loop properly
            if not self.event_loop.is_closed():
                try:
                    # Wait for any remaining tasks
                    pending = asyncio.all_tasks(self.event_loop)
                    if pending:
                        self.event_loop.run_until_complete(asyncio.gather(*pending, return_exceptions=True))
                    self.event_loop.close()
                except Exception as e:
                    if not self._shutdown_event.is_set():
                        logger.error(f'Error closing event loop: {e}')
    
    def start(self):
        """Start the HTTP server, similar to TcpServer.start"""
        self.app = self._create_app()
        
        # Start server in daemon thread
        server_thread = threading.Thread(target=self._serve)
        server_thread.daemon = True
        server_thread.start()
        
        # Wait for server to actually start
        wait(
            self._server_started.wait,
            self._server_started.is_set,
            MAXIMUM_WAIT_TIMEOUT,
        )
        
        logger.info(f'HTTP server thread started for {self.host}:{self.port}{self.path}')
    
    def reply(self, event: Event):
        """Send reply to HTTP client - this is the ONLY place that responds to clients"""
        if not event.request_id:
            logger.warning('Reply event missing request_id')
            return
            
        future = self.response_futures.get(event.request_id)
        if future and not future.done():
            # Serialize event and send as response
            response_data = event.serialize()
            
            if self.event_loop and not self.event_loop.is_closed():
                try:
                    self.event_loop.call_soon_threadsafe(
                        future.set_result, response_data
                    )
                except RuntimeError as e:
                    # Event loop might be closed during shutdown
                    if not self._shutdown_event.is_set():
                        logger.warning(f'Could not send reply: {e}')
            else:
                logger.warning('Event loop is not available for reply')
        else:
            if event.request_id not in self.response_futures:
                logger.warning(f'No pending future found for request_id: {event.request_id}')
            else:
                logger.warning(f'Future already completed for request_id: {event.request_id}')

    def shutdown(self):
        """Shutdown the HTTP server, similar to TcpServer.shutdown"""
        if self._shutdown_event.is_set():
            return
        
        self._shutdown_event.set()
        
        if self.server and self.event_loop and not self.event_loop.is_closed():
            # Schedule shutdown in the event loop
            def shutdown_server():
                try:
                    self._cleanup_futures()
                    
                    # Add shutdown event to queue
                    self.event_queue.put(Event(tag=EventTag.SHUTDOWN_FROM_SERVER))
                    
                    # Shutdown uvicorn server
                    if not self.server.should_exit:
                        self.server.should_exit = True
                        # Force shutdown if server is still running
                        if hasattr(self.server, 'servers') and self.server.servers:
                            for server in self.server.servers:
                                server.close()
                except Exception as e:
                    logger.error(f'Error during server shutdown: {e}')
            
            try:
                self.event_loop.call_soon_threadsafe(shutdown_server)
            except RuntimeError:
                # Event loop might already be closed
                pass
    
    def destroy(self):
        """Clean up resources, similar to TcpServer.destroy"""
        # Shutdown the server
        self.shutdown()
    
    def cancel_all_calls(self):
        """Cancel all pending HTTP requests, similar to TcpServer.cancel_all_calls"""
        cancelled_count = self._cleanup_futures()
        logger.info(f'Cancelled {cancelled_count} HTTP calls')