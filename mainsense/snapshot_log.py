"""Append JSON Lines snapshots without blocking collection on the log file."""

from collections import deque
import errno
import fcntl
import json
import os
import stat
import threading


class SnapshotLogError(OSError):
    """Report a failure owned by the snapshot log."""


class SnapshotLog:
    MAX_PENDING = 32
    CLOSE_TIMEOUT = 2.0

    def __init__(self, path, stdout):
        self.path = path
        try:
            self.stdout_stat = os.fstat(stdout.fileno())
        except (OSError, ValueError):
            self.stdout_stat = None
        self.pending = deque()
        self.ready = threading.Condition()
        self.closing = False
        self.abort = threading.Event()
        self.error = None
        self.thread = threading.Thread(target=self._write_pending, name="mainsense-log", daemon=True)

    def __enter__(self):
        # Validate the path before starting the monitor, even if no sample is taken.
        os.close(self._open())
        self.thread.start()
        return self

    def __exit__(self, exc_type, exc, trace):
        with self.ready:
            self.closing = True
            self.ready.notify()
        self.thread.join(self.CLOSE_TIMEOUT)
        if self.thread.is_alive():
            self.abort.set()
            if exc is None or isinstance(exc, KeyboardInterrupt):
                raise SnapshotLogError(errno.ETIMEDOUT, "timed out waiting for pending log records")
            return
        if self.error is not None and (exc is None or isinstance(exc, KeyboardInterrupt)):
            raise self.error

    def record(self, snapshot):
        with self.ready:
            if self.error is not None:
                raise self.error
            if len(self.pending) >= self.MAX_PENDING:
                raise SnapshotLogError(errno.ENOBUFS, "log writer fell behind and its queue is full")
            self.pending.append(snapshot)
            self.ready.notify()

    def _open(self):
        # A FIFO or other special destination must not block before validation.
        fd = os.open(self.path, os.O_RDWR | os.O_APPEND | os.O_CREAT | os.O_NONBLOCK, 0o666)
        try:
            info = os.fstat(fd)
            if self.stdout_stat is not None and os.path.samestat(info, self.stdout_stat):
                raise OSError(errno.EINVAL, "log file and stdout refer to the same destination")
            if not stat.S_ISREG(info.st_mode):
                raise OSError(errno.EINVAL, "log destination must be a regular file")
        except BaseException:
            os.close(fd)
            raise
        return fd

    def _write_pending(self):
        try:
            while not self.abort.is_set():
                with self.ready:
                    self.ready.wait_for(lambda: self.closing or self.pending)
                    if not self.pending:
                        return
                    snapshot = self.pending.popleft()
                self._append(snapshot)
        except Exception as error:
            with self.ready:
                self.error = (SnapshotLogError(error.errno, error.strerror, error.filename)
                              if isinstance(error, OSError) else error)

    def _append(self, snapshot):
        line = (json.dumps(snapshot, allow_nan=False) + "\n").encode("utf-8")
        # Open by path for each record so a rename takes effect on the next write.
        while not self.abort.is_set():
            fd = self._open()
            try:
                try:
                    fcntl.flock(fd, fcntl.LOCK_EX | fcntl.LOCK_NB)
                except BlockingIOError:
                    self.abort.wait(0.05)
                    continue
                if not self.abort.is_set():
                    self._write_line(fd, line)
                return
            finally:
                os.close(fd)

    @staticmethod
    def _write_line(fd, line):
        size = os.fstat(fd).st_size
        # An existing JSON Lines file may omit its final newline.
        if size and os.pread(fd, 1, size - 1) != b"\n":
            line = b"\n" + line
        try:
            if os.write(fd, line) != len(line):
                raise OSError(errno.EIO, "incomplete log write")
        except OSError:
            # Keep the caller's lock until any partial append has been removed.
            if os.fstat(fd).st_size > size:
                os.ftruncate(fd, size)
            raise
