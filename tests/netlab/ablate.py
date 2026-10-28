#!/usr/bin/env python3
"""Ablation study: what each mechanism of qsh gives, in the lab of lab.sh.

Each variant turns one mechanism off, in the client and in the lab daemon
(QSH_ABLATE, src/ablate.rs; "nofast" is the client option --no-fast). The
driver runs rounds. In each round, the variants come in random order, and
each variant runs the experiments that can show its effect. So a slow change
of the machine affects all variants alike. Each result is one line of JSON;
--summary makes the tables: median with its 95% bootstrap interval, p90,
failures, and the change against "base" with a Mann-Whitney test (Holm
correction over all comparisons).

Usage: ablate.py [--lab DIR] [--rounds N] [--out FILE] [--only EXP,...] [--variants V,...]
       ablate.py --summary FILE
"""

import argparse
import json
import math
import os
import random
import sys
import time

sys.path.insert(0, os.path.dirname(os.path.abspath(__file__)))
import run as R  # noqa: E402

YES = b"yes 0123456789012345678901234567890123456789012345678901234567890123456789\r"


SEQ = [0]


def name(tag):
    """A new session name each time: a session that an earlier run left
    behind must not answer for this one."""
    SEQ[0] += 1
    return f"ab-{tag}-{os.getpid()}-{SEQ[0]}"


def new(kind, tag):
    c = R.Client(kind, name(tag))
    c.start(90)
    return c


def x_startup(prof):
    """Time to the prompt of a new session (the host is known)."""
    R.set_profile(R.PROFILES[prof])
    c = R.Client("qsh", name("start"))
    try:
        return [c.start(60)], 0, ""
    except (TimeoutError, EOFError) as e:
        return [], 1, str(e)[-80:]
    finally:
        c.stop()


def x_echo(prof):
    """Time until a typed key shows, without prediction; pauses of 0.3-3 s
    between keys, as in typing with thinking."""
    R.set_profile(R.PROFILES[prof])
    c = new("qsh-np", "echo")
    times, fails = [], 0
    try:
        for _ in range(10):
            c.term.run_for(random.uniform(0.3, 3))
            c.term.buf = b""
            t0 = time.monotonic()
            c.term.send(b"Q")
            try:
                c.term.wait(lambda b: b"Q" in b, 30)
                times.append(time.monotonic() - t0)
            except TimeoutError:
                fails += 1
            c.term.run_for(0.1)
            c.term.send(b"\x7f")
    finally:
        c.stop()
    return times, fails, ""


def x_ctrlc(prof, flood):
    """Ctrl-C after `flood` seconds of `yes`: time until the next command runs."""
    R.set_profile(R.PROFILES[prof])
    c = new("qsh", "ctrlc")
    try:
        c.term.send(YES)
        c.term.run_for(flood)
        c.term.buf = b""
        t0 = time.monotonic()
        c.term.send(b"\x03")
        c.term.send(b"echo STOP-$((5*9))\r")
        try:
            c.term.wait(lambda b: b"STOP-45" in b, 120)
            return [time.monotonic() - t0], 0, ""
        except TimeoutError:
            return [], 1, "no prompt in 120 s"
    finally:
        c.stop()


def x_flow(prof, lines):
    """Output rate of `seq 1 LINES` (MB/s; larger is better)."""
    R.set_profile(R.PROFILES[prof])
    c = new("qsh", "flow")
    try:
        c.term.all = 0
        t0 = time.monotonic()
        c.term.send(f"seq 1 {lines}; echo END-$((6*7))\r".encode())
        try:
            c.term.wait(lambda b: b"END-42" in b, 300)
            return [c.term.all / (time.monotonic() - t0) / 1e6], 0, ""
        except TimeoutError:
            return [], 1, "no end in 300 s"
    finally:
        c.stop()


