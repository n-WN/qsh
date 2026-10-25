//! A session: one pty, its output log (ring buffer with absolute offsets), and a
//! terminal emulator that mirrors the screen so a new client can get a snapshot.
//!
//! Normal output goes to clients as raw bytes, so the client's own terminal does the
//! full VT work (kitty graphics, kitty keyboard, OSC 8, truecolor, native scrollback).
//! The mirror (crate::vt) is only used when raw bytes cannot be replayed: a
//! fresh attach, a client that lags too far behind, or a resume point that left
//! the ring.

use std::{
    collections::{BTreeMap, VecDeque},
    os::fd::OwnedFd,
    path::{Path, PathBuf},
    sync::{Arc, Condvar, Mutex},
    time::{Duration, Instant},
};

use anyhow::Result;
use tokio::sync::{mpsc, watch};

use super::pty;

pub const RING_CAP: usize = 8 << 20;
/// Lines of history in the mirror (what a reattach brings back).
pub const SCROLLBACK: usize = 10_000;
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
    pub mirror: crate::vt::Mirror,
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
                mirror: crate::vt::Mirror::new(size[0], size[1], SCROLLBACK),
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
                            st.mirror.feed(&buf[..n]);
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
        st.mirror.resize(size[0], size[1]);
        st.size = size;
        pty::resize(&self.master, size[0], size[1], size[2], size[3]);
    }

    /// Make a full-screen program redraw itself at the same size: change the
    /// pixel width by one for a moment. The kernel sends SIGWINCH for any
    /// change of the window size structure. The redraw also restores program
    /// state that a snapshot cannot carry (scroll regions, images).
    pub fn nudge(self: &Arc<Self>) {
        let s = self.st.lock().unwrap().size;
        pty::resize(&self.master, s[0], s[1], s[2] ^ 1, s[3]);
        let me = self.clone();
        tokio::spawn(async move {
            tokio::time::sleep(Duration::from_millis(40)).await;
            let s = me.st.lock().unwrap().size;
            pty::resize(&me.master, s[0], s[1], s[2], s[3]);
        });
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

impl State {
    /// True while the active, connected client is too far behind.
    pub fn output_blocked(&self) -> bool {
        let Some(c) = self.active.and_then(|id| self.clients.get(&id)) else { return false };
        c.conn.is_some() && self.ring.head().saturating_sub(c.acked) > PAUSE_LIMIT && c.ack_at.elapsed() < PAUSE_MAX
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
    fn reports() {
        assert!(looks_like_report(b"\x1b[?62;22c"));
        assert!(looks_like_report(b"\x1b[12;40R"));
        assert!(looks_like_report(b"\x1b]11;rgb:0000/0000/0000\x07"));
        assert!(!looks_like_report(b"\x1b[A"));
        assert!(!looks_like_report(b"a"));
        assert!(!looks_like_report(b"\x1b[97;5u"));
    }
}
