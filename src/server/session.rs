//! A session: one pty, its output log (ring buffer with absolute offsets), and a
//! terminal emulator that mirrors the screen so a new client can get a snapshot.
//!
//! Normal output goes to clients as raw bytes, so the client's own terminal does the
//! full VT work (kitty graphics, kitty keyboard, OSC 8, truecolor, native scrollback).
//! The emulator is only used when raw bytes cannot be replayed: a fresh attach, a
//! client that lags too far behind, or a resume point that left the ring.

use std::{
    collections::{BTreeMap, VecDeque},
    num::NonZeroU16,
    os::fd::OwnedFd,
    path::{Path, PathBuf},
    sync::{Arc, Condvar, Mutex},
    time::{Duration, Instant},
};

use anyhow::Result;
use tokio::sync::{mpsc, watch};

use super::pty;

pub const RING_CAP: usize = 8 << 20;
pub const SCROLLBACK: usize = 5000;
/// Stop reading the pty while the active client is this far behind. The program
/// then blocks on write, as with ssh, and on Ctrl-C the kernel discards the pty
/// output queue: no output is lost, and little output is in flight.
pub const PAUSE_LIMIT: u64 = 1 << 20;
/// A paused pty starts again if the active client stops acknowledging.
const PAUSE_MAX: Duration = Duration::from_secs(5);

pub struct Ring {
    buf: VecDeque<u8>,
    base: u64,
    cap: usize,
}

impl Ring {
    pub fn new(cap: usize) -> Self {
        Ring { buf: VecDeque::new(), base: 0, cap }
    }
    pub fn head(&self) -> u64 {
        self.base + self.buf.len() as u64
    }
    pub fn base(&self) -> u64 {
        self.base
    }
    pub fn push(&mut self, d: &[u8]) {
        self.buf.extend(d);
        if self.buf.len() > self.cap {
            let n = self.buf.len() - self.cap;
            self.buf.drain(..n);
            self.base += n as u64;
        }
    }
    pub fn read(&self, from: u64, max: usize) -> Option<Vec<u8>> {
        if from < self.base || from > self.head() {
            return None;
        }
        let start = (from - self.base) as usize;
        let end = (start + max).min(self.buf.len());
        let (a, b) = self.buf.as_slices();
        let mut v = Vec::with_capacity(end - start);
        if start < a.len() {
            v.extend_from_slice(&a[start..end.min(a.len())]);
        }
        if end > a.len() {
            let s = start.max(a.len()) - a.len();
            v.extend_from_slice(&b[s..end - a.len()]);
        }
        Some(v)
    }
}

/// Tracks state that the vt100 crate does not keep: the title and the kitty
/// keyboard protocol flag stacks (one per screen).
#[derive(Default)]
pub struct Tracker {
    pub title: Vec<u8>,
    pub kitty: [Vec<u32>; 2],
}

impl vt100::Callbacks for Tracker {
    fn set_window_title(&mut self, _: &mut vt100::Screen, title: &[u8]) {
        self.title = title.to_vec();
    }

    fn unhandled_csi(&mut self, screen: &mut vt100::Screen, i1: Option<u8>, _i2: Option<u8>, params: &[&[u16]], c: char) {
        if c != 'u' {
            return;
        }
        let st = &mut self.kitty[screen.alternate_screen() as usize];
        let p0 = params.first().and_then(|p| p.first()).copied();
        match i1 {
            Some(b'>') => {
                if st.len() >= 32 {
                    st.remove(0);
                }
                st.push(p0.unwrap_or(0) as u32);
            }
            Some(b'<') => {
                let n = p0.unwrap_or(1).max(1) as usize;
                st.truncate(st.len().saturating_sub(n));
            }
            Some(b'=') => {
                let f = p0.unwrap_or(0) as u32;
                let mode = params.get(1).and_then(|p| p.first()).copied().unwrap_or(1);
                if st.is_empty() {
                    st.push(0);
                }
                let top = st.last_mut().unwrap();
                match mode {
                    2 => *top |= f,
                    3 => *top &= !f,
                    _ => *top = f,
                }
            }
            _ => {}
        }
    }
}

pub struct ClientSlot {
    /// cols, rows, width and height in pixels
    pub size: [u16; 4],
    pub readonly: bool,
    /// Attached with the owner's SSH bootstrap token (not a share link). Only
    /// owners can open or make other sessions on the same connection.
    pub owner: bool,
    pub resume_token: String,
    pub conn: Option<quinn::Connection>,
    /// Writer channel of the current connection (for control frames).
    pub ctl: Option<mpsc::Sender<Vec<u8>>>,
    /// Increases on each new connection for this client.
    pub epoch: u64,
    pub gone_since: Option<Instant>,
    pub server_addr: String,
    /// Output offset that the client acknowledged, and when it last moved.
    pub acked: u64,
    pub ack_at: Instant,
}