def x_idle(prof, secs):
    """'no reply' badges per minute without input (fewer is better)."""
    R.set_profile(R.PROFILES[prof])
    c = new("qsh", "idle")
    try:
        badges, reconnects = 0, 0
        end = time.monotonic() + secs
        while time.monotonic() < end:
            c.term.buf = b""
            c.term.run_for(0.5)
            badges += c.term.buf.count(b"no reply")
            reconnects += c.term.buf.count("reconnecting".encode())
        return [badges * 60 / secs], 0, f"reconnecting {reconnects}"
    finally:
        c.stop()


def x_outage(secs):
    """lte, then no network for `secs`: time from the network's return until
    output continues; every line of the counter once, in order."""
    base = R.PROFILES["lte"]
    R.set_profile(base)
    c = new("qsh", "outage")
    try:
        c.term.token = R.COUNTER
        total = 3 + secs + 8
        c.term.send(f"(i=0; end=$((SECONDS+{total})); while [ $SECONDS -lt $end ]; do i=$((i+1)); echo \"QC$i.\"; sleep 0.05; done; echo QC-END-$((6*7))) &\r".encode())
        c.term.run_for(3)
        R.set_profile({**base, "blackout": True})
        c.term.run_for(secs)
        last = max([n for n, _ in c.term.seen] or [0])
        R.set_profile(base)
        back = time.monotonic()
        try:
            c.term.wait(lambda b: any(n > last for n, _ in c.term.seen), 120)
            rec = time.monotonic() - back
            c.term.wait(lambda b: b"QC-END-42" in b, total + 120)
            c.term.run_for(0.5)
            ok, note = R.check(c.numbers(R.COUNTER))
            return [rec], 0 if ok else 1, "" if ok else note
        except TimeoutError:
            return [], 1, "no output in 120 s"
    finally:
        c.stop()


def x_udpblock():
    """lte, then UDP is blocked: time until output continues (on the SSH
    tunnel); every line of the counter once, in order."""
    base = R.PROFILES["lte"]
    R.set_profile(base)
    c = new("qsh", "udpblock")
    try:
        c.term.token = R.COUNTER
        c.term.send(b"(i=0; end=$((SECONDS+30)); while [ $SECONDS -lt $end ]; do i=$((i+1)); echo \"QC$i.\"; sleep 0.05; done; echo QC-END-$((6*7))) &\r")
        c.term.run_for(3)
        R.set_profile({**base, "block_udp": True})
        t0 = time.monotonic()
        last = max([n for n, _ in c.term.seen] or [0])
        try:
            c.term.wait(lambda b: any(n > last + 5 for n, _ in c.term.seen), 60)
            dt = time.monotonic() - t0
            c.term.wait(lambda b: b"QC-END-42" in b, 90)
            c.term.run_for(0.5)
            ok, note = R.check(c.numbers(R.COUNTER))
            return [dt], 0 if ok else 1, "" if ok else note
        except TimeoutError:
            return [], 1, "no output in 60 s"
    finally:
        R.set_profile(base)
        c.stop()


def x_integrity(prof):
    """Time for 60,000 numbered lines; each line once, in order."""
    R.set_profile(R.PROFILES[prof])
    c = new("qsh", "integ")
    try:
        c.term.token = R.NUMBERED
        t0 = time.monotonic()
        c.term.send(b"awk 'BEGIN{for(i=1;i<=60000;i++) printf \"QN%d.\\n\", i}'; echo END-$((6*7))\r")
        try:
            c.term.wait(lambda b: b"END-42" in b, 600)
            dt = time.monotonic() - t0
            ok, note = R.check(c.numbers(R.NUMBERED), 60000)
            return [dt], 0 if ok else 1, "" if ok else note
        except TimeoutError:
            return [], 1, "no end in 600 s"
    finally:
        c.stop()


START = ["base", "knownrtt", "fastretry", "fastrace", "floor", "initwin", "bbr", "nofast"]
ECHO = ["base", "floor", "ping4", "bbr", "copies"]
CTRLC = ["base", "initwin", "win32k", "skip", "bbr", "floor"]
FLOW = ["base", "bbr", "win32k", "initwin", "floor"]
IDLE = ["base", "ping4", "floor", "bbr"]
OUTAGE = ["base", "reconnect", "ping4"]
INTEG = ["base", "floor", "bbr", "copies"]

