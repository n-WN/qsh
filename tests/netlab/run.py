#!/usr/bin/env python3
"""Weak network tests for qsh and ssh, in the lab of lab.sh (Linux, root).

Each test sets a network profile in the emulator, runs the clients in a pty
(as a terminal does), and checks the result, not only the time:

  startup    time to the prompt
  echo       time until a typed key shows (qsh: also without prediction)
  integrity  60,000 numbered lines: each line arrives once, in order
  typing     a command typed in small pieces at random times runs once,
             with the exact text
  outage     a counter prints 20 lines per second; the network goes away for
             N seconds, and a command is typed during the outage. Time until
             output continues; every line once; the command runs once
  udpblock   UDP stops during a session (qsh moves to the SSH tunnel), then
             works again (qsh moves back)
  ctrlc      Ctrl-C during a flood on a slow link with a long queue
  idle       60 s without input on a lossy link: "no reply" badges
  revoke     a stolen device token works until `qsh devices --revoke`; then
             the thief's connection ends, its resume token and the token
             stop working, and the user's next start logs in over SSH
  soak       ssh and qsh side by side for N minutes of random profiles and
             outages, with a counter and typed commands

For qsh, the output checks use the session stream that the client received
(QSH_DUMP), so that the status overlay does not change the text.

Usage: run.py [--lab DIR] [--only test,...] [--soak MINUTES]
"""

import argparse
import fcntl
import json
import os
import pty
import random
import re
import select
import signal
import string
import struct
import sys
import termios
import time

COLS, ROWS = 120, 40
ESC = re.compile(rb"\x1b(\[[0-9;?<>=:$ ]*[@-~]|\][^\x07\x1b]*(\x07|\x1b\\)|P[^\x1b]*\x1b\\|[()][0-9A-Za-z]|.)")
COUNTER = re.compile(rb"QC(\d+)\.")
NUMBERED = re.compile(rb"QN(\d+)\.")

# One-way values; the round trip is twice the delay.
PROFILES = {
    "fiber": {"delay_ms": 10},
    "wifi": {"delay_ms": 20, "jitter_ms": 10, "loss": 0.01},
    "lte": {"delay_ms": 40, "jitter_ms": 20, "loss": 0.03, "burst": 3},
    "bad": {"delay_ms": 80, "jitter_ms": 40, "loss": 0.10, "burst": 4, "reorder": 0.02},
    "awful": {"delay_ms": 150, "jitter_ms": 60, "loss": 0.25, "burst": 5},
    "slow": {"delay_ms": 30, "rate_kbit": 1000, "queue_ms": 2000},
    "udp-blocked": {"delay_ms": 25, "block_udp": True},
}

L = "/tmp/qshlab"
SSH = []
DEST = "root@10.99.0.2"
RESULTS = []


def plain(b):
    return ESC.sub(b"", b)


def at_prompt(b):
    t = plain(b).rstrip()
    return t.endswith(b"#") or t.endswith(b"$")


def set_profile(p):
    with open(f"{L}/profile.json.new", "w") as f:
        json.dump(p, f)
    os.replace(f"{L}/profile.json.new", f"{L}/profile.json")
    time.sleep(0.05)


class Term:
    """A client in a pty. Keeps the last 64 KB of output, and the counter
    numbers in the output with their arrival times."""

    def __init__(self, argv, env):
        self.pid, self.fd = pty.fork()
        if self.pid == 0:
            os.execvpe(argv[0], argv, env)
        fcntl.ioctl(self.fd, termios.TIOCSWINSZ, struct.pack("HHHH", ROWS, COLS, 0, 0))
        self.buf, self.tail, self.all = b"", b"", 0
        self.seen = []
        self.token = None
        self.exited = None

    def pump(self, timeout):
        r, _, _ = select.select([self.fd], [], [], max(0, timeout))
        if not r:
            return False
        try:
            d = os.read(self.fd, 1 << 20)
        except OSError:
            d = b""
        if not d:
            raise EOFError
        self.all += len(d)
        if b"\x1b[6n" in d:
            os.write(self.fd, b"\x1b[1;1R")
        self.buf = (self.buf + d)[-65536:]
        if self.token:
            data = self.tail + d
            cut = data.rfind(b"\n") + 1
            self.tail = data[cut:]
            now = time.monotonic()
            self.seen += [(int(m.group(1)), now) for m in self.token.finditer(plain(data[:cut]))]
        return True

    def run_for(self, secs):
        end = time.monotonic() + secs
        while time.monotonic() < end:
            self.pump(end - time.monotonic())

    def wait(self, test, timeout=60):
        t0 = time.monotonic()
        while not test(self.buf):
            left = timeout - (time.monotonic() - t0)
            if left <= 0:
                raise TimeoutError(repr(plain(self.buf)[-300:]))
            self.pump(min(left, 0.5))
        return time.monotonic() - t0

    def send(self, b):
        os.write(self.fd, b)

    def type(self, text, gap=0.04):
        """Type in pieces of 1-6 bytes, at random times."""
        b = text.encode()
        while b:
            n = random.randint(1, 6)
            self.send(b[:n])
            b = b[n:]
            self.run_for(random.uniform(0, gap))

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