pub struct State {
    pub ring: Ring,
    pub parser: vt100::Parser<Tracker>,
    pub clients: BTreeMap<u64, ClientSlot>,
    pub active: Option<u64>,
    pub size: [u16; 4],
}

pub struct Session {
    pub name: String,
    pub command: String,
    pub pid: i32,
    pub created: u64,
    master: OwnedFd,
    input_tx: std::sync::mpsc::Sender<Vec<u8>>,
    pub st: Mutex<State>,
    /// Signalled when an acknowledgement arrives (wakes the paused pty reader).
    pub cv: Condvar,
    pub head: watch::Sender<u64>,
    /// Changes on each acknowledgement (wakes output loops).
    pub ack: watch::Sender<u64>,
    pub exit: watch::Sender<Option<i32>>,
}

fn nz(v: u16) -> NonZeroU16 {
    NonZeroU16::new(v.max(1)).unwrap()
}

impl Session {
    pub fn start(
        name: &str,
        argv: &[String],
        login: bool,
        env: &[(String, String)],
        cwd: &Path,
        size: [u16; 4],
    ) -> Result<Arc<Session>> {
        let p = pty::spawn(argv, login, env, cwd, size)?;
        let pid = p.child.id() as i32;
        let reader = p.master.try_clone()?;
        let writer = p.master.try_clone()?;
        let (input_tx, input_rx) = std::sync::mpsc::channel::<Vec<u8>>();
        let sess = Arc::new(Session {
            name: name.to_string(),
            command: argv.join(" "),
            pid,
            created: crate::paths::now_secs(),
            master: p.master,
            input_tx,
            st: Mutex::new(State {
                ring: Ring::new(RING_CAP),
                parser: vt100::Parser::new_with_callbacks(nz(size[1]), nz(size[0]), SCROLLBACK, Tracker::default()),
                clients: BTreeMap::new(),
                active: None,
                size,
            }),
            cv: Condvar::new(),
            head: watch::Sender::new(0),
            ack: watch::Sender::new(0),
            exit: watch::Sender::new(None),
        });

        std::thread::spawn(move || {
            use std::io::Write;
            let mut f = std::fs::File::from(writer);
            for data in input_rx {
                if f.write_all(&data).is_err() {
                    break;
                }
            }
        });

        let s = sess.clone();
        let mut child = p.child;
        std::thread::spawn(move || {
            let mut buf = vec![0u8; 64 * 1024];
            loop {
                {
                    let mut st = s.st.lock().unwrap();
                    while st.output_blocked() {
                        st = s.cv.wait_timeout(st, Duration::from_millis(100)).unwrap().0;
                    }
                }
                match nix::unistd::read(&reader, &mut buf) {
                    Ok(0) => break,
                    Ok(n) => {
                        let head = {
                            let mut st = s.st.lock().unwrap();
                            st.ring.push(&buf[..n]);
                            st.parser.process(&buf[..n]);
                            st.ring.head()
                        };
                        s.head.send_replace(head);
                    }
                    Err(nix::errno::Errno::EINTR) | Err(nix::errno::Errno::EAGAIN) => continue,
                    Err(_) => break,
                }
            }
            let code = match child.wait() {
                Ok(st) => {
                    use std::os::unix::process::ExitStatusExt;
                    st.code().unwrap_or_else(|| 128 + st.signal().unwrap_or(0))
                }
                Err(_) => -1,
            };
            s.exit.send_replace(Some(code));
        });
        Ok(sess)
    }

    pub fn write_input(&self, data: Vec<u8>) {
        let _ = self.input_tx.send(data);
    }

    pub fn resize(&self, st: &mut State, size: [u16; 4]) {
        if size[0] == 0 || size[1] == 0 || st.size == size {
            return;
        }
        if st.size[..2] != size[..2] {
            st.parser.set_size(nz(size[1]), nz(size[0]));
        }
        st.size = size;
        pty::resize(&self.master, size[0], size[1], size[2], size[3]);
    }

    /// The shell's current directory (falls back to $HOME).
    pub fn cwd(&self) -> PathBuf {
        pty::process_cwd(self.pid).unwrap_or_else(crate::paths::home)
    }

