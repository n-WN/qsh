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
   first (see [Install and upgrades](#install-and-upgrades)).
2. The client connects with QUIC and pins the certificate. QUIC over UDP and
   QUIC through the same SSH process (a datagram pipe) start at the same time.
   The first that works wins; UDP gets 150 ms more when the tunnel is first.
3. The server sends the pty output as raw bytes with absolute offsets. Your own
   terminal does all the VT work. The client acknowledges offsets.
4. On a new connection the client sends its offset; the server replays the
   missing bytes from the ring buffer. If the bytes are gone (or the client is
   new), the server sends a snapshot from its mirror of the screen instead.

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

- Every second the client sends a QUIC datagram; the server echoes it. After
  3 s without an echo the client moves to a new UDP socket (new NAT mapping,
  new interface), and at the same time races a new connection (UDP and the SSH
  tunnel). The first path that works continues the session at its offset. No
  output is lost or shown twice. After 10 s the client tries again and again.
- On the SSH tunnel, the client tests UDP every 30 s and moves the live
  connection back to UDP when it works.
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

The server keeps unacknowledged output per connection to about two congestion
windows (minimum 256 KB). It stops reading the pty while the active client is
more than 1 MB behind, so the program blocks on write as with ssh and no
output is lost. Ctrl-C during a flood returns in about 0.5 s at 75 ms RTT,
because little output is in flight and the kernel discards the pty queue on
SIGINT. A disconnected client never stops the session; a client that lags far
behind gets a snapshot.

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
`--no-install` keeps the server's qsh as it is.

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
- Ctrl-C during an output flood: the prompt comes back in about 0.5 s.
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
| `src/client/mod.rs` | connect, liveness, reconnect, input log, session switch, overlay state |
| `src/client/net.rs` | SSH helper process, SSH tunnel socket, path race, path changes |
| `src/client/install.rs` | install and upgrade qsh on the server |
| `src/client/term.rs` | raw mode, keys, paste |
| `src/client/screen.rs` | output thread: overlay, predictions, view mode, VT scanner |
| `src/client/predict.rs` | local echo prediction |
| `src/client/drop.rs` | drag and drop: path detection, upload, paste rewrite |
| `src/client/rpc.rs` | requests from the server, `qsh cp` |
| `src/client/clipboard.rs` | local clipboard (NSPasteboard on macOS) |

## Debug

`QSH_LOG=/tmp/qsh.log` writes connection events; `QSH_TRACE=quinn=debug`
adds quinn tracing. `kill -USR1` / `kill -USR2` on the client moves the
connection to a new UDP socket / to the SSH tunnel.