class Client:
    def __init__(self, kind, name):
        self.kind, self.name = kind, name
        self.dump = f"{L}/dump-{name}"
        env = dict(os.environ)
        if kind == "ssh":
            argv = SSH + ["-tt", DEST]
        else:
            for p in (self.dump, self.dump + ".in"):
                if os.path.exists(p):
                    os.remove(p)
            env.update(HOME=f"{L}/chome", QSH_LOG=f"{L}/c.log", QSH_DUMP=self.dump)
            argv = [f"{L}/qsh", "-s", name, "--port", "56901", "--server", f"{L}/qsh", "--ssh", " ".join(SSH), DEST]
            if kind == "qsh-np":
                argv[1:1] = ["--predict", "never"]
        self.t0 = time.monotonic()
        self.term = Term(argv, env)

    def start(self, timeout=120):
        self.term.wait(at_prompt, timeout)
        dt = time.monotonic() - self.t0
        self.term.run_for(0.3)
        return dt

    def numbers(self, regex):
        """Numbers in the session output, in order of arrival."""
        if self.kind == "ssh":
            return [n for n, _ in self.term.seen]
        with open(self.dump, "rb") as f:
            return [int(m.group(1)) for m in regex.finditer(plain(f.read()))]

    def stop(self):
        # Ctrl-C makes the tty discard unread input: "exit" goes after it.
        try:
            self.term.send(b"\x03")
            self.term.run_for(0.5)
            self.term.send(b"exit\r")
            self.term.wait(lambda b: False, 8)
        except (EOFError, TimeoutError):
            pass
        self.term.close()


def check(nums, n=None):
    """Missing, duplicate and out-of-order numbers in 1..n."""
    n = n or (max(nums) if nums else 0)
    s = set(nums)
    missing = sum(1 for i in range(1, n + 1) if i not in s)
    dup = len(nums) - len(s)
    first, seen = [], set()
    for x in nums:
        if x not in seen:
            seen.add(x)
            first.append(x)
    order = sum(1 for a, b in zip(first, first[1:]) if b < a)
    ok = missing == dup == order == 0
    return ok, f"{n} lines: " + ("all once, in order" if ok else f"{missing} missing, {dup} twice, {order} out of order")


def rand_word(k=12):
    return "".join(random.choice(string.ascii_lowercase + string.digits) for _ in range(k))


def typed_file(tag):
    return f"{L}/typed-{tag}.txt"


def read_lines(path):
    try:
        with open(path) as f:
            return f.read().splitlines()
    except FileNotFoundError:
        return []


def report(test, profile, kind, value, ok=True, note=""):
    RESULTS.append((test, profile, kind, value, ok, note))
    print(f"{'ok ' if ok else 'BAD'} {test:10} {profile:12} {kind:7} {value:>12}  {note}", flush=True)


KINDS = ["ssh", "qsh"]


def t_startup():
    for prof in ["fiber", "wifi", "lte", "bad", "awful", "slow", "udp-blocked"]:
        set_profile(PROFILES[prof])
        for kind in KINDS:
            times = []
            for i in range(3):
                c = Client(kind, f"st-{prof}-{i}")
                try:
                    times.append(c.start())
                except (TimeoutError, EOFError) as e:
                    report("startup", prof, kind, "fail", False, str(e)[-120:])
                finally:
                    c.stop()
            if times:
                times.sort()
                report("startup", prof, kind, f"{times[len(times) // 2] * 1000:.0f} ms", True, f"min {times[0] * 1000:.0f} ms")


def t_echo():
    for prof in ["fiber", "lte", "bad", "awful"]:
        set_profile(PROFILES[prof])
        for kind in ["ssh", "qsh", "qsh-np"]:
            c = Client(kind, f"echo-{prof}")
            c.start()
            times = []
            for _ in range(10):
                c.term.buf = b""
                t0 = time.monotonic()
                c.term.send(b"Q")
                c.term.wait(lambda b: b"Q" in b, 30)
                times.append(time.monotonic() - t0)
                c.term.run_for(0.2)
                c.term.send(b"\x7f")
                c.term.run_for(0.4)
            c.stop()
            times.sort()
            report("echo", prof, kind, f"{times[5] * 1000:.0f} ms", True, f"p90 {times[8] * 1000:.0f} ms")


