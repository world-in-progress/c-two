from ..event import Event, EventQueue

class BaseServer:
    def __init__(self, bind_address: str, event_queue: EventQueue | None = None):
        self.bind_address = bind_address
        self.event_queue = event_queue

    def register_queue(self, event_queue: EventQueue) -> None:
        # Empty event queue if self.event_queue is not None
        if self.event_queue is not None:
            self.event_queue.shutdown()
            self.event_queue = None

        self.event_queue = event_queue

    def start(self):
        raise NotImplementedError('This method should be implemented by subclasses.')

    def reply(self, event: Event):
        raise NotImplementedError('This method should be implemented by subclasses.')

    def shutdown(self):
        raise NotImplementedError('This method should be implemented by subclasses.')
    
    def destroy(self):
        raise NotImplementedError('This method should be implemented by subclasses.')
    
    def cancel_all_calls(self):
        raise NotImplementedError('This method should be implemented by subclasses.')