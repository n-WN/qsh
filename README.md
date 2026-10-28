# qsh

A remote terminal over QUIC for interactive work. Use it instead of `ssh`, or
instead of `mosh` and `tmux` together:

- The session lives on the server. Detach, attach again from another
  machine, share it.
- The connection survives a new network, a sleeping laptop, a lossy link and
  blocked UDP. No output is lost or shown twice, and no key is lost or sent
  twice.
- Your own terminal draws everything: truecolor, hyperlinks, the kitty
  keyboard and graphics protocols, and its native scrollback.

At a glance, in a network lab and over the internet:

| | ssh | qsh |
|---|---|---|
| Time to the prompt (internet, 60 ms RTT) | 1.0 s | 0.22 s |
| A typed key shows (lossy link, 160 ms RTT) | 0.22–0.73 s | at once (local echo); 0.20 s from the server |
| Output again after a 45 s outage | 14.6–16.2 s | 0.15–0.38 s |
| Ctrl-C in an output flood (1 Mbit/s link) | 9.8 s | 0.43 s |
| Output rate on an LTE-like link (3 % loss) | 0.10 MB/s | 1.6–2.7 MB/s |

## Why qsh

| Need | qsh |
|---|---|
| Network changes (Wi-Fi to LTE, sleep, new IP) | QUIC connection migration, and a lossless resume on a new connection |
| Slow or lossy link | Typed characters show at once (local echo), also while the link is down, but never at a password prompt. Small interactive packets go twice on a lossy path |
| UDP blocked | QUIC over UDP and QUIC through the SSH channel race; there is no timeout to wait for. qsh moves back to UDP when UDP works again |
| Full modern VT | A raw byte stream to your terminal: truecolor, OSC 8, kitty keyboard, synchronized output, your `TERM` (qsh compiles the terminfo entry on the server if it is missing) |
| Scrollback | Your terminal's native scrollback, selection and scrolling. On attach, the history comes back with all attributes, links and soft wraps |
| Kitty graphics protocol | Works (`kitten icat`, yazi, timg). The pixel size of the window goes to the server |
| Clipboard | Both directions, any data type. `xclip`, `xsel`, `wl-copy`, `wl-paste`, `pbcopy` and `pbpaste` on the server use your local clipboard (so Ctrl-V of an image in Claude Code on the server works) |
| Files without scp | Drop files into the terminal: they go to the server, and qsh pastes the remote paths. `qsh send` and `qsh get` in the session, `qsh cp` on your machine, all on the open connection |
| Multiplexer | Many sessions per server, detach and attach, live sharing. With several clients, the one that types sets the size; the others show a view |
| Install and upgrade | The client installs itself on the server. A new version takes over the running daemon; sessions and clients stay |
| Startup | A known host needs no SSH login: one QUIC handshake, then the prompt |

## Install

Build on your machine (Rust 1.88 or later):

```sh
cargo build --release
install -m 755 target/release/qsh /usr/local/bin/   # or ~/.local/bin
```

