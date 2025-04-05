from enum import Enum

BASE_RESPONSE = 'BaseResponse'

class Code(Enum):
    UNKNOWN = 0
    SUCCESS = 1
    ERROR_INVALID = 2
    ERROR_TIMEOUT = 3
    ERROR_UNAVAILABLE = 4
    BUSY = 5
    IDLE = 6
    PENDING = 7
