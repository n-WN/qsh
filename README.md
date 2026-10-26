# qsh

A durable remote terminal over QUIC. It replaces `ssh` for interactive work and
fixes the usual problems of `ssh`, `mosh` and `tmux`:

| Need | qsh |
|---|---|
| Network changes (Wi-Fi to LTE, sleep, new IP) | QUIC connection migration, plus lossless resume on a new connection. No key is lost or sent twice |
| Slow or broken link | Typed characters show at once (local echo prediction), also while the link is down. Never at a password prompt |
| UDP blocked | QUIC over UDP and QUIC through the SSH channel race; no timeout to wait for. qsh moves back to UDP when UDP works again |
| Full modern VT | Raw byte stream to your own terminal: truecolor, OSC 8, kitty keyboard, synchronized output, your `TERM` (terminfo is compiled on the server if it is missing) |
| Scrollback | Native scrollback, selection and scrolling of your terminal. On reattach, the history comes back with all attributes, links and soft wraps |
| Kitty graphics protocol | Works (`kitten icat`, yazi, timg). The pixel size of the window is forwarded |
| Clipboard | Both directions, any data type. `xclip`, `xsel`, `wl-copy`, `wl-paste`, `pbcopy`, `pbpaste` on the server use your local clipboard. The commands that Claude Code runs on Ctrl-V return the local image |
| Files without scp | Drop files into the terminal: they go to the server and the remote paths are pasted. `qsh send` / `qsh get` in the session, `qsh cp` on your machine, all on the open connection |
| Multiplexer | Sessions live on the server. Detach, reattach from another device, many sessions per client, live sharing. Clients with different window sizes: the one that types sets the size, the others show a view |
| Install and upgrade | The client installs itself on the server. A new version takes over the running daemon: sessions and clients stay |
| Startup | A known host gets no SSH login: one QUIC handshake, and the prompt shows in 0.25–0.3 s (ssh: 1.3–2 s) |

## How it works

```
 your terminal ── qsh client ══ QUIC (UDP, or UDP-in-SSH) ══ qsh daemon ── pty ── shell
                     │                                          │
                     ├── local clipboard, files, open URL       ├── ring buffer of output (8 MB, byte offsets)
                     └── mirror of the screen (overlay)         ├── mirror of the screen (snapshots)
                                                                └── unix socket ── qsh tools / xclip shims
```

