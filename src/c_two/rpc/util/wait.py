import time
from typing import Callable

MAXIMUM_WAIT_TIMEOUT = 0.1

def _wait_once(
    wait_fn: Callable[..., bool],
    timeout: float,
    spin_cb: Callable[[], None] | None = None
):
    wait_fn(timeout=timeout)
    if spin_cb is not None:
        spin_cb()

def wait(
    wait_fn: Callable[..., bool],
    wait_complete_fn: Callable[[], bool],
    timeout: float,
    spin_cb: Callable[[], None] | None = None
):
    """Wait for a condition to be met or a timeout to occur.

    Args:
      wait_fn: A callable acceptable a single float-valued kwarg named
        `timeout`. This function is expected to be one of `threading.Event.wait`
        or `threading.Condition.wait`.
      wait_complete_fn: A callable taking no arguments and returning a bool.
        When this function returns true, it indicates that waiting should cease.
      timeout: An optional float-valued number of seconds after which the wait
        should cease.
      spin_cb: An optional Callable taking no arguments and returning nothing.
        This callback will be called on each iteration of the spin. This may be
        used for, e.g. work related to forking.

    Returns:
      bool: True if the wait completed successfully, False if it timed out.
    """
    if timeout is None:
        while not wait_complete_fn():
            _wait_once(wait_fn, MAXIMUM_WAIT_TIMEOUT, spin_cb)
        return True
    else:
        end = time.time() + timeout
        while not wait_complete_fn():
            remaining = min(end - time.time(), MAXIMUM_WAIT_TIMEOUT)
            if remaining < 0:
                return True
            _wait_once(wait_fn, remaining, spin_cb)
        return False