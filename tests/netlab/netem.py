#!/usr/bin/env python3
"""A network emulator between two TUN devices, like netem (Linux, root).

It needs no network namespaces and no netem kernel module:

  client side: tun "qa", address 10.99.0.1/24. Clients connect to 10.99.0.2.
  server side: tun "qb", address 10.99.1.1/24. Servers listen there; they see
               the client as 10.99.1.2.

The kernel sends the client's packets to 10.99.0.2 into qa. The emulator
changes the addresses (10.99.0.1 -> 10.99.1.2, 10.99.0.2 -> 10.99.1.1) and
gives the packet to the kernel on qb, after the delay of the profile. The
replies go the other way. Only IPv4 goes through.

The profile is a JSON file; the emulator reads it again when it changes:

  delay_ms, jitter_ms   one-way delay, and uniform jitter (order is kept)
  loss                  probability that a packet is lost
  burst                 mean length of a loss burst (Gilbert model); 0: independent
  reorder               probability that a packet can pass earlier packets
  dup                   probability of a duplicate
  rate_kbit, queue_ms   link rate, and the longest queue (then tail drop); 0: no limit
  blackout              true: all packets are lost
  block_udp             true: UDP packets are lost (TCP passes)
  up, down              objects with keys for one direction only
                        (up: client to server)

Usage: netem.py PROFILE.json [STATS.json]
"""

import fcntl
import heapq
import json
import os
import random
import select
import signal
import struct
import subprocess
import sys
import time

TUNSETIFF = 0x400454CA
IFF_TUN, IFF_NO_PI = 0x0001, 0x1000
A_HOST, A_PEER = bytes([10, 99, 0, 1]), bytes([10, 99, 0, 2])
B_HOST, B_PEER = bytes([10, 99, 1, 1]), bytes([10, 99, 1, 2])


def tun(name, addr):
    if not os.path.exists("/dev/net/tun"):
        os.makedirs("/dev/net", exist_ok=True)
        os.mknod("/dev/net/tun", 0o666 | 0o020000, os.makedev(10, 200))
    fd = os.open("/dev/net/tun", os.O_RDWR | os.O_NONBLOCK)
    fcntl.ioctl(fd, TUNSETIFF, struct.pack("16sH", name.encode(), IFF_TUN | IFF_NO_PI))
    subprocess.run(["ip", "addr", "add", addr, "dev", name], check=True)
    subprocess.run(["ip", "link", "set", name, "up"], check=True)
    return fd


def fix(c, old, new):
    """Checksum update for changed 16-bit words (RFC 1624)."""
    s = ~c & 0xFFFF
    for i in range(0, len(old), 2):
        s += (~((old[i] << 8) | old[i + 1]) & 0xFFFF) + ((new[i] << 8) | new[i + 1])
    while s >> 16:
        s = (s & 0xFFFF) + (s >> 16)
    return ~s & 0xFFFF


def rewrite(p, src, dst):
    if len(p) < 20 or p[0] >> 4 != 4:
        return None
    p = bytearray(p)
    old, new = bytes(p[12:20]), src + dst
    p[12:20] = new
    p[10:12] = fix((p[10] << 8) | p[11], old, new).to_bytes(2, "big")
    ihl, proto = (p[0] & 15) * 4, p[9]
    first = ((p[6] & 0x1F) << 8 | p[7]) == 0
    at = {6: ihl + 16, 17: ihl + 6}.get(proto) if first else None
    if at is not None and len(p) >= at + 2:
        c = (p[at] << 8) | p[at + 1]
        if not (proto == 17 and c == 0):
            c = fix(c, old, new)
            p[at:at + 2] = (c or 0xFFFF if proto == 17 else c).to_bytes(2, "big")
    return bytes(p)


class Dir:
    def __init__(self, name, out_fd, src, dst):
        self.name, self.out_fd, self.src, self.dst = name, out_fd, src, dst
        self.queue = []
        self.last = 0.0  # delivery time of the last packet (keeps the order)
        self.free = 0.0  # when the link is free (rate limit)
        self.bad = False  # Gilbert state
        self.stats = {"packets": 0, "lost": 0, "queue_drop": 0, "sent": 0}

    def admit(self, p, prof, now, seq):
        self.stats["packets"] += 1
        q = {**prof, **prof.get(self.name, {})}
        loss, burst = q.get("loss", 0), q.get("burst", 0)
        if burst > 1 and 0 < loss < 1:
            if self.bad:
                self.bad = random.random() >= 1 / burst
            else:
                self.bad = random.random() < loss / (burst * (1 - loss))
            lost = self.bad
        else:
            lost = random.random() < loss
        if q.get("blackout") or (q.get("block_udp") and p[9] == 17) or lost:
            self.stats["lost"] += 1
            return
        t = now
        rate = q.get("rate_kbit", 0)
        if rate:
            start = max(now, self.free)
            if start - now > q.get("queue_ms", 1000) / 1000:
                self.stats["queue_drop"] += 1
                return
            self.free = start + len(p) * 8 / (rate * 1000)
            t = self.free
        j = q.get("jitter_ms", 0)
        t += max(0.0, q.get("delay_ms", 0) + random.uniform(-j, j)) / 1000
        if random.random() >= q.get("reorder", 0):
            t = max(t, self.last)
            self.last = t
        heapq.heappush(self.queue, (t, seq, p))
        if random.random() < q.get("dup", 0):
            heapq.heappush(self.queue, (t + 0.0005, seq + 0.5, p))

    def deliver(self, now):
        while self.queue and self.queue[0][0] <= now:
            _, _, p = heapq.heappop(self.queue)
            try:
                os.write(self.out_fd, p)
                self.stats["sent"] += 1
            except OSError:
                pass


def main():
    prof_path = sys.argv[1]
    stats_path = sys.argv[2] if len(sys.argv) > 2 else None
    fa = tun("qa", "10.99.0.1/24")
    fb = tun("qb", "10.99.1.1/24")
    up = Dir("up", fb, B_PEER, B_HOST)
    down = Dir("down", fa, A_PEER, A_HOST)
    by_fd = {fa: up, fb: down}
    signal.signal(signal.SIGTERM, lambda *_: sys.exit(0))
    prof, mtime, checked, written, seq = {}, None, 0.0, 0.0, 0
    print("netem: ready", flush=True)
    while True:
        now = time.monotonic()
        if now - checked > 0.02:
            checked = now
            try:
                m = os.stat(prof_path).st_mtime_ns
                if m != mtime:
                    with open(prof_path) as f:
                        prof = json.load(f)
                    mtime = m
            except (OSError, ValueError):
                pass
        if stats_path and now - written > 0.5:
            written = now
            with open(stats_path + ".new", "w") as f:
                json.dump({"up": up.stats, "down": down.stats, "profile": prof}, f)
            os.replace(stats_path + ".new", stats_path)
        nxt = min([d.queue[0][0] for d in (up, down) if d.queue] + [now + 0.02])
        r, _, _ = select.select([fa, fb], [], [], max(0.0, nxt - now))
        now = time.monotonic()
        for fd in r:
            d = by_fd[fd]
            while True:
                try:
                    p = os.read(fd, 65536)
                except BlockingIOError:
                    break
                p = rewrite(p, d.src, d.dst)
                if p is not None:
                    seq += 1
                    d.admit(p, prof, now, seq)
        up.deliver(now)
        down.deliver(now)


if __name__ == "__main__":
    main()
