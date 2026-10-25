#!/usr/bin/env python3
"""End-to-end check: kitty graphics protocol through qsh.

Runs the qsh client on a pty. This script plays the local terminal: it answers
kitty graphics queries (a=q) and DA1, and it collects the image data that
`kitten icat` on the server sends. Then it compares the image with the file.

Usage: kitty_graphics.py QSH_BINARY DEST LOCAL_PNG REMOTE_PNG [qsh options...]
"""
import base64, os, pty, re, select, struct, sys, time, fcntl, termios

qsh, dest, local_png, remote_png = sys.argv[1:5]
extra = sys.argv[5:]
want = open(local_png, "rb").read()

pid, fd = pty.fork()
if pid == 0:
    os.environ["TERM"] = "xterm-kitty"
    os.execv(qsh, [qsh, *extra, dest])
fcntl.ioctl(fd, termios.TIOCSWINSZ, struct.pack("HHHH", 30, 100, 1000, 800))

buf = b""
def pump(sec, until=None):
    global buf
    end = time.time() + sec
    while time.time() < end:
        r, _, _ = select.select([fd], [], [], 0.05)
        if r:
            try:
                d = os.read(fd, 65536)
            except OSError:
                return False
            buf += d
            answer(d)
        if until and until(buf):
            return True
    return False

seen = 0
def answer(_):
    # Reply like kitty would. Look only at new data.
    global seen
    new = buf[seen:]
    for m in re.finditer(rb"\x1b_G([^;\x1b]*)(?:;[^\x1b]*)?\x1b\\", new):
        ctl = dict(kv.split(b"=", 1) for kv in m.group(1).split(b",") if b"=" in kv)
        if ctl.get(b"a") == b"q":
            os.write(fd, b"\x1b_Gi=" + ctl.get(b"i", b"1") + b";OK\x1b\\")
    for _ in re.finditer(rb"\x1b\[c|\x1b\[0c", new):
        os.write(fd, b"\x1b[?62;22c")
    for _ in re.finditer(rb"\x1b\[14t", new):
        os.write(fd, b"\x1b[4;800;1000t")
    for _ in re.finditer(rb"\x1b\[16t", new):
        os.write(fd, b"\x1b[6;26;10t")
    seen = max(seen, len(buf) - 64)

ok = pump(15, lambda b: b"# " in b[-200:])
assert ok, "no prompt: %r" % buf[-300:]
mark = len(buf)
os.write(fd, b"clear; kitten icat --transfer-mode=stream --align=left " + remote_png.encode() + b"; echo ICAT-DONE $?\r")
pump(20, lambda b: b"ICAT-DONE" in b[mark:] and b[mark:].rstrip().endswith(b"#"))
out = buf[mark:]

# Collect transmitted chunks: ESC _ G <ctl> ; <base64> ESC \  (m=1 means more chunks)
data = b""
for m in re.finditer(rb"\x1b_G([^;\x1b]*);([^\x1b]*)\x1b\\", out):
    ctl = dict(kv.split(b"=", 1) for kv in m.group(1).split(b",") if b"=" in kv)
    if ctl.get(b"a") in (b"T", b"t", None) and ctl.get(b"a") != b"q":
        data += m.group(2)
img = base64.b64decode(data) if data else b""
status = re.search(rb"ICAT-DONE (\d+)", out)
print("icat exit:", status.group(1).decode() if status else "?")
print("graphics bytes: %d, decoded %d, expected %d" % (len(data), len(img), len(want)))
# icat may re-encode (f=32 RGBA) or send the PNG as is (f=100).
fmt = re.search(rb"\x1b_G[^;]*f=(\d+)", out)
print("format f=%s" % (fmt.group(1).decode() if fmt else "?"))
if fmt and fmt.group(1) == b"100":
    print("PNG identical:", img == want)
os.write(fd, b"\x1e.")
pump(3)
