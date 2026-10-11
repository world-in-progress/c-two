"""Scoped OS resources for the local development test executors."""
from contextlib import contextmanager
import hashlib
import os
from pathlib import Path
import re
import shutil
import signal
import subprocess
import time
from typing import Iterator


class PlatformError(Exception):
    pass


def core_library_name(platform: str) -> str:
    return 'fastdb.dll' if platform == 'win32' else 'libfastdb.dylib' if platform == 'darwin' else 'libfastdb.so'


def validate_node_runtime(version: str, napi: str | int) -> None:
    try:
        api = int(napi)
    except (ValueError, TypeError) as error:
        raise PlatformError('需要Node22，Node-API >=10；无法识别Node-API版本') from error
    if not re.fullmatch(r'v?22\.\d+\.\d+', version.strip()) or api < 10:
        raise PlatformError(f'需要Node22，Node-API >=10；实际为{version}, Node-API {napi}')


@contextmanager
def resource_lease(resource: Path) -> Iterator[None]:
    """Nonblocking lease, stable across differing run temporary directories."""
    leases = Path.home() / '.cache' / 'c-two' / 'python-test-leases'
    leases.mkdir(parents=True, exist_ok=True)
    key = hashlib.sha256(os.path.normcase(str(resource.resolve())).encode()).hexdigest()
    # Never unlink: contenders may already hold an open handle to this file.
    with (leases / key).open('a+b') as stream:
        if os.name == 'nt':
            import msvcrt
            stream.seek(0, 2)
            if stream.tell() == 0:
                stream.write(b'\0')
                stream.flush()
            stream.seek(0)
            try:
                msvcrt.locking(stream.fileno(), msvcrt.LK_NBLCK, 1)
            except OSError as error:
                raise PlatformError(f'另一验证入口正使用资源：{resource}') from error
            try:
                yield
            finally:
                stream.seek(0)
                msvcrt.locking(stream.fileno(), msvcrt.LK_UNLCK, 1)
        else:
            import fcntl
            try:
                fcntl.flock(stream, fcntl.LOCK_EX | fcntl.LOCK_NB)
            except BlockingIOError as error:
                raise PlatformError(f'另一验证入口正使用资源：{resource}') from error
            try:
                yield
            finally:
                fcntl.flock(stream, fcntl.LOCK_UN)


