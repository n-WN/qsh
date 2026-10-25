# qsh

A durable remote terminal over QUIC. It replaces `ssh` for interactive work and
fixes the usual problems of `ssh`, `mosh` and `tmux`:

| Need | qsh |
|---|---|
| Network changes (Wi-Fi to LTE, sleep, new IP) | QUIC connection migration, plus lossless resume on a new connection |
| UDP blocked | QUIC datagrams go through the SSH channel; qsh moves back to UDP when UDP works again |
| Full modern VT | Raw byte stream to your own terminal: truecolor, OSC 8, kitty keyboard, synchronized output, your `TERM` (terminfo is compiled on the server if it is missing) |
| Scrollback | Native scrollback, selection and scrolling of your terminal. On reattach, the history comes back |
| Kitty graphics protocol | Works (`kitten icat`, yazi, timg). The pixel size of the window is forwarded |
| Clipboard | Both directions, any data type. `xclip`, `xsel`, `wl-copy`, `wl-paste`, `pbcopy`, `pbpaste` on the server use your local clipboard. The commands that Claude Code runs on Ctrl-V return the local image |
| Files without scp | `qsh send` / `qsh get` in the session, `qsh cp` on your machine, all on the open connection |
| Multiplexer | Sessions live on the server. Detach, reattach from another device, many sessions per client, live sharing |

## How it works

```
 your terminal ── qsh client ══ QUIC (UDP, or UDP-in-SSH) ══ qsh daemon ── pty ── shell
                     │                                          │
                     └── local clipboard, files, open URL       ├── ring buffer of output (8 MB, byte offsets)
                                                                ├── vt100 mirror of the screen (snapshots)
                                                                └── unix socket ── qsh tools / xclip shims
```

1. `qsh host` runs `ssh host qsh helper`. The helper starts the per-user daemon if
   necessary and prints the UDP port, the SHA-256 of the daemon certificate and a
   one-time token. SSH does the authentication, as with mosh.
2. The client connects with QUIC and pins the certificate. If no UDP path exists
   in 2.5 s, the same SSH process becomes a datagram pipe for QUIC.
3. The server sends the pty output as raw bytes with absolute offsets. Your own
   terminal does all the VT work. The client acknowledges offsets.
4. On a new connection the client sends its offset; the server replays the
   missing bytes from the ring buffer. If the bytes are gone (or the client is
   new), the server sends a snapshot from its vt100 mirror instead: scrollback,
   screen, cursor, modes, kitty keyboard flags and title.

### Roaming and liveness

- Every second the client sends a QUIC datagram; the server echoes it. After
  3 s without an echo the client moves to a new UDP socket (new NAT mapping,
  new interface). After 8 s it makes a new connection (UDP first, then the SSH
  tunnel) and resumes at its offset. No output is lost or shown twice.
- On the SSH tunnel, the client tests UDP every 30 s and moves the live
  connection back to UDP when it works.
- A suspended laptop resumes in about 0.2 s after wake-up.

### Flow control

The server keeps unacknowledged output per connection to about two congestion
windows (minimum 256 KB). It stops reading the pty while the active client is
more than 1 MB behind, so the program blocks on write as with ssh and no
output is lost. Ctrl-C during a flood returns in about 0.5 s at 75 ms RTT,
because little output is in flight and the kernel discards the pty queue on
SIGINT. A disconnected client never stops the session; a client that lags far
behind gets a snapshot.

## Install

Build on both machines (Rust 1.88 or later):

```sh
cargo build --release
install -m 755 target/release/qsh /usr/local/bin/   # or ~/.local/bin
```

To build and install on a Linux server from this tree:

```sh
scripts/deploy.sh user@host -i ~/.ssh/key
```