    /// Make `id` the active client: its size drives the pty size.
    pub fn activate(&self, st: &mut State, id: u64) {
        st.active = Some(id);
        if let Some(c) = st.clients.get(&id) {
            let size = c.size;
            self.resize(st, size);
        }
    }

    /// Remove a client. Another client becomes active if needed.
    pub fn remove_client(&self, st: &mut State, id: u64) -> Option<ClientSlot> {
        let slot = st.clients.remove(&id);
        if st.active == Some(id) {
            st.active = None;
            let next = st
                .clients
                .iter()
                .filter(|(_, c)| !c.readonly)
                .max_by_key(|(_, c)| c.conn.is_some())
                .map(|(k, _)| *k);
            if let Some(n) = next {
                self.activate(st, n);
            }
        }
        slot
    }
}

const RESET_MODES: &str = concat!(
    "\x1b\\\x18",              // end any string or sequence in progress
    "\x1b[0m\x1b[?25h\x1b[?1l\x1b>",
    "\x1b[?2004l\x1b[?1000l\x1b[?1002l\x1b[?1003l\x1b[?1005l\x1b[?1006l\x1b[?1015l",
    "\x1b[<99u",               // pop the whole kitty keyboard stack
    "\x1b[r",                  // full scroll region
);

impl State {
    /// True while the active, connected client is too far behind.
    pub fn output_blocked(&self) -> bool {
        let Some(c) = self.active.and_then(|id| self.clients.get(&id)) else { return false };
        c.conn.is_some() && self.ring.head().saturating_sub(c.acked) > PAUSE_LIMIT && c.ack_at.elapsed() < PAUSE_MAX
    }

    /// Escape sequences that bring a terminal to the current screen state.
    /// `full`: also send the scrollback (for a client that has nothing yet).
    pub fn snapshot(&mut self, full: bool) -> Vec<u8> {
        let mut out = String::from(RESET_MODES);
        let screen = self.parser.screen();
        let tracker = self.parser.callbacks();
        let (rows, cols) = screen.size();
        if screen.alternate_screen() {
            if full {
                let mut tmp = vt100::Parser::new(rows, cols, 0);
                *tmp.screen_mut() = screen.clone();
                tmp.process(b"\x1b[?47l");
                out.push_str("\x1b[?1049l");
                flow(&mut out, tmp.screen(), true);
                out.push_str(&tmp.screen().cursor_state_formatted());
                kitty_flags(&mut out, &tracker.kitty[0]);
            }
            out.push_str("\x1b[?1049h\x1b[H\x1b[2J");
            out.push_str("\x1b[<99u");
            out.push_str(&screen.contents_formatted());
            kitty_flags(&mut out, &tracker.kitty[1]);
        } else {
            out.push_str("\x1b[?1049l");
            flow(&mut out, screen, full);
            out.push_str(&screen.cursor_state_formatted());
            out.push_str(&screen.attributes_formatted());
            kitty_flags(&mut out, &tracker.kitty[0]);
        }
        out.push_str(&screen.input_mode_formatted());
        if !tracker.title.is_empty() {
            out.push_str("\x1b]2;");
            out.push_str(&String::from_utf8_lossy(&tracker.title));
            out.push('\x07');
        }
        out.into_bytes()
    }
}

/// Print rows top to bottom with CR LF, so that the terminal scrolls lines into its
/// own scrollback. Soft wraps stay soft wraps.
fn flow(out: &mut String, screen: &vt100::Screen, scrollback: bool) {
    let mut s = String::new();
    let range = if scrollback {
        vt100::capture::BasicFormattedCaptureRange::Full(&mut Default::default())
    } else {
        vt100::capture::BasicFormattedCaptureRange::Visible
    };
    let _ = screen.write_contents_formatted_basic(&mut s, range);
    // Scroll the old screen into the terminal's scrollback, so that no old text
    // stays at the end of new, shorter lines. Then print from a blank screen.
    let rows = screen.size().0.get() as usize;
    out.push_str(&format!("\x1b[0m\x1b[{rows};1H"));
    out.push_str(&"\n".repeat(rows));
    out.push_str("\x1b[H");
    out.push_str(&s.replace('\n', "\r\n"));
    out.push_str("\x1b[0m");
}

fn kitty_flags(out: &mut String, stack: &[u32]) {
    for f in stack {
        out.push_str(&format!("\x1b[>{f}u"));
    }
}

