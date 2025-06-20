from ..event import Event

class BaseServer:
    def __call__(self, bind_address: str):
        self.bind_address = bind_address
    
    def start(self):
        raise NotImplementedError('This method should be implemented by subclasses.')

    def pool(self, timeout: float = 0.0) -> Event | None:
        raise NotImplementedError('This method should be implemented by subclasses.')

    def reply(self, event: Event):
        raise NotImplementedError('This method should be implemented by subclasses.')

    def shutdown(self):
        raise NotImplementedError('This method should be implemented by subclasses.')
    
    def destroy(self):
        raise NotImplementedError('This method should be implemented by subclasses.')
    
    def cancel_all_calls(self):
        raise NotImplementedError('This method should be implemented by subclasses.')