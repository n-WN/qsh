#!/usr/bin/env python3
"""Compare qsh and ssh: startup, key echo, throughput, Ctrl-C in a flood.

Each client runs in a pty, as in a terminal. The script reads the output as
fast as it can (it does not draw), and answers cursor position queries as a
terminal does.

Usage:
  bench.py DEST --qsh PATH [--ssh "ssh -i KEY"] [--port P] [--runs N]
           [--only startup,echo,throughput,ctrlc]

Example:
  tests/bench/bench.py root@server.example.com --qsh target/release/qsh \\
      --ssh "ssh -i ~/.ssh/key" --port 60100
"""

import argparse
import fcntl
import os
import pty
import re
import select
import shlex
import signal
import statistics
import struct
import sys
import termios
import time

COLS, ROWS = 120, 40
ESC = re.compile(rb"\x1b(\[[0-9;?<>=:$ ]*[@-~]|\][^\x07\x1b]*(\x07|\x1b\\)|P[^\x1b]*\x1b\\|[()][0-9A-Za-z]|.)")


class Term:
    """A program in a pty, with a growing output buffer."""

    def __init__(self, argv):
        self.pid, self.fd = pty.fork()
        if self.pid == 0:
            os.execvp(argv[0], argv)
        fcntl.ioctl(self.fd, termios.TIOCSWINSZ, struct.pack("HHHH", ROWS, COLS, 0, 0))
        self.buf = b""
        self.total = 0

    def pump(self, timeout):
        r, _, _ = select.select([self.fd], [], [], timeout)
        if not r:
            return False
        try:
            d = os.read(self.fd, 1 << 20)
        except OSError:
            raise EOFError
        if not d:
            raise EOFError
        self.total += len(d)
        if b"\x1b[6n" in d:
            os.write(self.fd, b"\x1b[1;1R")
        self.buf = (self.buf + d)[-65536:]
        return True

    def send(self, b):
        os.write(self.fd, b)

    def wait(self, test, timeout=60):
        """Read until test(buffer) is true. Returns the time it took."""
        t0 = time.monotonic()
        while not test(self.buf):
            left = timeout - (time.monotonic() - t0)
            if left <= 0:
                raise TimeoutError(repr(self.buf[-300:]))
            try:
                self.pump(left)
            except EOFError:
                raise EOFError("the client exited: " + repr(plain(self.buf)[-400:]))
        return time.monotonic() - t0

    def mark(self):
        self.buf = b""

    def close(self):
        try:
            os.kill(self.pid, signal.SIGTERM)
        except ProcessLookupError:
            pass
        try:
            os.waitpid(self.pid, 0)
        except ChildProcessError:
            pass
        os.close(self.fd)


def plain(b):
    return ESC.sub(b"", b)


def at_prompt(b):
    t = plain(b).rstrip(b" ")
    return t.endswith(b"#") or t.endswith(b"$")


def clients(a):
    ssh = shlex.split(a.ssh) + ["-o", "ControlMaster=no", "-o", "ControlPath=none"]
    port = ["--port", str(a.port)] if a.port else []
    qsh = [a.qsh, "-s", "bench"] + port + ["--ssh", " ".join(ssh), a.dest]
    return {
        "ssh": ssh + ["-tt", a.dest],
        "qsh": qsh,
        "qsh (no prediction)": qsh[:1] + ["--predict", "never"] + qsh[1:],
    }


def start(argv):
    t0 = time.monotonic()
    t = Term(argv)
    t.wait(at_prompt, 60)
    dt = time.monotonic() - t0
    # Let the shell settle (prompt hooks, title).
    while t.pump(0.3):
        pass
    return t, dt


def stop(t):
    t.send(b"exit\r")
    try:
        t.wait(lambda b: False, 5)
    except (EOFError, TimeoutError):
        pass
    t.close()


def ms(x):
    return f"{x * 1000:7.0f} ms"


def bench_startup(name, argv, runs):
    times = []
    for _ in range(runs):
        t, dt = start(argv)
        times.append(dt)
        stop(t)
    return f"startup to prompt   {name:22} median {ms(statistics.median(times))}  (min {ms(min(times))}, {runs} runs)"


def bench_echo(name, argv, runs):
    t, _ = start(argv)
    times = []
    for _ in range(runs):
        t.mark()
        t.send(b"Q")
        times.append(t.wait(lambda b: b"Q" in b, 10))
        while t.pump(0.15):
            pass
        t.send(b"\x7f")
        while t.pump(0.25):
            pass
    stop(t)
    return f"key echo            {name:22} median {ms(statistics.median(times))}  (p90 {ms(sorted(times)[int(len(times) * 0.9) - 1])}, {runs} keys)"


def bench_throughput(name, argv, lines):
    t, _ = start(argv)
    t.mark()
    t.total = 0
    t.send(f"seq 1 {lines}; echo DONE-$((6*7))\r".encode())
    dt = t.wait(lambda b: b"DONE-42" in b, 600)
    mb = t.total / 1e6
    stop(t)
    return f"output {mb:5.1f} MB      {name:22} {dt:7.2f} s  ({mb / dt:5.1f} MB/s)"


def bench_ctrlc(name, argv):
    t, _ = start(argv)
    t.send(b"yes 0123456789012345678901234567890123456789012345678901234567890123456789\r")
    t0 = time.monotonic()
    while time.monotonic() - t0 < 3:
        t.pump(0.1)
    t.mark()
    t.send(b"\x03")
    t.send(b"echo STOP-$((5*9))\r")
    dt = t.wait(lambda b: b"STOP-45" in b, 120)
    stop(t)
    return f"Ctrl-C in a flood   {name:22} {ms(dt)} until the next command ran"


def main():
    p = argparse.ArgumentParser()
    p.add_argument("dest")
    p.add_argument("--qsh", required=True)
    p.add_argument("--ssh", default="ssh")
    p.add_argument("--port", type=int)
    p.add_argument("--runs", type=int, default=5)
    p.add_argument("--lines", type=int, default=2_000_000)
    p.add_argument("--only", default="startup,echo,throughput,ctrlc")
    a = p.parse_args()
    only = a.only.split(",")
    c = clients(a)
    for name, argv in c.items():
        if "startup" in only and name != "qsh (no prediction)":
            print(bench_startup(name, argv, a.runs), flush=True)
    for name, argv in c.items():
        if "echo" in only:
            print(bench_echo(name, argv, a.runs * 4), flush=True)
    for name, argv in c.items():
        if "throughput" in only and name != "qsh (no prediction)":
            print(bench_throughput(name, argv, a.lines), flush=True)
    for name, argv in c.items():
        if "ctrlc" in only and name != "qsh (no prediction)":
            print(bench_ctrlc(name, argv), flush=True)


if __name__ == "__main__":
    sys.exit(main())
