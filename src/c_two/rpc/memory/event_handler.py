import os
import logging
import threading
from watchdog.events import FileSystemEventHandler

class MemoryEventHandler(FileSystemEventHandler):
    def __init__(self, file_pattern: str, file_extension: str = '.mem'):
        self.file_received = False
        self.file_pattern = file_pattern
        self.file_extension = file_extension
        self.condition = threading.Condition()
    
    def on_moved(self, event):
        if not event.is_directory:
            dest_filename = os.path.basename(event.dest_path)
            if dest_filename.startswith(self.file_pattern) and dest_filename.endswith(self.file_extension):
                with self.condition:
                    self.file_received = True
                    self.condition.notify_all() # wake up all waiting threads
                    