On the server, the client looks for `qsh` in `PATH`, `~/.local/bin`,
`~/.cargo/bin` and `~/.qsh/bin`. Use `--server PATH` for another place.

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
(see [Security](#security)).

Keys (the prefix is `Ctrl-^`, as in mosh):

| Keys | Action |
|---|---|
| `Ctrl-^ .` | quit; the session stays on the server |
| `Ctrl-^ d` | detach |
| `Ctrl-^ c` | new session |
| `Ctrl-^ n` / `Ctrl-^ p` | next / previous session |
| `Ctrl-^ 0`…`9` | select a session |
| `Ctrl-^ w` | list sessions (in the window title) |
| `Ctrl-^ ?` | help (in the window title) |
| `Ctrl-^ Ctrl-^` | send `Ctrl-^` |

Each session has its own scrollback. On a switch the local scrollback is
cleared and the target session's history comes back.

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
- A snapshot brings back at most 5000 lines of history (the size of the
  server's vt100 mirror).
- With two clients on one session, the pty size follows the client that
  attached or typed last. A client with a different window size sees that
  layout. Terminal replies from the other client are dropped (heuristic).
- The file picker and the consent dialog use AppleScript (macOS). On Linux
  clients, give paths and use `--read-paths always`.
- Reconnects through the SSH tunnel use `BatchMode`: they need key or agent
  authentication.
- A new server binary does not replace a running daemon. The old daemon stops
  when it has no sessions for 5 minutes.

## Tests

```sh
cargo test                     # ring buffer, snapshots, archive, VT scanner, kitty keys, reply filter, MIME mapping
tests/e2e/kitty_graphics.py target/release/qsh host local.png remote.png --port P
```

Manual end-to-end checks that were done against a Linux server (75 ms RTT):

- 25 s of dropped inbound UDP during output: loss found at 8 s, resume through
  the SSH tunnel, 75/75 lines in order; later the connection moved back to UDP.
- Start with UDP blocked, then unblock: the live connection moved from the
  SSH tunnel to UDP (QUIC migration), no reconnect.
- SIGSTOP of the client for 75 s: new connection in 0.17 s, 100/100 lines.
- 200,000 lines of output: all lines in the local scrollback.
- Image on the macOS clipboard: `xclip -t image/png -o` on the server gives
  the same bytes (MD5).
- `kitten icat` on the server: the PNG reaches the local terminal unchanged.
- Detach and reattach from a new terminal: screen and scrollback come back.
- Ctrl-C during an output flood: the prompt comes back in about 0.5 s.
- Session switch with `Ctrl-^ c` / `Ctrl-^ p`: each session shows only its own
  scrollback. When a session's shell exits, the client moves to another one.
- A read-write share link: the guest can type, but `Ctrl-^ c` makes no session.
- 6 s of dropped UDP: the window title shows `[qsh: no contact for 4s]`; the
  remote title comes back after the outage.

## Source layout

| File | Content |
|---|---|
| `src/main.rs` | command line, shim dispatch by program name |
| `src/proto.rs` | frames and messages |
| `src/tls.rs` | certificate, fingerprint pinning, QUIC transport settings |
| `src/files.rs` | streaming archive for file transfer |
| `src/tools.rs` | tools inside a session, and the clipboard shims |
| `src/server/daemon.rs` | QUIC listener, attach and resume, flow control, local socket |
| `src/server/session.rs` | pty session, ring buffer, vt100 mirror, snapshots |
| `src/server/helper.rs` | `qsh helper` (SSH bootstrap and datagram relay) |
| `src/server/pty.rs` | pty and child process |
| `src/client/mod.rs` | connect, liveness, reconnect, session switch |
| `src/client/net.rs` | SSH helper process, SSH tunnel socket, path changes |
| `src/client/term.rs` | raw mode, keys, output thread, VT scanner |
| `src/client/rpc.rs` | requests from the server, `qsh cp` |
| `src/client/clipboard.rs` | local clipboard (NSPasteboard on macOS) |

## Debug

`QSH_LOG=/tmp/qsh.log` writes connection events; `QSH_TRACE=quinn=debug`
adds quinn tracing. `kill -USR1` / `kill -USR2` on the client moves the
connection to a new UDP socket / to the SSH tunnel.