1. `qsh host` runs `ssh host qsh helper`. The helper starts the per-user daemon if
   necessary and prints the UDP port, the SHA-256 of the daemon certificate, a
   one-time token, and its build. SSH does the authentication, as with mosh. If
   the server has no qsh, or an older one, the client installs its own build
   first (see [Install and upgrades](#install-and-upgrades)). A known host
   needs no SSH (see [Fast start](#fast-start)).
2. The client connects with QUIC and pins the certificate. QUIC over UDP and
   QUIC through the same SSH process (a datagram pipe) start at the same time.
   The first that works wins; UDP gets 150 ms more when the tunnel is first.
3. The server sends the pty output as raw bytes with absolute offsets. Your own
   terminal does all the VT work. The client acknowledges offsets.
4. On a new connection the client sends its offset; the server replays the
   missing bytes from the ring buffer. If the bytes are gone (or the client is
   new), the server sends a snapshot from its mirror of the screen instead.

### Fast start

After a login over SSH, the client gets a device token from the daemon. It
keeps the token, the address, the certificate fingerprint and the daemon's
build in `~/.qsh/known/` (mode 0600). The next `qsh host` connects with QUIC
and the token directly: no SSH, so the prompt shows after the QUIC handshake
and one round trip.

- The handshake uses the stored RTT, so a lost packet goes again after about
  3 RTT (not after 0.5 s). If the direct connection has not worked after
  8 RTT (0.5–1.5 s), the SSH login starts too, and the first that works wins;
  the other one stops (its ssh process is killed). If the last login went
  through the SSH tunnel (no UDP path), SSH starts at once.
- On a lossy link one attempt can fail although the link works (QUIC's
  retransmission timers back off). The direct path then tries again with a
  new connection, for up to 60 s, and when SSH fails, the client waits for
  the direct path. In the lab, 6 of 6 starts worked at 25 % loss, where ssh
  failed 1 of 3. (A start that makes a session, `-n` or a command, does not
  try twice: the first attempt may have made it.)
- If the daemon rejects the token, the client deletes it and logs in over
  SSH.
- The token belongs to the daemon (its certificate): a login over SSH to the
  same daemon keeps it, also when the address changed.
- When this client is a newer build than the daemon, it logs in over SSH,
  because that path installs the new build.
- `--transport ssh` and `--no-fast` always log in over SSH. For the security
  trade-off, see [Security](#security).

### Snapshots

The mirror is an `alacritty_terminal` grid that gets the same bytes as your
terminal. qsh turns it back into escape sequences (`src/vt.rs`):

- scrollback (10,000 lines) and screen, with soft wraps, so that your terminal
  reflows the lines on resize as if it had received the original output
- all SGR attributes: truecolor and 256 colors, underline styles (`4:3` curly
  and others), underline color (`58`), strikeout, dim, italic, inverse, hidden
- OSC 8 hyperlinks, wide (CJK, emoji) and combining characters
- cursor position (also the pending-wrap state), shape, visibility and pen
- the alternate screen and, under it, the main screen with its history
- modes: application keys, mouse, bracketed paste, focus events, kitty
  keyboard flags, wrap, insert, the palette (OSC 4/10/11/12) and the title

When you attach, the lines above the cursor go into your terminal's scrollback,
and the session starts on a blank screen. Nothing is drawn twice. A full-screen
program (vim, htop) also gets a SIGWINCH, so it redraws itself at once.

### Roaming and liveness

- Four times a second the client sends a QUIC datagram; the server echoes it.
  Lost pings are not sent again, so the rate matters: on a path with 4 % loss,
  one ping a second gave a false alarm every few minutes. After
  3 s without an echo the client moves to a new UDP socket (new NAT mapping,
  new interface), and at the same time races a new connection (UDP and the SSH
  tunnel). The first path that works continues the session at its offset. No
  output is lost or shown twice.
- After 10 s the client reconnects and does not give up: a fresh UDP
  handshake starts every second (new socket, the last RTT), and the SSH
  tunnel is tried at the same time. After an outage of 45 s, the session
  continued 0.36 s after the network came back (ssh: 16 s).
- On the SSH tunnel, the client tests UDP every 10 s (a few packets) and
  moves the live connection back to UDP when it works.
- A suspended laptop resumes in about 0.2 s after wake-up.

### Status overlay

After 2.5 s without an echo, a badge (`qsh ↻ no reply from the server`) shows
at the top right; when the connection is back, the badge goes away and the
cells under it come back exactly. The overlay is not part of the session: the
client keeps its own mirror of the screen and repaints those cells from it, in
one synchronized update. It is never put inside an escape sequence or a UTF-8
character, it does not end a synchronized update of the program, and it
handles insert mode and the line-drawing character set. Help, the session list
and connection information use the same overlay.

### Local echo

At a long round trip (30 ms or more) or when the link stalls, typed characters
show at once at the cursor, before the server's echo. The server acknowledges
input when the program has had it for 50 ms, after the output that holds the
echo; then every predicted cell must be on the screen. If one is not, all
predictions go away, and the screen shows only what the server sent.
Predictions that wait long are underlined.

Predictions show only where an echo is sure:

- the kernel echoes (line mode with echo), or a shell's line editor reads the
  line (the server reports the pty mode and the foreground program)
- in other programs (vim, a REPL), only after a prediction of the typing burst
  came true
- never when the terminal does not echo in line mode (a password prompt), and
  never while the cursor is hidden (the program draws its own cursor)

Characters, Backspace (not left of where typing started), wide characters,
insertion inside a line and the kitty keyboard protocol are predicted. After
Enter and other keys with an unknown effect, prediction waits until the server
has had the key. `--predict auto|always|never` (default `auto`).

All input goes into a log until the server acknowledges it. On a new
connection the server tells how much input it has; the client sends the rest.
Keys typed while the link is down show as predictions and arrive exactly once.

### Several clients

The client that types (or attached) last sets the pty size; its output goes to
its terminal as it is. Another client with a different window size cannot
show that output directly. It shows a view instead: its terminal switches to
the alternate screen, and the client draws its own mirror of the screen there
(only rows that changed). A larger window shows the pty area and a dotted
fill; a smaller one shows the part with the cursor. Type in a window to make it
set the size again: its main screen comes back, and the lines that scrolled
during the view go into its scrollback.

### Flow control

Everything that waits in a queue of the path goes before your next key or
Ctrl-C. So the congestion controller is BBR: it measures the bottleneck rate
and the round trip and does not fill the queues, and a random loss does not
slow it down (Cubic, quinn's default, does both). Two changes to quinn's BBR:

- It starts with 14.7 KB in flight (RFC 9002), not 240 KB: that was 2 s in
  the queue of a 1 Mbit/s link before BBR knew the rate.
- Its window never falls below 16 packets (19 KB). On an idle connection BBR
  estimates the rate from the pings alone, and its window fell to 4 packets:
  a few lost pings then blocked pings and keys until loss detection, seconds
  on a bad link.

The server keeps unacknowledged output per connection to about two
congestion windows (minimum 32 KB, 0.26 s at 1 Mbit/s). It stops reading the
pty while the active client is more than 1 MB behind, so the program blocks
on write as with ssh and no output is lost. A disconnected client never stops
the session; a client that lags far behind gets a snapshot.

Ctrl-C during a flood: when the server reads an interrupt key from a client
(byte `0x03`, or Ctrl-C in the kitty keyboard encoding), it waits 40 ms for
the program to stop. If that client is then more than 128 KB behind, it gets
the screen (a snapshot) instead of the rest of the output. The prompt comes
back in about 0.3 s at 70 ms RTT (ssh: 1.5 s). The skipped lines are not in
the local scrollback; the screen is correct.

## Install

Build on your machine (Rust 1.88 or later):

```sh
cargo build --release
install -m 755 target/release/qsh /usr/local/bin/   # or ~/.local/bin
```

The server needs nothing: the client installs qsh there.

### Install and upgrades

Each build has an id (a hash of its source) and a time (the newest source
file). The client compares its build with the server's:

- No qsh on the server, or an older build: the client puts its own build into
  `~/.qsh/versions/<id>/` on the server and points `~/.qsh/current` to it. The
  binary comes from the local cache (`~/.qsh/cache`), or is this program (same
  platform), or is built on the server from the source inside the client (the
  server needs a Rust toolchain; the first build takes a few minutes). A build
  made on a server goes into the local cache, for other servers.
- A newer build on the server stays. `--no-install` turns this off.

When the helper of a newer build finds an older daemon, the daemon replaces
its program in place (exec, same process): the ptys, the UDP socket, the local
socket, the output rings, the screens and the clients go to the new program.
The clients see their connection close and connect again at once (about
0.1 s); no output is lost or shown twice, and no session ends.

To build and install on a server from this tree (a server without network
access to crates.io needs the dependencies in its cargo cache):

```sh
scripts/deploy.sh user@host -i ~/.ssh/key
```

On the server, the client runs `~/.qsh/current`, or else `qsh` from `PATH`,
`~/.local/bin`, `~/.cargo/bin` or `~/.qsh/bin`. Use `--server PATH` for another
place (then nothing is installed).

## Use

```sh
qsh host                      # attach to session "main", or make it
qsh -s work host              # attach to session "work"
qsh -n host                   # make a new session
qsh host -- htop              # new session that runs a command
qsh ls host                   # list sessions
qsh --ssh "ssh -i ~/.ssh/key -p 2222" user@host
qsh --port 60100 host         # UDP port for the daemon (default: first free in 60001-60999)
qsh --transport ssh host      # always use the SSH tunnel (or: udp, auto)
qsh -v host                   # tell which network path the connection uses
qsh --download-dir ~/inbox host   # where `qsh send` puts files (default: ~/Downloads)
```

Options that limit what the server can do on your machine: `--no-clipboard`,
`--no-receive`, `--no-open`, `--no-osc52`, `--read-paths ask|always|never`
(see [Security](#security)). `--no-drop` pastes dropped files as local paths.
`--predict auto|always|never` controls [local echo](#local-echo);
`--no-install` keeps the server's qsh as it is; `--no-fast` always logs in
over SSH (see [Fast start](#fast-start)).

Keys (the prefix is `Ctrl-^`, as in mosh):

| Keys | Action |
|---|---|
| `Ctrl-^ .` | quit; the session stays on the server |
| `Ctrl-^ d` | detach |
| `Ctrl-^ c` | new session |
| `Ctrl-^ n` / `Ctrl-^ p` | next / previous session |
| `Ctrl-^ 0`…`9` | select a session |
| `Ctrl-^ w` | list sessions |
| `Ctrl-^ i` | connection: path, RTT, loss, MTU |
| `Ctrl-^ ?` | help |
| `Ctrl-^ Ctrl-^` | send `Ctrl-^` |

Each session has its own scrollback. On a switch the local scrollback is
cleared and the target session's history comes back. Panels go away when you
type.

### Drag and drop

Drop files or folders from Finder (or any file manager) into the terminal. The
terminal pastes their paths; qsh sees that every pasted word is a local file
(under your home folder, `/Volumes`, the temporary folders or `/media`),
sends the files to `~/.qsh/drop/<time>/` on the server and pastes the remote
paths instead, quoted in the same style. A screenshot dropped into Claude Code
on the server arrives as a file that it can read.

The progress shows at the top right. Keys that you type during the upload wait
and follow the paste. `Ctrl-C` stops the upload; the server removes the
partial folder. Paths such as `/etc/hosts` stay text, because they can also
name files on the server. The server removes dropped files after 7 days.

### Inside a session

```sh
qsh send report.pdf logs/     # to the local download folder (~/Downloads)
qsh send --open plot.png      # send and open on the local machine
qsh get                       # a file picker opens on the local machine (macOS)
qsh get ~/Desktop/data.csv    # local file by path (the local user must allow it)
qsh get -C ~/in               # put the files into ~/in (default: current folder)
qsh clip paste -t image/png > shot.png
qsh clip copy < notes.txt
qsh clip list
qsh open https://example.com  # opens in the local browser
qsh open report.pdf           # sends the file and opens it on the local machine
qsh share [--readonly]        # a qsh:// link for another person (1 hour)
qsh share --ttl 600           # the link works for 10 minutes
qsh detach
qsh ls
```

The shims make normal tools use the local clipboard:

```sh
xclip -selection clipboard -t TARGETS -o
xclip -selection clipboard -t image/png -o > img.png   # what Claude Code runs on Ctrl-V
wl-paste --type image/png > img.png
echo hi | wl-copy
```

`xdg-open` and `$BROWSER` open links on the local machine. OSC 52 writes from
remote programs (vim, tmux, Claude Code) also go into the local clipboard.

The shell of a session gets these variables:

| Variable | Value |
|---|---|
| `QSH` | `1` |
| `QSH_SESSION` | the session name |
| `QSH_SOCK` | the daemon socket that the tools use |
| `QSH_BIN` | `~/.qsh/bin`, the folder with the shims; it is first in `PATH` |
| `BROWSER` | the `xdg-open` shim |
| `TERM` | from the client; `xterm-256color` if the server has no terminfo entry and cannot compile the client's |
| `COLORTERM`, `LANG`, `LC_*` | from the client; locales that the server does not have are left out, and `LANG` is then `C.UTF-8` |

A login profile can reset `PATH`. For bash, `PROMPT_COMMAND` puts `QSH_BIN`
back at the first prompt.

### On your machine

```sh
qsh cp report.pdf :           # into the remote shell's current folder
qsh cp a.txt b/ :~/inbox/
qsh cp :/var/log/syslog .
qsh 'qsh://host:port/main?fp=...#token'   # join a shared session
```

`qsh cp` uses the connection of the newest running client (`-c PID` selects
another). It does not log in again. It was faster than scp in a test
(20 MB: 6.8 s against 9.1 s at 75 ms RTT).

## Security

- SSH authenticates the user. The daemon runs as that user and is reachable
  only with a token: a one-time bootstrap token (120 s) from the SSH helper, a
  per-client resume token, or a share token. Tokens are compared in constant
  time.
- TLS 1.3 with a self-signed certificate. The client pins its SHA-256, which
  it gets over SSH.
- Only owner connections (from SSH) can open or make other sessions. A share
  link gives access to one session, optionally read-only.
- The local unix sockets are in `~/.qsh` (mode 0700).
- Fast start keeps a device token in `~/.qsh/known/` on your machine. It
  works like an SSH key without a passphrase: a person who has that file can
  open a shell on the server without SSH, also after you remove your key
  from `authorized_keys`. The server keeps only the SHA-256 of each token. A
  token that is not used for 30 days stops working. Only a login over SSH
  can make a device token, so a stolen token cannot make more tokens. On the
  server, `qsh devices` lists the devices, and `qsh devices --revoke ID` (or
  `all`) removes them. A revoke also ends the clients that the device logged
  in: their connections close and their resume tokens stop working (the
  real user's client then logs in again over SSH). `--no-fast` turns fast
  start off for a client.
- These rules are proven in a formal model (Tamarin): the connection to the
  pinned daemon, single-use bootstrap tokens, device tokens only from SSH
  logins, final revocation, input only with a credential for that session,
  and forward secrecy of what you type. See [formal/README.md](formal/README.md).
- The server can use your local clipboard, send files to your download folder
  and open http(s) links. Disable with `--no-clipboard`, `--no-receive`,
  `--no-open`. Reads of local files by path need your consent in a dialog
  (`--read-paths ask|always|never`); the file picker is always your choice.

## Limitations

- Snapshots (reattach, far lag) do not contain kitty graphics images. Normal
  streaming and resume from the ring buffer keep them.
- A snapshot brings back at most 10,000 lines of history.
- Snapshots do not contain the scroll region (DECSTBM) or origin mode;
  full-screen programs set them again when they redraw. The overlay waits
  while origin mode is on.
- A view (a client that does not set the pty size) does not show kitty
  graphics, and mouse positions are not moved when the view shows a part of
  a larger pty. Terminal replies from that client are dropped (heuristic).
- The file picker and the consent dialog use AppleScript (macOS). On Linux
  clients, give paths and use `--read-paths always`.
- Reconnects through the SSH tunnel use `BatchMode`: they need key or agent
  authentication.
- Prediction works on the cursor row only. Programs that draw their own
  cursor get no prediction.
- A build on the server needs a Rust toolchain and the crates (crates.io, or
  the cargo cache). Daemons from before the in-place upgrade cannot hand over
  their sessions; they stop when they have no sessions for 5 minutes.

## Tests

```sh
cargo test                     # mirror and snapshots, overlay, view mode, prediction, input log, ring buffer, archive, VT scanner, kitty keys, drop detection, build order
tests/e2e/kitty_graphics.py target/release/qsh host local.png remote.png --port P
```

Manual end-to-end checks that were done against a Linux server (75 ms RTT):

- 7 s of dropped inbound UDP: badge at 2.5 s, new socket at 3 s, session on a
  new connection through the SSH tunnel at 5 s; 3 s after UDP worked again the
  connection moved back to UDP. The cells under the badge came back exactly.
- Start with UDP blocked: the prompt shows in 1.7 s (through the tunnel); when
  UDP works again, the live connection moves to UDP (QUIC migration).
- SIGSTOP of the client for 75 s: new connection in 0.17 s, 100/100 lines.
- 200,000 lines of output: all lines in the local scrollback.
- Image on the macOS clipboard: `xclip -t image/png -o` on the server gives
  the same bytes (MD5).
- `kitten icat` on the server: the PNG reaches the local terminal unchanged.
- Detach and reattach from a new terminal: screen and scrollback come back,
  with curly underline and its color, strikeout, truecolor, hyperlinks, wide
  and combining characters. No empty lines between the old local text and the
  session.
- Reattach while vim runs, from a terminal of another size: vim redraws at the
  new size; after `:q` the main screen and its history are there.
- Drop of a 300 kB file into `md5sum `: same MD5 on the server. `Ctrl-C`
  during a 60 MB drop: the upload stops, the partial folder is removed.
- Ctrl-C during an output flood: the prompt comes back in about 0.3 s.
- Session switch with `Ctrl-^ c` / `Ctrl-^ p`: each session shows only its own
  scrollback. When a session's shell exits, the client moves to another one.
- A read-write share link: the guest can type, but `Ctrl-^ c` makes no session.
- Session switch while vim runs: the new session starts clean, and `Ctrl-^ p`
  shows vim again.
- Local echo: 30 ms after typing, the text is on the screen (with
  `--predict never`, only after the round trip).
- Typing during 8 s of dropped UDP: the text showed at once; after the move to
  the SSH tunnel, the command ran exactly once. At a `read -s` prompt nothing
  showed, and the password arrived exactly once.
- Two clients (100×20 and 70×12) on one session: the larger one showed a view
  of the 70×12 pty; after typing in it, it set the size, and the other showed
  a view.
- Daemon upgrade while a counter printed 10 lines per second: all 500 lines,
  in order, once; the client was back after 0.12 s, with no badge.
- A server without qsh: installed from the local cache in 2.5 s, prompt after
  7 s. A server with an older qsh and no binary for its platform here: built
  on the server, then the running daemon took over the new build.
- Fast start with the stored address on a closed port (as when UDP is
  blocked): SSH started after 6 RTT, prompt after 1.8–2.2 s; the token stayed
  and no new device was made. A revoked token: prompt after 1.7 s over SSH,
  and a new token.
- Quit a session and attach again within 2 s: a new session starts (the
  ended one is not used). A client whose connection closes stops holding the
  pty size at once.

### Benchmark against ssh

`tests/bench/bench.py` runs ssh and qsh in a pty, as a terminal does (it
reads all output and answers cursor position queries). It measures the time
to the prompt, key echo, output throughput, and Ctrl-C during a flood:

```sh
tests/bench/bench.py user@host --qsh target/release/qsh --ssh "ssh -i ~/.ssh/key" --port P
```

Results from a macOS client to a Linux server over the internet (about
60 ms RTT, some packet loss), with BBR:

| Test | ssh | qsh |
|---|---|---|
| Startup to prompt (median of 5) | 1016 ms | 223 ms (fast start) |
| Key echo (median of 20) | 66 ms | 0 ms (local echo); 47 ms with `--predict never` |
| `seq 1 2000000` (16.9 MB) | 11.4 s, 1.5 MB/s | 5.6 s, 3.0 MB/s |
| Ctrl-C in a flood, until the next command ran (1 run) | 492 ms | 235 ms |

The network changes from day to day: on another day, with about 4 % loss,
ssh moved 0.6 MB/s and needed 1.5 s for the Ctrl-C (qsh with Cubic: 2.0 MB/s
and 0.3 s). Compare clients in the same run.

### Weak networks

`tests/netlab` is a network lab for one Linux machine (as root). `netem.py`
is a network emulator between two TUN devices (no netem module or network
namespace needed): delay, jitter, loss with bursts, reordering, duplicates,
a rate limit with a queue, outages and blocked UDP. `lab.sh` starts it with
an sshd on the server side, which gives logins a separate `QSH_HOME`, so the
lab does not touch the real user's daemon. `run.py` runs ssh and qsh through
it and checks the results, not only the times: each output line must arrive
once and in order (for qsh: in the session stream that the client got), and
typed commands must run once with the exact text.

```sh
tests/netlab/lab.sh start
tests/netlab/run.py [--only startup,echo,integrity,typing,outage,udpblock,ctrlc,idle,revoke,soak]
tests/netlab/lab.sh stop
```

Profiles (one way; the round trip is twice the delay): fiber 10 ms; wifi
20±10 ms, 1 % loss; lte 40±20 ms, 3 % loss in bursts; bad 80±40 ms, 10 %
loss, 2 % reordering; awful 150±60 ms, 25 % loss; slow 1 Mbit/s with a 2 s
queue; slow-shallow 1 Mbit/s with a 0.2 s queue; dsl 10 Mbit/s with a 0.5 s
queue. Results (ranges are from several runs):

| Test | ssh | qsh |
|---|---|---|
| Startup, lte / bad / awful (median) | 1.2 s / 2.5 s / 12.9 s (1 of 3 failed) | 0.29 s / 0.45–0.5 s / 2.6–3.4 s (6 of 6) |
| Key echo, lte / bad / awful (median) | 0.11–0.12 s / 0.22–0.39 s / 1.2–1.7 s | 0 ms (local echo); without: 82–100 ms / 0.18–0.22 s / 0.4–1.1 s |
| 60,000 lines, lte / bad / awful | 27 s / not done in 10 min / not done in 10 min | 0.7–0.9 s / 1.7–2.5 s / 3.4–9.1 s, each line once, in order |
| 300 typed characters in pieces, lte to awful | ran once | ran once |
| Outage 3 s / 8 s / 20 s / 45 s: output again after | 1.7 s / 0.9 s / 9.9 s / 16.2 s | 0.1 s / 0.1 s / 0.4–0.5 s / 0.2–1.1 s |
| Command typed during the outage | ran once | ran once |
| UDP blocked during a session | – | output on the SSH tunnel after 5.1 s; back on UDP 0.3–0.5 s after UDP works |
| Ctrl-C in a flood, lte / slow (2 s queue) | 7.6 s / 9.9 s | 0.14–0.34 s / 0.37 s |
| 60 s idle: "no reply" badges, bad / awful | – | 0–1 / 2–5, as with Cubic (on awful the link is silent for more than 2.5 s at times) |
| 10 minutes of random profiles and outages up to 15 s | every line once, every command once | every line once, every command once |

Output rate (3.4 MB) and Ctrl-C after 4 s of flood, by link (`--only flow`):

| Link | ssh | qsh with Cubic (before) | qsh with BBR |
|---|---|---|---|
| fiber (no rate limit) | 9.6 MB/s, 27 ms | 2.1 MB/s, 92 ms | 6.5–8.0 MB/s, 38–40 ms |
| dsl | 0.74 MB/s, 0.88 s | 1.12 MB/s, 0.86 s | 1.13–1.16 MB/s, 0.25–0.28 s |
| lte | 0.10 MB/s, 13.1 s | 0.22 MB/s, 2.2 s | 1.96–2.67 MB/s, 0.21–0.22 s |
| slow-shallow | 0.08 MB/s, 9.3 s | 0.12 MB/s, 2.4 s | 0.12 MB/s, 0.31–0.37 s |
| slow | 0.08 MB/s, 8.6 s | 0.12 MB/s, 4.2 s | 0.12 MB/s, 0.37–0.44 s |

On the fiber profile, 3.4 MB is short: BBR starts with 14.7 KB in flight
and needs a few round trips to reach the rate. With 25.9 MB, qsh moved
9.9 MB/s there, as ssh. On the internet path above, qsh moved twice as much
as ssh.

### Formal model

`formal/` has the Tamarin model of the access rules and its results; see
[formal/README.md](formal/README.md).

## Source layout

| File | Content |
|---|---|
| `src/main.rs` | command line, shim dispatch by program name |
| `build.rs`, `src/version.rs` | build id and time, the source inside the binary |
| `src/proto.rs` | frames and messages |
| `src/tls.rs` | certificate, fingerprint pinning, QUIC transport settings |
| `src/files.rs` | streaming archive for file transfer |
| `src/tools.rs` | tools inside a session, and the clipboard shims |
| `src/server/daemon.rs` | QUIC listener, attach and resume, flow control, input acknowledgements, sizes, local socket, in-place upgrade |
| `src/vt.rs` | mirror of the screen (alacritty_terminal) and its serializer |
| `src/server/session.rs` | pty session, ring buffer, backpressure, pty mode |
| `src/server/helper.rs` | `qsh helper` (SSH bootstrap and datagram relay) |
| `src/server/pty.rs` | pty and child process |
| `src/server/devices.rs` | device tokens for fast start |
| `src/client/mod.rs` | connect, liveness, reconnect, input log, session switch, overlay state |
| `src/client/net.rs` | SSH helper process, SSH tunnel socket, path race, path changes |
| `src/client/install.rs` | install and upgrade qsh on the server |
| `src/client/known.rs` | known hosts for fast start |
| `src/client/term.rs` | raw mode, keys, paste |
| `src/client/screen.rs` | output thread: overlay, predictions, view mode, VT scanner |
| `src/client/predict.rs` | local echo prediction |
| `src/client/drop.rs` | drag and drop: path detection, upload, paste rewrite |
| `src/client/rpc.rs` | requests from the server, `qsh cp` |
| `src/client/clipboard.rs` | local clipboard (NSPasteboard on macOS) |
| `tests/bench/bench.py` | benchmark against ssh |
| `tests/netlab/` | weak network lab: emulator, lab setup, tests |
| `formal/` | Tamarin model: connection (`tls.spthy`) and access rules (`qsh.spthy`) |

## Debug

`QSH_LOG=/tmp/qsh.log` writes connection events and startup times;
`QSH_TRACE=quinn=debug` adds quinn tracing. `QSH_DUMP=/tmp/qsh.out` writes
the raw session output that the client gets (and the input that it sends, to
`/tmp/qsh.out.in`). `QSH_CC=cubic|newreno` changes the congestion controller
of the process (default BBR); the daemon sends the output. `kill -USR1` /
`kill -USR2` on the client moves the connection to a new UDP socket / to the
SSH tunnel.