The server needs nothing: the client installs qsh there (see
[Install and upgrades](#install-and-upgrades)).

## Use

```sh
qsh host                      # attach to session "main", or make it
qsh -s work host              # attach to session "work", or make it
qsh -n host                   # make a new session
qsh host -- htop              # a new session that runs a command
qsh ls host                   # list sessions
qsh --ssh "ssh -i ~/.ssh/key -p 2222" user@host
qsh --port 60100 host         # UDP port of the daemon (default: the first free one in 60001-60999)
qsh --transport ssh host      # always use the SSH tunnel (or: udp, auto)
qsh -v host                   # tell which network path the connection uses
qsh --download-dir ~/inbox host   # where `qsh send` puts files (default: ~/Downloads)
```

More options:

| Option | Effect |
|---|---|
| `--no-clipboard`, `--no-receive`, `--no-open`, `--no-osc52` | the server cannot use the local clipboard, send files, open links, or copy OSC 52 writes |
| `--read-paths ask\|always\|never` | reads of local files by path (`qsh get PATH`; default `ask`) |
| `--no-drop` | paste dropped files as local paths |
| `--predict auto\|always\|never` | [local echo](#input-and-local-echo) (default `auto`) |
| `--no-install` | keep the server's qsh as it is |
| `--no-fast` | always log in over SSH (see [Fast start](#fast-start)) |

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

Each session has its own scrollback. When you switch, qsh clears the local
scrollback and brings back the history of the other session. A panel goes
away when you type.

### Drag and drop

Drop files or folders from Finder (or another file manager) into the
terminal. The terminal pastes their paths. When each pasted word is a local
file (under your home folder, `/Volumes`, `/media` or a temporary folder),
qsh sends the files to `~/.qsh/drop/<time>/` on the server and pastes the
remote paths instead, quoted in the same style. A screenshot that you drop
into Claude Code on the server arrives as a file that it can read.

The progress shows at the top right. Keys that you type during the upload
wait and come after the paste. `Ctrl-C` stops the upload, and the server
removes the partial folder. Paths such as `/etc/hosts` stay text, because
they can also name files on the server. The server removes dropped files
after 7 days.

### Inside a session

```sh
qsh send report.pdf logs/     # to the local download folder (~/Downloads)
qsh send --open plot.png      # send, and open on the local machine
qsh get                       # a file picker opens on the local machine (macOS)
qsh get ~/Desktop/data.csv    # a local file by path (the local user must allow it)
qsh get -C ~/in               # put the files into ~/in (default: the current folder)
qsh clip paste -t image/png > shot.png
qsh clip copy < notes.txt
qsh clip list
qsh open https://example.com  # opens in the local browser
qsh open report.pdf           # sends the file and opens it on the local machine
qsh share [--readonly]        # a qsh:// link for another person (valid 1 hour)
qsh share --ttl 600           # the link is valid 10 minutes
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

`xdg-open` and `$BROWSER` open links on the local machine. OSC 52 writes of
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
| `COLORTERM`, `LANG`, `LC_*` | from the client; qsh leaves out locales that the server does not have, and `LANG` is then `C.UTF-8` |

A login profile can reset `PATH`. For bash, `PROMPT_COMMAND` puts `QSH_BIN`
back at the first prompt.

### On your machine

```sh
qsh cp report.pdf :           # into the current folder of the remote shell
qsh cp a.txt b/ :~/inbox/
qsh cp :/var/log/syslog .
qsh 'qsh://host:port/main?fp=...#token'   # join a shared session
```

`qsh cp` uses the connection of the newest running client (`-c PID` selects
another one). It does not log in again. In a test it was faster than scp
(20 MB at 75 ms RTT: 6.8 s against 9.1 s).

## How it works

```
 your terminal ── qsh client ══ QUIC (UDP, or UDP in SSH) ══ qsh daemon ── pty ── shell
                     │                                         │
                     ├── local clipboard, files, open URL      ├── ring buffer of output (8 MB, byte offsets)
                     └── mirror of the screen (overlay)        ├── mirror of the screen (snapshots)
                                                               └── unix socket ── qsh tools, clipboard shims
```

1. `qsh host` runs `ssh host qsh helper`. The helper starts the daemon of the
   user if necessary. It prints the UDP port, the SHA-256 of the daemon
   certificate, a one-time token and its build. SSH does the
   authentication, as with mosh.
2. The client connects with QUIC and pins the certificate. QUIC over UDP and
   QUIC through the same SSH process (a datagram pipe) start at the same
   time. The first that works wins; when the tunnel is first, UDP gets
   150 ms more.
3. The server sends the pty output as raw bytes with absolute offsets. Your
   terminal does all the VT work. The client acknowledges offsets.
4. On a new connection, the client sends its offset, and the server sends
   the missing bytes again from the ring buffer. If the bytes are gone (or
   the client is new), the server sends a snapshot of its mirror of the
   screen.

### Fast start

After a login over SSH, the client gets a device token from the daemon. It
keeps the token, the address, the certificate fingerprint and the build of
the daemon in `~/.qsh/known/` (mode 0600). The next `qsh host` connects with
QUIC and the token directly: there is no SSH login, and the prompt shows
after the QUIC handshake and one round trip.

- The handshake uses the stored RTT, so a lost packet goes again after about
  3 RTT (not after 0.5 s). QUIC doubles that time for each loss in a row.
  So a fresh handshake (on a new socket) starts every 2 RTT (0.25–1 s), up
  to four at a time, and the first that completes wins.
- If the direct connection does not work after 8 RTT (0.5–1.5 s), the SSH
  login starts too. The first that works wins, and qsh stops the other (it
  kills its ssh process). If the last login used the SSH tunnel (no UDP
  path), SSH starts at once.
- On a lossy link, one attempt can fail although the link works. Then the
  direct path tries again with a new connection, for up to 60 s. When SSH
  fails, the client waits for the direct path. A start that makes a session
  (`-n`, or a command) does not try twice, because the first attempt can
  have made it.
- If the daemon refuses the token, the client deletes it and logs in over
  SSH.
- The token belongs to the daemon (its certificate). A login over SSH to the
  same daemon keeps it, also when the address changed.
- When the client is a newer build than the daemon, it logs in over SSH,
  because that path installs the new build.
- `--transport ssh` and `--no-fast` always log in over SSH. For the security
  trade-off, see [Security](#security).

### Output and snapshots

The daemon keeps the output of each session in a ring buffer (8 MB) with
byte offsets, and it gives the same bytes to a mirror of the screen (an
`alacritty_terminal` grid). From the mirror, qsh makes escape sequences
again (`src/vt.rs`):

- scrollback (10,000 lines) and screen, with soft wraps, so that your
  terminal reflows the lines on a resize as if it had received the original
  output
- all SGR attributes: truecolor and 256 colors, underline styles (`4:3`
  curly and others), underline color (`58`), strikeout, dim, italic,
  inverse, hidden
- OSC 8 hyperlinks, wide (CJK, emoji) and combining characters
- cursor position (also the pending-wrap state), shape, visibility and pen
- the alternate screen and, under it, the main screen with its history
- modes: application keys, mouse, bracketed paste, focus events, kitty
  keyboard flags, wrap, insert, the palette (OSC 4/10/11/12) and the title

When you attach, the lines above the cursor go into the scrollback of your
terminal, and the session starts on a blank screen. Nothing is drawn twice.
A full-screen program (vim, htop) also gets a SIGWINCH, so it redraws at once.

### Input and local echo

All input goes into a log until the server acknowledges it. On a new
connection, the server tells how much input it has, and the client sends the
rest. So keys that you type while the link is down arrive exactly once.

At a long round trip (30 ms or more), or when the link stalls, typed
characters show at once at the cursor, before the echo of the server. The
server acknowledges input when the program has had it for 50 ms, after the
output that holds the echo. Then each predicted cell must be on the screen.
If one is not, all predictions go away, and the screen shows only what the
server sent. Predictions that wait long are underlined.

Predictions show only where an echo is sure:

- the kernel echoes (line mode with echo), or the line editor of a shell
  reads the line (the server reports the pty mode and the foreground program)
- in other programs (vim, a REPL), only after a prediction of the typing
  burst came true
- never when the terminal does not echo in line mode (a password prompt),
  and never while the cursor is hidden (the program draws its own cursor)

qsh predicts characters, Backspace (not left of where typing started), wide
characters, insertion inside a line and the kitty keyboard protocol. After
Enter and other keys with an unknown effect, prediction waits until the
server has had the key.

### Lossy links

A lost packet with a key press or its echo waits for the probe timeout of
QUIC (about srtt + 4 rttvar + 25 ms, doubled for each loss in a row),
because no later packet shows the loss. So on a path that lost a packet in
the last 20 s, small interactive data also goes as copies in QUIC datagrams,
at δ, 3δ, 7δ and 15δ after the original (δ = RTT/8, 8–40 ms), until the
other side acknowledges it:

- Keys: each copy holds all input that the server has not acknowledged, with
  its place in the input count. One copy that arrives fills all gaps.
- Output: a batch of at most 1000 bytes (the echo, with the input
  acknowledgement), with its place in the stream, while at most 2 KB is
  unacknowledged. Bulk output gets no copies: with many packets in flight,
  QUIC finds a loss soon by itself.

The stream stays the reliable path. The receiver uses a copy only at its
exact place, and it skips that part when the stream brings it, so nothing is
applied twice. The server takes input from the stream and from copies under
one lock, in order. On a clean path (and on the SSH tunnel) there are no
copies.

### Flow control and Ctrl-C

All data in a queue of the path goes before your next key or Ctrl-C. So the
congestion controller is BBR: it measures the bottleneck rate and the round
trip and does not fill the queues, and a random loss does not slow it down
(Cubic, the default of quinn, does both). qsh changes two things in the BBR
of quinn:

- It starts with 32 packets (38.4 KB) in flight, not 240 KB. With 240 KB, a
  Ctrl-C right after the start on a 1 Mbit/s link took 4 s. A sweep in the
  lab chose the value (see [docs/ablation.md](docs/ablation.md)).
- Its window never falls below 16 packets (19 KB). On an idle connection BBR
  estimates the rate from the pings alone, and its window fell to 4 packets.
  Then a few lost pings blocked pings and keys until loss detection: seconds
  on a bad link.

The server keeps the unacknowledged output of a connection to about two
congestion windows (minimum 32 KB: 0.26 s at 1 Mbit/s). It stops reading the
pty while the active client is more than 1 MB behind, so the program blocks
on write, as with ssh, and no output is lost. A disconnected client never
stops the session. A client that lags far behind gets a snapshot.

When the server reads an interrupt key from a client (byte `0x03`, or Ctrl-C
in the kitty keyboard encoding), it waits 40 ms for the program to stop. If
that client is then more than 128 KB behind, it gets the screen (a snapshot)
instead of the rest of the output. The skipped lines are not in the local
scrollback; the screen is correct.

### Network changes

- Four times a second, the client sends a QUIC datagram, and the server
  echoes it. A lost ping does not go again, so the rate matters: with one
  ping a second, a lossy link gave six times more false alarms.
- After 2.5 s without an echo, a badge (`qsh ↻ no reply from the server`)
  shows at the top right.
- After 1.5 s with no echo and no packet at all from the server (a lossy
  path still brings some), the client starts the SSH helper in advance. The
  helper waits a minute for use. There is at most one such SSH login a
  minute, and the wait doubles after each one (up to 16 minutes) until a move
  to the tunnel. So a lossy link that is silent at times soon causes none.
- After 3 s without an echo, the client moves to a new UDP socket (a new NAT
  mapping, a new interface). At the same time it races a new connection over
  UDP and the SSH tunnel. When the helper is ready and the path is silent,
  the race starts at 2 s and needs no SSH login. The first path that works
  continues the session at its offset.
- After 10 s, the client reconnects and does not give up. A fresh UDP
  handshake starts every 0.5 s (a new socket, the last RTT; up to six at a
  time), and the SSH tunnel is tried at the same time.
- On the SSH tunnel, the client tests UDP every 10 s (a few packets), and it
  moves the live connection back to UDP when UDP works.
- A suspended laptop continues about 0.2 s after wake-up.

### Status overlay

The badge, the help, the session list and the connection information use an
overlay. The overlay is not part of the session: the client keeps its own
mirror of the screen and repaints the cells under the overlay from it, in one
synchronized update. When the badge goes away, those cells come back exactly.
The client never puts the overlay inside an escape sequence or a UTF-8
character, it does not end a synchronized update of the program, and it
handles insert mode and the line-drawing character set.

### Several clients

The client that typed (or attached) last sets the pty size, and its output
goes to its terminal as it is. Another client with a different window size
cannot show that output directly, so it shows a view: its terminal switches
to the alternate screen, and the client draws its own mirror of the screen
there (only the rows that changed). A larger window shows the pty area and a
dotted fill; a smaller one shows the part with the cursor. Type in a window
to make it set the size again: its main screen comes back, and the lines that
scrolled during the view go into its scrollback.

### Install and upgrades

Each build has an id (a hash of its source) and a time (the newest source
file). The client compares its build with the build on the server:

- No qsh on the server, or an older build: the client puts its own build into
  `~/.qsh/versions/<id>/` on the server and points `~/.qsh/current` to it.
  The binary comes from the local cache (`~/.qsh/cache`), or it is this
  program (same platform), or the server builds it from the source inside the
  client (the server needs a Rust toolchain; the first build takes a few
  minutes). A build made on a server goes into the local cache, for other
  servers.
- A newer build on the server stays. `--no-install` turns this off.

When the helper of a newer build finds an older daemon, the daemon replaces
its program in place (exec, same process). The ptys, the UDP socket, the
local socket, the output rings, the screens and the clients go to the new
program. The clients see their connection close and connect again at once
(about 0.1 s). No output is lost or shown twice, and no session ends.

To build and install on a server from this tree (a server without access to
crates.io needs the dependencies in its cargo cache):

```sh
scripts/deploy.sh user@host -i ~/.ssh/key
```

On the server, the client runs `~/.qsh/current`, or else `qsh` from `PATH`,
`~/.local/bin`, `~/.cargo/bin` or `~/.qsh/bin`. Use `--server PATH` for
another place (then the client installs nothing).

## Measurements

### Over the internet

`tests/bench/bench.py` runs ssh and qsh in a pty, as a terminal does (it
reads all output and answers cursor position queries):

```sh
tests/bench/bench.py user@host --qsh target/release/qsh --ssh "ssh -i ~/.ssh/key" --port P
```

From a macOS client to a Linux server (about 60 ms RTT, some packet loss):

| Test | ssh | qsh |
|---|---|---|
| Time to the prompt (median of 5) | 1016 ms | 223 ms (fast start) |
| Key echo (median of 20) | 66 ms | 0 ms (local echo); 47 ms with `--predict never` |
| `seq 1 2000000` (16.9 MB) | 11.4 s, 1.5 MB/s | 5.6 s, 3.0 MB/s |
| Ctrl-C in a flood, until the next command ran (1 run) | 492 ms | 235 ms |

The network changes from day to day: on another day, with about 4 % loss,
ssh moved 0.6 MB/s and needed 1.5 s for the Ctrl-C. Compare clients in the
same run.

### In a network lab

`tests/netlab` is a network lab for one Linux machine (as root). `netem.py`
emulates the network between two TUN devices (it needs no netem module and
no network namespace): delay, jitter, loss in bursts or in time, reordering,
duplicates, a rate limit with a queue, outages and blocked UDP. `run.py` runs
ssh and qsh through it and checks the results, not only the times: each
output line must arrive once and in order, and each typed command must run
once with the exact text.

Profiles (one way; the round trip is twice the delay): lte 40±20 ms, 3 %
loss in bursts; bad 80±40 ms, 10 % loss, 2 % reordering; awful 150±60 ms,
25 % loss; slow 1 Mbit/s with a 2 s queue; slow-shallow 1 Mbit/s with a
0.2 s queue; dsl 10 Mbit/s with a 0.5 s queue. Results of the current build
(ranges: several runs):

| Test | ssh | qsh |
|---|---|---|
| Time to the prompt, lte / bad / awful (median) | 1.4 s / 2.4 s (1 of 3 failed) / 10.8 s | 0.30 s / 0.50 s / 1.7 s (all started) |
| Key echo, lte / bad / awful (median) | 0.10 s / 0.73 s / 2.1 s (p90 11 s) | at once (local echo); from the server: 85 ms / 0.20 s / 0.52 s (p90 0.65 s) |
| 60,000 lines, lte / bad / awful | 12 s / 507 s / not done in 10 min | 1.1 s / 2.0 s / 3.8 s; each line once, in order |
| 300 characters typed in pieces, lte to awful | ran once (on awful, ssh did not start) | ran once |
| Outage of 3 / 8 / 20 / 45 s: output again after | 1.3 / 0.9 / 9.8 / 14.6 s | 0.10–0.14 / 0.12 / 0.31–0.44 / 0.15–0.38 s |
| A command typed during the outage | ran once | ran once; each counter line once, in order |
| UDP blocked during a session | – | output on the SSH tunnel after 3.1–3.4 s; back on UDP 0.5 s after UDP works |
| Ctrl-C in a flood, lte / slow | 8.1 s / 9.8 s | 0.15 s / 0.43 s |
| 60 s idle: "no reply" badges, lte / bad / awful | – | 0 / 1 / 4 (on awful, the link is silent for more than 2.5 s at times) |
| 10 minutes of random profiles and outages up to 15 s | each line once, each command once | each line once, 57 of 57 commands once |

Output rate (3.4 MB) and Ctrl-C after 4 s of flood:

| Link | ssh | qsh |
|---|---|---|
| fiber (no rate limit) | 9.6 MB/s, 27 ms | 9.6 MB/s, 46 ms |
| dsl | 0.67–0.74 MB/s, 0.88 s | 1.17 MB/s, 0.27 s |
| lte | 0.10 MB/s, 13.1 s (once not done in 300 s) | 1.6–2.7 MB/s, 0.21–0.27 s |
| slow-shallow | 0.08 MB/s, 9.0–9.3 s | 0.12 MB/s, 0.41 s |
| slow | 0.08 MB/s, 8.4–8.6 s | 0.12 MB/s, 0.44 s |

On fiber, 3.4 MB is short: BBR needs a few round trips to reach the rate,
and the result depends on the load of the lab machine. With 25.9 MB, qsh
moved 9.9 MB/s there, as ssh.

### What each mechanism gives

An ablation study turned each mechanism off in turn, in rounds with a random
order, and compared it with the full build (medians; the method, all
results and the statistics are in [docs/ablation.md](docs/ablation.md)):

| Mechanism | Measured on | With | Without |
|---|---|---|---|
| BBR (instead of Cubic) | output rate, lte | 1.88 MB/s | 0.15 MB/s |
| BBR | Ctrl-C in a flood, slow | 0.42 s | 6.1 s |
| Skip to the screen after Ctrl-C | Ctrl-C in a flood, slow | 0.42 s | 8.8 s |
| Output window of 2 congestion windows, minimum 32 KB (instead of 256 KB) | Ctrl-C in a flood, slow | 0.42 s | 2.2 s |
| Start window of 32 packets (instead of 240 KB) | Ctrl-C right after the start, slow | 0.45 s | 4.0 s |
| Start window of 32 packets (instead of 10) | output rate, fiber | 8.6 MB/s | 6.3 MB/s |
| Window floor of 16 packets | Ctrl-C right after the start, slow | 0.45 s | 0.63 s |
| Copies of keys and echoes | echo p90, awful / bad | 1.15 s / 0.37 s | 2.58 s / 0.80 s |
| Parallel handshakes at fast start | handshake p90, lossy links | 1.0 s | 1.7 s |
| SSH helper in advance | UDP blocked: output on the tunnel after | 3.4 s | 5.1 s |
| Fast start | time to the prompt, bad | 0.65 s | 3.9 s |
| Reconnect with handshakes every 0.5 s | output after a 30 s outage | 0.28 s | 1.65 s |
| Four pings a second (instead of one) | idle "no reply" badges, awful in time | 0 a minute | 6.7 a minute |

The copies and the parallel handshakes act on the slow tail; their effect
was clear in A/B runs, but not significant in the confirmatory run (see the
document). The study also removed a mechanism: pings during the first
exchange after the handshake gave no measurable gain.

## Security

- SSH authenticates the user. The daemon runs as that user. A connection
  needs a token: a one-time bootstrap token (valid 120 s) from the SSH
  helper, a resume token of a client, a device token (fast start) or a share
  token. qsh compares tokens in constant time.
- TLS 1.3 with a self-signed certificate. The client pins its SHA-256, which
  it gets over SSH (a fast start uses the stored one).
- Only owner connections (from SSH, or a device token) can open or make
  other sessions. A share link gives access to one session, optionally read
  only.
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
- A formal model (Tamarin) proves these rules: the connection goes to the
  pinned daemon, a bootstrap token works once, only SSH logins make device
  tokens, a revoke is final, input needs a credential for that session, and
  what you type stays secret also if the daemon key is stolen later (forward
  secrecy). See [formal/README.md](formal/README.md).
- The server can use your local clipboard, send files to your download
  folder and open http(s) links. Turn this off with `--no-clipboard`,
  `--no-receive`, `--no-open`. Reads of local files by path need your consent
  in a dialog (`--read-paths ask|always|never`); the file picker is always
  your choice.

## Limitations

- Snapshots (attach, far lag) do not contain kitty graphics images. Normal
  streaming and resume from the ring buffer keep them.
- A snapshot brings back at most 10,000 lines of history.
- Snapshots do not contain the scroll region (DECSTBM) or origin mode;
  full-screen programs set them again when they redraw. The overlay waits
  while origin mode is on.
- A view (a client that does not set the pty size) does not show kitty
  graphics, and qsh does not move mouse positions when the view shows a part
  of a larger pty. qsh drops terminal replies from that client (heuristic).
- The file picker and the consent dialog use AppleScript (macOS). On Linux
  clients, give paths and use `--read-paths always`.
- Reconnects through the SSH tunnel use `BatchMode`: they need key or agent
  authentication.
- Prediction works on the cursor row only. Programs that draw their own
  cursor get no prediction.
- A build on the server needs a Rust toolchain and the crates (from
  crates.io, or from the cargo cache). Daemons from before the in-place
  upgrade cannot hand over their sessions; they stop when they have had no
  sessions for 5 minutes.

## Development

### Tests

```sh
cargo test     # mirror and snapshots, overlay, view mode, prediction, input log, copies, ring buffer, archive, VT scanner, kitty keys, drop detection, build order
tests/e2e/kitty_graphics.py target/release/qsh host local.png remote.png --port P
```

The network lab (Linux, as root):

```sh
tests/netlab/lab.sh start
tests/netlab/run.py [--only startup,echo,integrity,typing,outage,udpblock,ctrlc,idle,flow,revoke,soak] [--kinds ssh,qsh]
tests/netlab/ablate.py --rounds 6 --out r.jsonl && tests/netlab/ablate.py --summary r.jsonl
tests/netlab/lab.sh stop
```

`lab.sh` starts the emulator and an sshd on the server side. The sshd gives
logins a separate `QSH_HOME`, so the lab does not touch the daemon of the
real user. Several labs can run at the same time (`QSH_LAB_NET=98
QSH_LAB_TUN=r lab.sh start /tmp/qshlab2`, then `--lab /tmp/qshlab2 --net
98`).

Manual end-to-end checks against a Linux server (75 ms RTT):

- Dropped inbound UDP for 7 s: the badge showed at 2.5 s, and the session
  continued on a new connection through the SSH tunnel. When UDP worked
  again, the connection moved back to UDP. The cells under the badge came
  back exactly.
- Start with UDP blocked: the prompt showed in 1.7 s (through the tunnel).
  When UDP worked again, the live connection moved to UDP (QUIC migration).
- SIGSTOP of the client for 75 s: a new connection in 0.17 s, 100 of 100
  lines.
- 200,000 lines of output: all lines in the local scrollback.
- An image on the macOS clipboard: `xclip -t image/png -o` on the server gave
  the same bytes (MD5).
- `kitten icat` on the server: the PNG reached the local terminal unchanged.
- Detach, and attach again from a new terminal: the screen and the
  scrollback came back, with curly underline and its color, strikeout,
  truecolor, hyperlinks, wide and combining characters. There were no empty
  lines between the old local text and the session.
- Attach while vim runs, from a terminal of another size: vim redrew at the
  new size; after `:q`, the main screen and its history were there.
- Drop of a 300 kB file into `md5sum `: the same MD5 on the server. `Ctrl-C`
  during a 60 MB drop: the upload stopped, and the server removed the partial
  folder.
- Session switch with `Ctrl-^ c` and `Ctrl-^ p`: each session showed only its
  own scrollback. When the shell of a session ended, the client moved to
  another session.
- A read-write share link: the guest could type, but `Ctrl-^ c` made no
  session.
- Session switch while vim runs: the new session started clean, and
  `Ctrl-^ p` showed vim again.
- Typing during 8 s of dropped UDP: the text showed at once; after the move
  to the SSH tunnel, the command ran exactly once. At a `read -s` prompt
  nothing showed, and the password arrived exactly once.
- Two clients (100×20 and 70×12) on one session: the larger one showed a
  view of the 70×12 pty. After typing in it, it set the size, and the other
  one showed a view.
- Daemon upgrade while a counter printed 10 lines a second: all 500 lines, in
  order, once; the client was back after 0.12 s, with no badge.
- A server without qsh: installed from the local cache in 2.5 s, the prompt
  after 7 s. A server with an older qsh and no binary for its platform here:
  built on the server, then the running daemon took over the new build.
- Fast start with the stored address on a closed port (as when UDP is
  blocked): SSH started after 6 RTT, the prompt after 1.8–2.2 s; the token
  stayed, and no new device was made. A revoked token: the prompt after
  1.7 s over SSH, and a new token.
- Quit a session and attach again within 2 s: a new session started (not the
  ended one). A client whose connection closed stopped holding the pty size
  at once.

### Formal model

`formal/` has the Tamarin model of the connection and of the access rules,
with its results; see [formal/README.md](formal/README.md).

### Source layout

| File | Content |
|---|---|
| `src/main.rs` | command line, shim dispatch by program name |
| `build.rs`, `src/version.rs` | build id and time, the source inside the binary |
| `src/proto.rs` | frames and messages |
| `src/tls.rs` | certificate, fingerprint pinning, QUIC transport settings, BBR changes |
| `src/copies.rs` | copies of small interactive data in datagrams |
| `src/ablate.rs` | switches for the ablation study (`QSH_ABLATE`) |
| `src/files.rs` | streaming archive for file transfer |
| `src/tools.rs` | tools inside a session, and the clipboard shims |
| `src/vt.rs` | mirror of the screen (alacritty_terminal) and its serializer |
| `src/server/daemon.rs` | QUIC listener, attach and resume, flow control, input acknowledgements, sizes, local socket, in-place upgrade |
| `src/server/session.rs` | pty session, ring buffer, backpressure, pty mode |
| `src/server/helper.rs` | `qsh helper` (SSH bootstrap and datagram relay) |
| `src/server/pty.rs` | pty and child process |
| `src/server/devices.rs` | device tokens for fast start |
| `src/client/mod.rs` | connect, liveness, reconnect, input log, session switch, overlay state |
| `src/client/net.rs` | SSH helper process, SSH tunnel socket, path races, path changes |
| `src/client/install.rs` | install and upgrade qsh on the server |
| `src/client/known.rs` | known hosts for fast start |
| `src/client/term.rs` | raw mode, keys, paste |
| `src/client/screen.rs` | output thread: overlay, predictions, view mode, VT scanner |
| `src/client/predict.rs` | local echo prediction |
| `src/client/drop.rs` | drag and drop: path detection, upload, paste rewrite |
| `src/client/rpc.rs` | requests from the server, `qsh cp` |
| `src/client/clipboard.rs` | local clipboard (NSPasteboard on macOS) |
| `tests/bench/bench.py` | benchmark against ssh |
| `tests/netlab/` | network lab: emulator, lab setup, tests, ablation study |
| `formal/` | Tamarin model: connection (`tls.spthy`) and access rules (`qsh.spthy`) |
| `docs/ablation.md` | the ablation study |

### Debug

| Variable or signal | Effect |
|---|---|
| `QSH_LOG=/tmp/qsh.log` | connection events and startup times |
| `QSH_TRACE=quinn=debug` | quinn tracing |
| `QSH_DUMP=/tmp/qsh.out` | the raw session output that the client gets (and the input that it sends, in `/tmp/qsh.out.in`) |
| `QSH_CC=cubic\|newreno` | another congestion controller for this process (the daemon sends the output) |
| `QSH_ABLATE=name,...` | turn mechanisms off for measurements (`tests/netlab/ablate.py`); a wrong name stops qsh at start |
| `kill -USR1` / `kill -USR2` on the client | move the connection to a new UDP socket / to the SSH tunnel |