# name: (function, arguments, variants, runs per round, unit, larger is better)
EXPS = {
    "startup/fiber": (x_startup, ("fiber",), ["base", "knownrtt", "nofast"], 2, "s", False),
    "startup/bad": (x_startup, ("bad",), START, 2, "s", False),
    "startup/awful": (x_startup, ("awful",), START, 2, "s", False),
    "startup/awful-t": (x_startup, ("awful-t",), START, 2, "s", False),
    "echo/lte": (x_echo, ("lte",), ECHO, 1, "s", False),
    "echo/bad": (x_echo, ("bad",), ECHO, 1, "s", False),
    "echo/awful": (x_echo, ("awful",), ECHO, 1, "s", False),
    "echo/awful-t": (x_echo, ("awful-t",), ECHO, 1, "s", False),
    "echo/lossy": (x_echo, ("lossy",), ["base", "copies"], 1, "s", False),
    "echo/lte-t": (x_echo, ("lte-t",), ["base", "copies"], 1, "s", False),
    "echo/bad-t": (x_echo, ("bad-t",), ["base", "copies"], 1, "s", False),
    "ctrlc/slow-fresh": (x_ctrlc, ("slow", 0.5), CTRLC, 1, "s", False),
    "ctrlc/slow": (x_ctrlc, ("slow", 3), CTRLC, 1, "s", False),
    "ctrlc/slow-shallow": (x_ctrlc, ("slow-shallow", 3), CTRLC, 1, "s", False),
    "ctrlc/lte": (x_ctrlc, ("lte", 3), CTRLC, 1, "s", False),
    "flow/fiber": (x_flow, ("fiber", 500000), FLOW, 1, "MB/s", True),
    "flow/dsl": (x_flow, ("dsl", 500000), FLOW, 1, "MB/s", True),
    "flow/lte": (x_flow, ("lte", 500000), FLOW, 1, "MB/s", True),
    "flow/slow-shallow": (x_flow, ("slow-shallow", 100000), FLOW, 1, "MB/s", True),
    "idle/bad": (x_idle, ("bad", 45), IDLE, 1, "/min", False),
    "idle/awful": (x_idle, ("awful", 45), IDLE, 1, "/min", False),
    "idle/lte-t": (x_idle, ("lte-t", 45), IDLE, 1, "/min", False),
    "idle/awful-t": (x_idle, ("awful-t", 45), IDLE, 1, "/min", False),
    "outage/8s": (x_outage, (8,), OUTAGE, 1, "s", False),
    "outage/30s": (x_outage, (30,), OUTAGE, 1, "s", False),
    "udpblock/lte": (x_udpblock, (), ["base", "prewarm"], 1, "s", False),
    "integrity/bad": (x_integrity, ("bad",), INTEG, 1, "s", False),
    "integrity/awful": (x_integrity, ("awful",), INTEG, 1, "s", False),
}


def use_variant(v):
    """Client and lab daemon with this variant; the daemon starts again."""
    off = "" if v in ("base", "nofast") else v.replace("+", ",")
    os.environ["QSH_ABLATE"] = off
    R.EXTRA[:] = ["--no-fast"] if v == "nofast" else []
    L = R.L
    with open(f"{L}/key.pub") as f:
        key = f.read().strip()
    env = f'environment="QSH_HOME={L}/home/.qsh"' + (f',environment="QSH_ABLATE={off}"' if off else "")
    with open(f"{L}/authorized_keys", "w") as f:
        f.write(f"{env} {key}\n")
    os.system(f"pkill -f '^{L}/qsh daemon'")
    time.sleep(0.3)
    # The first start logs in over SSH and starts the daemon.
    R.set_profile(R.PROFILES["fiber"])
    c = R.Client("qsh", "ab-warm")
    try:
        c.start(60)
    finally:
        c.stop()