class _WindowsJob:
    """Kill-on-close Job Object; children cannot run before assignment."""
    def __init__(self):
        import ctypes
        from ctypes import wintypes as w
        self.ctypes = ctypes
        self.kernel = ctypes.WinDLL('kernel32', use_last_error=True)
        k = self.kernel
        signatures = {
            'CreateJobObjectW': ([ctypes.c_void_p, w.LPCWSTR], w.HANDLE),
            'SetInformationJobObject': ([w.HANDLE, ctypes.c_int, ctypes.c_void_p, w.DWORD], w.BOOL),
            'QueryInformationJobObject': ([w.HANDLE, ctypes.c_int, ctypes.c_void_p, w.DWORD, ctypes.c_void_p], w.BOOL),
            'AssignProcessToJobObject': ([w.HANDLE, w.HANDLE], w.BOOL),
            'TerminateJobObject': ([w.HANDLE, w.UINT], w.BOOL),
            'CloseHandle': ([w.HANDLE], w.BOOL),
            'CreateToolhelp32Snapshot': ([w.DWORD, w.DWORD], w.HANDLE),
            'Thread32First': ([w.HANDLE, ctypes.c_void_p], w.BOOL),
            'Thread32Next': ([w.HANDLE, ctypes.c_void_p], w.BOOL),
            'OpenThread': ([w.DWORD, w.BOOL, w.DWORD], w.HANDLE),
            'ResumeThread': ([w.HANDLE], w.DWORD),
            'OpenProcess': ([w.DWORD, w.BOOL, w.DWORD], w.HANDLE),
            'QueryFullProcessImageNameW': ([w.HANDLE, w.DWORD, w.LPWSTR, ctypes.POINTER(w.DWORD)], w.BOOL),
        }
        for name, (args, result) in signatures.items():
            function = getattr(k, name)
            function.argtypes, function.restype = args, result

        class BasicLimits(ctypes.Structure):
            _fields_ = [('PerProcessUserTimeLimit', ctypes.c_longlong), ('PerJobUserTimeLimit', ctypes.c_longlong),
                        ('LimitFlags', w.DWORD), ('MinimumWorkingSetSize', ctypes.c_size_t),
                        ('MaximumWorkingSetSize', ctypes.c_size_t), ('ActiveProcessLimit', w.DWORD),
                        ('Affinity', ctypes.c_size_t), ('PriorityClass', w.DWORD), ('SchedulingClass', w.DWORD)]
        class ExtendedLimits(ctypes.Structure):
            _fields_ = [('BasicLimitInformation', BasicLimits), ('IoInfo', ctypes.c_ulonglong * 6),
                        ('ProcessMemoryLimit', ctypes.c_size_t), ('JobMemoryLimit', ctypes.c_size_t),
                        ('PeakProcessMemoryUsed', ctypes.c_size_t), ('PeakJobMemoryUsed', ctypes.c_size_t)]
        self.handle = k.CreateJobObjectW(None, None)
        if not self.handle:
            raise ctypes.WinError(ctypes.get_last_error())
        limits = ExtendedLimits()
        limits.BasicLimitInformation.LimitFlags = 0x2000  # JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE
        if not k.SetInformationJobObject(self.handle, 9, ctypes.byref(limits), ctypes.sizeof(limits)):
            error = ctypes.WinError(ctypes.get_last_error())
            self.close()
            raise error

    def assign_and_resume(self, process):
        c, k = self.ctypes, self.kernel
        if not k.AssignProcessToJobObject(self.handle, int(process._handle)):
            raise c.WinError(c.get_last_error())
        # Popen closes CreateProcess's thread handle. Locate the still-suspended
        # primary thread through Toolhelp, then resume only after job assignment.
        class ThreadEntry(c.Structure):
            _fields_ = [('size', c.c_ulong), ('usage', c.c_ulong), ('tid', c.c_ulong),
                        ('pid', c.c_ulong), ('base', c.c_long), ('delta', c.c_long), ('flags', c.c_ulong)]
        snapshot = k.CreateToolhelp32Snapshot(4, 0)
        if snapshot == c.c_void_p(-1).value:
            raise c.WinError(c.get_last_error())
        try:
            entry = ThreadEntry()
            entry.size = c.sizeof(entry)
            found = k.Thread32First(snapshot, c.byref(entry))
            while found:
                if entry.pid == process.pid:
                    thread = k.OpenThread(2, False, entry.tid)
                    if not thread:
                        raise c.WinError(c.get_last_error())
                    try:
                        if k.ResumeThread(thread) == 0xFFFFFFFF:
                            raise c.WinError(c.get_last_error())
                    finally:
                        k.CloseHandle(thread)
                    return
                found = k.Thread32Next(snapshot, c.byref(entry))
            raise OSError('Suspended primary thread was not found')
        finally:
            k.CloseHandle(snapshot)

    def alive(self):
        c = self.ctypes
        # JOBOBJECT_BASIC_ACCOUNTING_INFORMATION: four LARGE_INTEGERs,
        # then PageFaultCount, TotalProcesses, ActiveProcesses, TerminatedProcesses.
        class Accounting(c.Structure):
            _fields_ = [('times', c.c_longlong * 4), ('page_faults', c.c_ulong),
                        ('total', c.c_ulong), ('active', c.c_ulong), ('terminated', c.c_ulong)]
        value = Accounting()
        if not self.kernel.QueryInformationJobObject(self.handle, 1, c.byref(value), c.sizeof(value), None):
            raise c.WinError(c.get_last_error())
        return value.active != 0

    def stop(self):
        if not self.kernel.TerminateJobObject(self.handle, 1):
            raise self.ctypes.WinError(self.ctypes.get_last_error())

    def diagnostics(self):
        c, k = self.ctypes, self.kernel
        capacity = 64
        while True:
            class ProcessIds(c.Structure):
                _fields_ = [('assigned', c.c_ulong), ('listed', c.c_ulong), ('pids', c.c_size_t * capacity)]
            value = ProcessIds()
            if k.QueryInformationJobObject(self.handle, 3, c.byref(value), c.sizeof(value), None):
                break
            if c.get_last_error() != 234:  # ERROR_MORE_DATA
                raise c.WinError(c.get_last_error())
            capacity *= 2
        rows = []
        for pid in value.pids[:value.listed]:
            row = {'pid': pid}
            handle = k.OpenProcess(0x1000, False, pid)  # PROCESS_QUERY_LIMITED_INFORMATION
            if handle:
                try:
                    image = c.create_unicode_buffer(32768)
                    size = c.c_ulong(len(image))
                    if k.QueryFullProcessImageNameW(handle, 0, image, c.byref(size)):
                        row['image'] = image.value
                    else:
                        row['query_error'] = c.get_last_error()
                finally:
                    k.CloseHandle(handle)
            else:
                row['query_error'] = c.get_last_error()
            rows.append(row)
        return rows

    def close(self):
        if self.handle:
            if not self.kernel.CloseHandle(self.handle):
                raise self.ctypes.WinError(self.ctypes.get_last_error())
            self.handle = None