def t_integrity():
    for prof in ["wifi", "lte", "bad", "awful", "slow"]:
        set_profile(PROFILES[prof])
        for kind in KINDS:
            c = Client(kind, f"integ-{prof}")
            c.start()
            c.term.token = NUMBERED
            t0 = time.monotonic()
            c.term.send(b"awk 'BEGIN{for(i=1;i<=60000;i++) printf \"QN%d.\\n\", i}'; echo END-$((6*7))\r")
            try:
                c.term.wait(lambda b: b"END-42" in b, 600)
                dt = time.monotonic() - t0
                ok, note = check(c.numbers(NUMBERED), 60000)
                report("integrity", prof, kind, f"{dt:.1f} s", ok, note)
            except (TimeoutError, EOFError) as e:
                report("integrity", prof, kind, "fail", False, str(e)[-120:])
            c.stop()


def t_typing():
    for prof in ["lte", "bad", "awful"]:
        set_profile(PROFILES[prof])
        for kind in KINDS:
            c = Client(kind, f"typing-{prof}")
            c.start()
            good = 0
            for i in range(3):
                word = rand_word(300)
                f = typed_file(f"{kind}-{prof}-{i}")
                c.term.type(f"echo {word} >> {f}\r")
                # The result is the file (the screen can show the status overlay).
                end = time.monotonic() + 120
                while not read_lines(f) and time.monotonic() < end:
                    c.term.run_for(0.2)
                c.term.run_for(1)
                good += read_lines(f) == [word]
            c.stop()
            report("typing", prof, kind, f"{good}/3", good == 3, "300 characters, pieces of 1-6 bytes")


def t_outage():
    base = PROFILES["lte"]
    for secs in [3, 8, 20, 45]:
        for kind in KINDS:
            set_profile(base)
            c = Client(kind, f"out{secs}")
            c.start()
            c.term.token = COUNTER
            total = 4 + secs + 10
            c.term.send(f"(i=0; end=$((SECONDS+{total})); while [ $SECONDS -lt $end ]; do i=$((i+1)); echo \"QC$i.\"; sleep 0.05; done; echo QC-END) &\r".encode())
            c.term.run_for(4)
            set_profile({**base, "blackout": True})
            c.term.run_for(secs / 2)
            word = rand_word()
            f = typed_file(f"out-{kind}-{secs}")
            c.term.type(f"echo {word} >> {f}\r")
            c.term.run_for(max(0, secs / 2 - 0.5))
            last = max([n for n, _ in c.term.seen] or [0])
            set_profile(base)
            back = time.monotonic()
            try:
                c.term.wait(lambda b: any(n > last for n, _ in c.term.seen), 120)
                rec = time.monotonic() - back
                c.term.wait(lambda b: b"QC-END" in b, total + 120)
                c.term.run_for(1)
                ok, note = check(c.numbers(COUNTER))
                typed = read_lines(f) == [word]
                report("outage", f"lte+{secs}s", kind, f"{rec * 1000:.0f} ms", ok and typed, note + ("; typed command ran once" if typed else f"; typed command: {read_lines(f)}"))
            except (TimeoutError, EOFError) as e:
                report("outage", f"lte+{secs}s", kind, "fail", False, str(e)[-150:])
            c.stop()


def t_udpblock():
    base = PROFILES["lte"]
    set_profile(base)
    c = Client("qsh", "udpblock")
    c.start()
    c.term.token = COUNTER
    c.term.send(b"(i=0; end=$((SECONDS+75)); while [ $SECONDS -lt $end ]; do i=$((i+1)); echo \"QC$i.\"; sleep 0.05; done; echo QC-END) &\r")
    c.term.run_for(3)
    set_profile({**base, "block_udp": True})
    t0 = time.monotonic()
    last = max([n for n, _ in c.term.seen] or [0])
    c.term.wait(lambda b: any(n > last + 20 for n, _ in c.term.seen), 60)
    report("udpblock", "lte", "qsh", f"{(time.monotonic() - t0) * 1000:.0f} ms", True, "UDP blocked: output continues (SSH tunnel)")
    c.term.run_for(10)
    set_profile(base)
    t1 = time.monotonic()
    # The client logs a move back to UDP.
    log0 = os.path.getsize(f"{L}/c.log")
    moved = None
    while time.monotonic() - t1 < 45 and moved is None:
        c.term.run_for(0.25)
        with open(f"{L}/c.log", "rb") as f:
            f.seek(log0)
            if re.search(rb"moved to direct UDP|new connection \(Direct\)", f.read()):
                moved = time.monotonic() - t1
    c.term.wait(lambda b: b"QC-END" in b, 120)
    c.term.run_for(1)
    ok, note = check(c.numbers(COUNTER))
    report("udpblock", "lte", "qsh", f"{moved:.1f} s" if moved else "no move", ok and moved is not None, "UDP works again: back to UDP; " + note)
    c.stop()