def run(a):
    exps = {k: v for k, v in EXPS.items() if not a.only or any(k.startswith(o) for o in a.only.split(","))}
    variants = sorted({v for e in exps.values() for v in e[2]})
    if a.variants:
        # Given variants run in all chosen experiments (also values such as
        # initwin=76800; join several with "+": floor+skip).
        variants = a.variants.split(",")
        exps = {k: (e[0], e[1], variants, *e[3:]) for k, e in exps.items()}
    out = open(a.out, "a")
    for r in range(a.rounds):
        order = variants[:]
        random.shuffle(order)
        for v in order:
            try:
                use_variant(v)
            except Exception as e:
                print(f"round {r} {v}: no start: {e}", flush=True)
                continue
            names = [k for k, e in exps.items() if v in e[2]]
            random.shuffle(names)
            for name in names:
                fn, args, _, n, unit, _ = exps[name]
                for _ in range(n):
                    t0 = time.time()
                    try:
                        samples, fails, note = fn(*args)
                    except Exception as e:
                        samples, fails, note = [], 1, f"{type(e).__name__}: {str(e)[-100:]}"
                    rec = {"t": t0, "round": r, "exp": name, "variant": v, "samples": samples, "fails": fails, "note": note}
                    out.write(json.dumps(rec) + "\n")
                    out.flush()
                    shown = " ".join(f"{x:.3g}" for x in samples[:10])
                    print(f"r{r} {v:9} {name:20} {shown} {unit}" + (f" fails {fails}" if fails else "") + (f"  {note}" if note else ""), flush=True)
    R.set_profile({})


# Statistics (only the standard library).


def quantile(xs, q):
    xs = sorted(xs)
    if not xs:
        return float("nan")
    i = q * (len(xs) - 1)
    lo = math.floor(i)
    hi = min(lo + 1, len(xs) - 1)
    return xs[lo] + (xs[hi] - xs[lo]) * (i - lo)


def boot_ci(xs, stat, n=2000):
    if len(xs) < 3:
        return float("nan"), float("nan")
    rng = random.Random(1)
    vals = sorted(stat([rng.choice(xs) for _ in xs]) for _ in range(n))
    return vals[int(0.025 * n)], vals[int(0.975 * n) - 1]


def mann_whitney(a, b):
    """Two-sided p value (normal approximation with tie correction)."""
    n1, n2 = len(a), len(b)
    if n1 < 3 or n2 < 3:
        return float("nan")
    allv = sorted([(x, 0) for x in a] + [(x, 1) for x in b])
    ranks = [0.0] * len(allv)
    ties = 0.0
    i = 0
    while i < len(allv):
        j = i
        while j + 1 < len(allv) and allv[j + 1][0] == allv[i][0]:
            j += 1
        for k in range(i, j + 1):
            ranks[k] = (i + j) / 2 + 1
        t = j - i + 1
        ties += t**3 - t
        i = j + 1
    r1 = sum(r for r, (_, g) in zip(ranks, allv) if g == 0)
    u = r1 - n1 * (n1 + 1) / 2
    n = n1 + n2
    var = n1 * n2 / 12 * ((n + 1) - ties / (n * (n - 1)))
    if var <= 0:
        return 1.0
    z = (u - n1 * n2 / 2) / math.sqrt(var)
    return math.erfc(abs(z) / math.sqrt(2))


def fisher(a, na, b, nb):
    """Two-sided Fisher exact test: a of na against b of nb."""
    k, n = a + b, na + nb

    def pr(x):
        return math.comb(na, x) * math.comb(nb, k - x) / math.comb(n, k)

    lo, hi = max(0, k - nb), min(k, na)
    p0 = pr(a)
    return min(1.0, sum(pr(x) for x in range(lo, hi + 1) if pr(x) <= p0 * (1 + 1e-9)))


def fmt(x, unit):
    if x != x:
        return "-"
    if unit == "s":
        return f"{x * 1000:.0f} ms" if x < 1 else f"{x:.2f} s"
    return f"{x:.2f}{' ' if unit != '/min' else ''}{unit}"


