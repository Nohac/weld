"""Bounded full-run Android diagnostics capture; owns only its logcat process."""
import subprocess
import threading

MAX_BYTES = 16 * 1024 * 1024
MAX_LINE = 8192


def copy_bounded(source, destination, maximum=MAX_BYTES):
    written = 0
    truncated = False
    marker = b"WELD_CAPTURE_GAP reason=size_limit\n"
    while True:
        line = source.readline(MAX_LINE)
        if not line:
            break
        if written + len(line) <= maximum - len(marker) and not truncated:
            destination.write(line)
            destination.flush()
            written += len(line)
        elif not truncated:
            destination.write(marker)
            destination.flush()
            truncated = True
        # Continue draining after the limit: logging must never block the app.
    return truncated


class LogCapture:
    def __init__(self, adb, path):
        self.path = path
        self.failure = None
        self.process = subprocess.Popen(
            [*adb, "logcat", "-T", "1", "-v", "epoch", "-s", "godot:I", "PxrMetric:I", "*:S"],
            stdout=subprocess.PIPE, stderr=subprocess.STDOUT)
        self.thread = threading.Thread(target=self.copy, name="weld-logcat", daemon=True)
        self.thread.start()

    def copy(self):
        try:
            with self.path.open("wb") as output:
                copy_bounded(self.process.stdout, output)
        except OSError as error:
            self.failure = error

    def check(self):
        if self.failure is not None:
            raise RuntimeError(f"device log capture failed: {self.failure}")
        if self.process.poll() is not None:
            raise RuntimeError(f"device log capture exited; see {self.path}")

    def close(self):
        self.process.terminate()
        try:
            self.process.wait(timeout=5)
        except subprocess.TimeoutExpired:
            self.process.kill()
            self.process.wait(timeout=5)
        self.thread.join(timeout=5)
        if self.thread.is_alive():
            raise RuntimeError("device log reader did not stop")
        self.process.stdout.close()
        if self.failure is not None:
            raise RuntimeError(f"device log capture failed: {self.failure}")