class ProcessTree:
    def __init__(self, command, cwd, env, stdout):
        self._closed = False
        self._job = _WindowsJob() if os.name == 'nt' else None
        self.process = None
        try:
            self.process = subprocess.Popen(command, cwd=cwd, env=env, stdout=stdout, stderr=subprocess.STDOUT,
                                            creationflags=4 if self._job else 0,
                                            start_new_session=self._job is None)
            if self._job:
                self._job.assign_and_resume(self.process)
        except BaseException:
            if self.process is not None:
                self.process.kill()
                self.process.wait(timeout=5)
            if self._job:
                self._job.close()
            raise

    def alive(self) -> bool:
        if self._closed:
            return False
        if self._job:
            return self._job.alive()
        try:
            os.killpg(self.process.pid, 0)
            return True
        except ProcessLookupError:
            return False

    def stop(self) -> None:
        if self._closed:
            return
        if self._job:
            self._job.stop()
        else:
            for sig in (signal.SIGINT, signal.SIGKILL):
                try:
                    os.killpg(self.process.pid, sig)
                except ProcessLookupError:
                    break
                if sig == signal.SIGINT:
                    try:
                        self.process.wait(timeout=5)
                    except subprocess.TimeoutExpired:
                        pass
        self.process.wait(timeout=5)
        deadline = time.monotonic() + 5
        while self.alive() and time.monotonic() < deadline:
            time.sleep(.02)
        if self.alive():
            raise OSError('Owned process tree remains alive after termination')

    def diagnostics(self) -> list[dict]:
        if self._closed:
            return []
        return self._job.diagnostics() if self._job else [{'process_group': self.process.pid}]

    def reap_compiler_helpers(self, env: dict[str, str]) -> list[dict]:
        """Finish the selected MSVC telemetry helper's owned lifecycle.

        This is allowed only after the parent exits and every remaining Job
        process is the exact helper beside the selected compiler. Unknown
        descendants remain a failed cleanup, even if named vctip.exe.
        """
        if self._closed or not self._job or self.process.poll() is None:
            return []
        compiler = shutil.which('cl', path=env.get('PATH', ''))
        if compiler is None:
            return []
        helper = Path(compiler).resolve().parent / 'vctip.exe'
        if not helper.is_file():
            return []
        expected = os.path.normcase(str(helper.resolve()))
        rows = self.diagnostics()
        if not rows or any('image' not in row or os.path.normcase(str(Path(row['image']).resolve())) != expected for row in rows):
            return []
        self.stop()
        return rows

    def close(self) -> None:
        if self._closed:
            return
        try:
            if self.alive():
                self.stop()
        finally:
            if self._job:
                self._job.close()
            self._closed = True