def summary(path):
    recs = [json.loads(line) for line in open(path) if line.strip()]
    data = {}
    for r in recs:
        d = data.setdefault(r["exp"], {}).setdefault(r["variant"], {"s": [], "fails": 0, "runs": 0})
        d["s"] += r["samples"]
        d["fails"] += r["fails"]
        d["runs"] += 1
    rows, tests = [], []
    for exp in [e for e in EXPS if e in data] + [e for e in data if e not in EXPS]:
        unit, larger = (EXPS[exp][4], EXPS[exp][5]) if exp in EXPS else ("", False)
        base = data[exp].get("base", {"s": []})["s"]
        for v in sorted(data[exp], key=lambda v: (v != "base", v)):
            d = data[exp][v]
            xs = d["s"]
            med = quantile(xs, 0.5)
            lo, hi = boot_ci(xs, lambda s: quantile(s, 0.5))
            p = float("nan") if v == "base" else mann_whitney(xs, base)
            tail = ""
            if v != "base" and base and xs and not larger:
                # The tail: share of samples above twice the base median.
                cut = 2 * quantile(base, 0.5)
                a, b = sum(x > cut for x in xs) + d["fails"], sum(x > cut for x in base) + data[exp]["base"]["fails"]
                na, nb = len(xs) + d["fails"], len(base) + data[exp]["base"]["fails"]
                tail = f"{a}/{na} vs {b}/{nb} (p {fisher(a, na, b, nb):.2g})"
            change = ""
            if v != "base" and base and xs:
                bm = quantile(base, 0.5)
                if bm:
                    change = f"{(med / bm - 1) * 100:+.0f}%"
            row = [exp, v, len(xs), d["fails"], fmt(med, unit), f"{fmt(lo, unit)}–{fmt(hi, unit)}", fmt(quantile(xs, 0.9), unit), change, p, tail]
            rows.append(row)
            if p == p:
                tests.append((p, len(rows) - 1))
    # Holm: the k-th smallest p value is tested at 0.05 / (m - k).
    m = len(tests)
    sig = set()
    for k, (p, i) in enumerate(sorted(tests)):
        if p > 0.05 / (m - k):
            break
        sig.add(i)
    print("| Experiment | Variant | n | Fails | Median | 95% CI | p90 | Change | p | > 2× base median |")
    print("|---|---|---|---|---|---|---|---|---|---|")
    for i, r in enumerate(rows):
        p = r[8]
        ps = "" if p != p else (f"{p:.1g}" if p < 0.01 else f"{p:.2f}") + (" *" if i in sig else "")
        print("| " + " | ".join(str(x) for x in r[:8]) + f" | {ps} | {r[9]} |")
    print(f"\n* significant after Holm correction over {m} comparisons (family error 0.05)")
    print("Last column: samples (fails count as slow) above twice the median of base, with Fisher's exact test.")


def main():
    p = argparse.ArgumentParser()
    p.add_argument("--lab", default=R.L)
    p.add_argument("--net", type=int, default=99, help="the lab's net (lab.sh QSH_LAB_NET)")
    p.add_argument("--rounds", type=int, default=6)
    p.add_argument("--out", default="ablate.jsonl")
    p.add_argument("--only", default="", help="experiments whose names start with these")
    p.add_argument("--variants", default="")
    p.add_argument("--summary", metavar="FILE")
    a = p.parse_args()
    if a.summary:
        summary(a.summary)
        return 0
    R.L = a.lab
    R.DEST = f"root@10.{a.net}.0.2"
    R.PORT = 56802 + a.net
    R.SSH[:] = ["ssh", "-p", "2222", "-i", f"{R.L}/key", "-o", "StrictHostKeyChecking=no", "-o", "UserKnownHostsFile=/dev/null", "-o", "LogLevel=ERROR"]
    try:
        run(a)
    finally:
        reset_lab(R.L)
    return 0


def reset_lab(L):
    """The lab daemon without ablation for the next user of the lab."""
    with open(f"{L}/key.pub") as f:
        key = f.read().strip()
    with open(f"{L}/authorized_keys", "w") as f:
        f.write(f'environment="QSH_HOME={L}/home/.qsh" {key}\n')
    os.system(f"pkill -f '^{L}/qsh daemon'")
    R.set_profile({})


if __name__ == "__main__":
    sys.exit(main())