def t_ctrlc():
    for prof in ["lte", "slow"]:
        set_profile(PROFILES[prof])
        for kind in KINDS:
            c = Client(kind, f"ctrlc-{prof}")
            c.start()
            c.term.send(b"yes 0123456789012345678901234567890123456789012345678901234567890123456789\r")
            c.term.run_for(3)
            c.term.buf = b""
            t0 = time.monotonic()
            c.term.send(b"\x03")
            c.term.send(b"echo STOP-$((5*9))\r")
            try:
                c.term.wait(lambda b: b"STOP-45" in b, 300)
                report("ctrlc", prof, kind, f"{(time.monotonic() - t0) * 1000:.0f} ms", True, "until the next command ran")
            except (TimeoutError, EOFError) as e:
                report("ctrlc", prof, kind, "fail", False, str(e)[-100:])
            c.stop()


def t_idle():
    for prof in ["lte", "bad", "awful"]:
        set_profile(PROFILES[prof])
        c = Client("qsh", f"idle-{prof}")
        c.start()
        badges = 0
        end = time.monotonic() + 60
        while time.monotonic() < end:
            c.term.buf = b""
            c.term.run_for(0.5)
            badges += c.term.buf.count("no reply".encode())
        c.stop()
        report("idle", prof, "qsh", f"{badges} badges", True, "60 s without input")


def t_revoke():
    set_profile(PROFILES["wifi"])
    known = f"{L}/chome/.qsh/known"
    if os.path.isdir(known):
        for f in os.listdir(known):
            os.remove(os.path.join(known, f))
    # The user logs in over SSH; the client stores a device token.
    c = Client("qsh", "rv-user")
    c.start()
    end = time.monotonic() + 10
    while not (os.path.isdir(known) and os.listdir(known)) and time.monotonic() < end:
        c.term.run_for(0.2)
    with open(os.path.join(known, os.listdir(known)[0])) as f:
        k = json.load(f)
    # A thief with a copy of the token, and no SSH: a direct link.
    link = f"qsh://{k['addr']}/rv-thief?fp={k['fp']}#{k['token']}"
    env = dict(os.environ, HOME=f"{L}/thief", QSH_LOG=f"{L}/thief.log")
    thief = Term([f"{L}/qsh", link], env)
    try:
        thief.wait(at_prompt, 20)
        report("revoke", "wifi", "qsh", "before", True, "the stolen token works (a token is a credential, as an SSH key)")
    except (TimeoutError, EOFError) as e:
        report("revoke", "wifi", "qsh", "before", False, f"the token did not work: {str(e)[-100:]}")
    thief.run_for(1)
    out = os.popen(f"QSH_HOME={L}/home/.qsh {L}/qsh devices --revoke all").read().strip().splitlines()
    t0 = time.monotonic()
    try:
        thief.wait(lambda b: False, 20)
        ended = None
    except EOFError:
        ended = time.monotonic() - t0
    except TimeoutError:
        ended = None
    tail = plain(thief.buf)[-200:].decode(errors="replace").strip().replace("\r\n", " | ")
    report("revoke", "wifi", "qsh", f"{ended * 1000:.0f} ms" if ended else "still on", ended is not None, f"thief after revoke: {tail[-110:]}")
    thief.close()
    # The token again: refused.
    thief = Term([f"{L}/qsh", link], env)
    try:
        thief.wait(lambda b: False, 15)
        refused = False
    except EOFError:
        refused = b"token" in thief.buf
    except TimeoutError:
        refused = False
    report("revoke", "wifi", "qsh", "refused" if refused else "accepted", refused, plain(thief.buf)[-90:].decode(errors="replace").strip())
    thief.close()
    # The user's session (logged in over SSH) goes on.
    c.term.send(b"echo STILL-$((6*7))\r")
    try:
        c.term.wait(lambda b: b"STILL-42" in b, 10)
        report("revoke", "wifi", "qsh", "user ok", True, "the user's session (SSH login) continues")
    except (TimeoutError, EOFError) as e:
        report("revoke", "wifi", "qsh", "user", False, str(e)[-100:])
    c.stop()
    # The user's next start: the revoked token fails, SSH logs in, a new token.
    c = Client("qsh", "rv-user2")
    try:
        dt = c.start()
        with open(os.path.join(known, os.listdir(known)[0])) as f:
            new = json.load(f)["token"] != k["token"]
        report("revoke", "wifi", "qsh", f"{dt * 1000:.0f} ms", new, "next start of the user: over SSH, new device token" if new else "same token")
    except (TimeoutError, EOFError, IndexError) as e:
        report("revoke", "wifi", "qsh", "fail", False, str(e)[-100:])
    c.stop()