/// True if the input looks like a terminal's reply to a query (not a key press).
/// Replies from clients that are not active are dropped, so that two attached
/// terminals do not both answer.
pub fn looks_like_report(d: &[u8]) -> bool {
    if d.len() < 3 || d[0] != 0x1b {
        return false;
    }
    match d[1] {
        b'P' | b']' | b'_' => true,
        b'[' => {
            if matches!(d[2], b'?' | b'>' | b'=') {
                return true;
            }
            // cursor position report: CSI row ; col R
            let body = &d[2..];
            body.last() == Some(&b'R')
                && body[..body.len() - 1].iter().all(|c| c.is_ascii_digit() || *c == b';')
                && body.contains(&b';')
        }
        _ => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ring_wraps() {
        let mut r = Ring::new(10);
        r.push(b"0123456789");
        r.push(b"abc");
        assert_eq!(r.base(), 3);
        assert_eq!(r.head(), 13);
        assert_eq!(r.read(3, 100).unwrap(), b"3456789abc");
        assert_eq!(r.read(10, 2).unwrap(), b"ab");
        assert!(r.read(2, 1).is_none());
        assert_eq!(r.read(13, 5).unwrap(), b"");
    }

    #[test]
    fn snapshot_reproduces_screen() {
        let mut p = vt100::Parser::new_with_callbacks(nz(5), nz(20), 100, Tracker::default());
        for i in 0..12 {
            p.process(format!("line {i}\r\n").as_bytes());
        }
        p.process(b"\x1b[31mred\x1b[0m $ ");
        p.process(b"\x1b[>1u\x1b]2;hello\x07");
        let mut st = State { ring: Ring::new(10), parser: p, clients: BTreeMap::new(), active: None, size: [20, 5, 0, 0] };
        let snap = st.snapshot(true);
        let mut q = vt100::Parser::new(nz(5), nz(20), 100);
        q.process(b"local prompt $ ");
        q.process(&snap);
        assert_eq!(q.screen().contents(), st.parser.screen().contents());
        assert_eq!(q.screen().cursor_position(), st.parser.screen().cursor_position());
        let n = q.screen().scrollback_len();
        let found = (0..=n).any(|off| {
            q.screen_mut().set_scrollback(off);
            q.screen().contents().contains("line 0")
        });
        assert!(found);
        let s = String::from_utf8_lossy(&snap);
        assert!(s.contains("\x1b[>1u"));
        assert!(s.contains("\x1b]2;hello\x07"));
    }

    #[test]
    fn catch_up_snapshot_leaves_no_old_text() {
        let mut p = vt100::Parser::new_with_callbacks(nz(4), nz(20), 100, Tracker::default());
        p.process(b"a\r\nb\r\n$ ");
        let mut st = State { ring: Ring::new(10), parser: p, clients: BTreeMap::new(), active: None, size: [20, 4, 0, 0] };
        let snap = st.snapshot(false);
        let mut q = vt100::Parser::new(nz(4), nz(20), 100);
        q.process(b"XXXXXXXXXXXXXXXXXXXX\r\nYYYYYYYYYYYYYYYYYYYY\r\nZZZZ\x1b[2;5H");
        q.process(&snap);
        assert_eq!(q.screen().contents(), st.parser.screen().contents());
        assert_eq!(q.screen().cursor_position(), st.parser.screen().cursor_position());
        q.screen_mut().set_scrollback(100);
        assert!(q.screen().contents().contains("XXXX"));
    }

    #[test]
    fn snapshot_alt_screen() {
        let mut p = vt100::Parser::new_with_callbacks(nz(5), nz(20), 100, Tracker::default());
        p.process(b"shell 1\r\nshell 2\r\n$ vim");
        p.process(b"\x1b[?1049h\x1b[H\x1b[2Jeditor text");
        let mut st = State { ring: Ring::new(10), parser: p, clients: BTreeMap::new(), active: None, size: [20, 5, 0, 0] };
        let snap = st.snapshot(true);
        let mut q = vt100::Parser::new(nz(5), nz(20), 100);
        q.process(&snap);
        assert!(q.screen().alternate_screen());
        assert_eq!(q.screen().contents(), st.parser.screen().contents());
        q.process(b"\x1b[?1049l");
        assert!(q.screen().contents().contains("shell 2"));
    }

    #[test]
    fn reports() {
        assert!(looks_like_report(b"\x1b[?62;22c"));
        assert!(looks_like_report(b"\x1b[12;40R"));
        assert!(looks_like_report(b"\x1b]11;rgb:0000/0000/0000\x07"));
        assert!(!looks_like_report(b"\x1b[A"));
        assert!(!looks_like_report(b"a"));
        assert!(!looks_like_report(b"\x1b[97;5u"));
    }
}