def t_soak(minutes):
    end = time.monotonic() + minutes * 60
    set_profile(PROFILES["lte"])
    cs = {k: Client(k, f"soak-{k}") for k in KINDS}
    for c in cs.values():
        c.start()
        c.term.token = COUNTER
        c.term.send(f"(i=0; end=$((SECONDS+{int(minutes * 60) + 5})); while [ $SECONDS -lt $end ]; do i=$((i+1)); echo \"QC$i.\"; sleep 0.05; done; echo QC-END) &\r".encode())
    words = {k: [] for k in cs}
    events = []

    def run(secs):
        stop_at = time.monotonic() + secs
        while time.monotonic() < stop_at:
            for c in cs.values():
                c.term.pump(0.01)

    next_type = time.monotonic() + 5
    while time.monotonic() < end:
        name = random.choice(["fiber", "wifi", "lte", "bad", "awful", "slow", "udp-blocked", "outage", "outage"])
        if name == "outage":
            p, secs = {**PROFILES["lte"], "blackout": True}, random.uniform(2, 15)
        else:
            p, secs = PROFILES[name], random.uniform(3, 8)
        events.append(f"{name}:{secs:.0f}s")
        set_profile(p)
        stop_at = min(end, time.monotonic() + secs)
        while time.monotonic() < stop_at:
            if time.monotonic() >= next_type:
                next_type = time.monotonic() + random.uniform(6, 14)
                w = rand_word()
                for k, c in cs.items():
                    words[k].append(w)
                    c.term.send(f"echo {w} >> {typed_file('soak-' + k)}\r".encode())
            run(min(0.5, stop_at - time.monotonic()))
    set_profile(PROFILES["lte"])
    for k, c in cs.items():
        try:
            c.term.wait(lambda b: b"QC-END" in b, 180)
            c.term.run_for(1)
            ok, note = check(c.numbers(COUNTER))
            got = read_lines(typed_file("soak-" + k))
            typed = got == words[k]
            report("soak", f"{minutes} min", k, "alive", ok and typed, note + f"; typed {len(got)}/{len(words[k])} commands" + ("" if typed else " (differ)"))
        except (TimeoutError, EOFError) as e:
            report("soak", f"{minutes} min", k, "fail", False, str(e)[-150:])
        c.stop()
    print("soak profiles:", " ".join(events), flush=True)


def main():
    global L, SSH
    p = argparse.ArgumentParser()
    p.add_argument("--lab", default=L)
    p.add_argument("--only", default="startup,echo,integrity,typing,outage,udpblock,ctrlc,idle,revoke,soak")
    p.add_argument("--soak", type=float, default=10)
    a = p.parse_args()
    L = a.lab
    SSH = ["ssh", "-p", "2222", "-i", f"{L}/key", "-o", "StrictHostKeyChecking=no", "-o", "UserKnownHostsFile=/dev/null", "-o", "LogLevel=ERROR"]
    for f in os.listdir(L):
        if f.startswith("typed-") or f.startswith("dump-"):
            os.remove(os.path.join(L, f))
    # A clean lab daemon: no sessions from earlier runs.
    os.system(f"pkill -f '^{L}/qsh daemon'")
    tests = {
        "startup": t_startup,
        "echo": t_echo,
        "integrity": t_integrity,
        "typing": t_typing,
        "outage": t_outage,
        "udpblock": t_udpblock,
        "ctrlc": t_ctrlc,
        "idle": t_idle,
        "revoke": t_revoke,
        "soak": lambda: t_soak(a.soak),
    }
    for name in a.only.split(","):
        try:
            tests[name]()
        except Exception as e:
            report(name, "-", "-", "error", False, f"{type(e).__name__}: {str(e)[-200:]}")
    set_profile({})
    bad = [r for r in RESULTS if not r[4]]
    print(f"\n{len(RESULTS) - len(bad)} ok, {len(bad)} bad")
    return 1 if bad else 0


if __name__ == "__main__":
    sys.exit(main())
