//! Local terminal: raw mode, size, input and output threads.

use std::{
    io::{Read, Write},
    sync::Mutex,
};

use base64::Engine;
use tokio::sync::mpsc;

/// Terminal size: columns, rows, and width and height in pixels (0 if unknown).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Size {
    pub cols: u16,
    pub rows: u16,
    pub xpix: u16,
    pub ypix: u16,
}

impl Size {
    pub fn encode(&self) -> Vec<u8> {
        [self.cols, self.rows, self.xpix, self.ypix].iter().flat_map(|v| v.to_be_bytes()).collect()
    }
}

pub fn size() -> Size {
    for fd in [1, 0, 2] {
        let mut ws: libc::winsize = unsafe { std::mem::zeroed() };
        if unsafe { libc::ioctl(fd, libc::TIOCGWINSZ as _, &mut ws) } == 0 && ws.ws_col > 0 && ws.ws_row > 0 {
            return Size { cols: ws.ws_col, rows: ws.ws_row, xpix: ws.ws_xpixel, ypix: ws.ws_ypixel };
        }
    }
    Size { cols: 80, rows: 24, xpix: 0, ypix: 0 }
}

pub fn is_tty() -> bool {
    unsafe { libc::isatty(0) == 1 && libc::isatty(1) == 1 }
}

static SAVED: Mutex<Option<libc::termios>> = Mutex::new(None);

pub fn enter_raw() -> std::io::Result<()> {
    unsafe {
        let mut t: libc::termios = std::mem::zeroed();
        if libc::tcgetattr(0, &mut t) != 0 {
            return Err(std::io::Error::last_os_error());
        }
        *SAVED.lock().unwrap() = Some(t);
        let mut raw = t;
        libc::cfmakeraw(&mut raw);
        if libc::tcsetattr(0, libc::TCSADRAIN, &raw) != 0 {
            return Err(std::io::Error::last_os_error());
        }
    }
    let prev = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        restore();
        prev(info);
    }));
    Ok(())
}

/// Leave raw mode and undo modes that a remote program can leave on.
pub fn restore() {
    if let Some(t) = SAVED.lock().unwrap().take() {
        // Write to fd 1 directly: the output thread can hold the stdout lock.
        let reset: &[u8] =
            b"\x1b[0m\x1b[?1049l\x1b[?25h\x1b[?1l\x1b>\x1b[?2004l\x1b[?1000l\x1b[?1002l\x1b[?1003l\x1b[?1006l\x1b[<99u\x1b7\x1b[r\x1b8";
        unsafe {
            libc::write(1, reset.as_ptr() as *const libc::c_void, reset.len());
            libc::tcsetattr(0, libc::TCSADRAIN, &t);
        }
    }
}

#[derive(Debug)]
pub enum Event {
    Input(Vec<u8>),
    Resize(Size),
    Detach,
    Quit,
    /// The `Ctrl-^ ?` help request.
    Help,
    /// Session keys after the prefix: c (new), n, p, w (list), 0-9.
    Key(char),
    /// Move the connection to another path (SIGUSR1: new UDP socket, SIGUSR2: SSH tunnel).
    Migrate(super::net::PathKind),
}

/// Escape prefix: Ctrl-^ (as in mosh). `Ctrl-^ .` quits, `Ctrl-^ d` detaches,
/// `Ctrl-^ Ctrl-^` sends one Ctrl-^.
const PREFIX: u8 = 0x1e;

/// A key from the kitty keyboard protocol: `CSI key[:alt...] ; mods[:event] u`.
#[derive(Debug, PartialEq)]
struct CsiKey {
    len: usize,
    key: u32,
    alt: Option<u32>,
    mods: u32,
    release: bool,
}

fn parse_csi_u(b: &[u8]) -> Option<CsiKey> {
    if !b.starts_with(b"\x1b[") {
        return None;
    }
    let end = b.iter().skip(2).position(|c| !(c.is_ascii_digit() || *c == b';' || *c == b':'))? + 2;
    if b[end] != b'u' {
        return None;
    }
    let body = std::str::from_utf8(&b[2..end]).ok()?;
    let mut fields = body.split(';');
    let mut keys = fields.next()?.split(':');
    let key = keys.next()?.parse().ok()?;
    let alt = keys.next().and_then(|k| k.parse().ok());
    let (mods, event) = match fields.next() {
        Some(m) => {
            let mut it = m.split(':');
            (it.next().and_then(|x| x.parse().ok()).unwrap_or(1), it.next().and_then(|x| x.parse::<u32>().ok()).unwrap_or(1))
        }
        None => (1, 1),
    };
    Some(CsiKey { len: end + 1, key, alt, mods, release: event == 3 })
}

/// Ctrl-^ as the kitty protocol reports it: ctrl+shift+6, or ctrl+^.
fn is_prefix_key(k: &CsiKey) -> bool {
    let m = k.mods.saturating_sub(1);
    let ctrl = m & 4 != 0;
    ctrl && ((k.key == 54 && m & 1 != 0) || k.key == 94 || k.alt == Some(94))
}

pub fn spawn_stdin(tx: mpsc::UnboundedSender<Event>) {
    std::thread::spawn(move || {
        let mut stdin = std::io::stdin().lock();
        let mut buf = vec![0u8; 16 * 1024];
        let mut prefix = false;
        loop {
            let n = match stdin.read(&mut buf) {
                Ok(0) => {
                    let _ = tx.send(Event::Quit);
                    return;
                }
                Ok(n) => n,
                Err(e) if e.kind() == std::io::ErrorKind::Interrupted => continue,
                Err(_) => {
                    let _ = tx.send(Event::Quit);
                    return;
                }
            };
            let mut out = Vec::with_capacity(n);
            let data = &buf[..n];
            let mut i = 0;
            while i < data.len() {
                // One key: a plain byte, or a kitty protocol key event.
                let (len, ch, is_prefix, release) = match parse_csi_u(&data[i..]) {
                    Some(k) => {
                        let ch = char::from_u32(k.key).filter(|_| k.mods <= 1).unwrap_or('\0');
                        (k.len, ch, is_prefix_key(&k), k.release)
                    }
                    None => (1, data[i] as char, data[i] == PREFIX, false),
                };
                let raw = &data[i..i + len];
                i += len;
                if release {
                    if !(prefix || is_prefix) {
                        out.extend_from_slice(raw);
                    }
                    continue;
                }
                if prefix {
                    prefix = false;
                    let ev = match ch {
                        '.' => Some(Event::Quit),
                        'd' | '\x04' => Some(Event::Detach),
                        '?' => Some(Event::Help),
                        'c' | 'n' | 'p' | 'w' | '0'..='9' => Some(Event::Key(ch)),
                        _ if is_prefix => {
                            out.extend_from_slice(raw);
                            None
                        }
                        _ => {
                            out.push(PREFIX);
                            out.extend_from_slice(raw);
                            None
                        }
                    };
                    if let Some(ev) = ev {
                        if !out.is_empty() {
                            let _ = tx.send(Event::Input(std::mem::take(&mut out)));
                        }
                        let _ = tx.send(ev);
                    }
                } else if is_prefix {
                    prefix = true;
                } else {
                    out.extend_from_slice(raw);
                }
            }
            if !out.is_empty() && tx.send(Event::Input(out)).is_err() {
                return;
            }
        }
    });
}

pub enum Out {
    Data(Vec<u8>),
    /// Show a status text in the window title; `None` restores the title.
    Status(Option<String>),
    /// Write a message on its own line (only when the screen is in a normal state).
    Message(String),
    Flush(std::sync::mpsc::Sender<()>),
}

pub fn spawn_stdout(mut rx: mpsc::Receiver<Out>, osc52: bool) -> std::thread::JoinHandle<()> {
    std::thread::spawn(move || {
        let mut scan = Scan::default();
        let mut status_shown = false;
        while let Some(m) = rx.blocking_recv() {
            let mut out = std::io::stdout().lock();
            match m {
                Out::Data(d) => {
                    scan.feed(&d, |code, body| {
                        if osc52 && code == b"52" {
                            set_clipboard_from_osc52(body);
                        }
                    });
                    let _ = out.write_all(&d);
                }
                Out::Status(s) => {
                    if scan.ground() {
                        match s {
                            Some(text) => {
                                status_shown = true;
                                let _ = write!(out, "\x1b]2;{text}\x07");
                            }
                            None if status_shown => {
                                status_shown = false;
                                let _ = out.write_all(b"\x1b]2;");
                                let _ = out.write_all(&scan.title);
                                let _ = out.write_all(b"\x07");
                            }
                            None => {}
                        }
                    }
                }
                Out::Message(text) => {
                    if scan.ground() {
                        let _ = write!(out, "\r\n\x1b[0;7m {text} \x1b[0m\r\n");
                    }
                }
                Out::Flush(done) => {
                    let _ = out.flush();
                    let _ = done.send(());
                    continue;
                }
            }
            let _ = out.flush();
        }
    })
}

fn set_clipboard_from_osc52(body: &[u8]) {
    // body: "c;BASE64" (selection list, then data). "?" is a query, which the
    // local terminal answers itself.
    let Some(i) = body.iter().position(|&c| c == b';') else { return };
    let data = &body[i + 1..];
    if data == b"?" {
        return;
    }
    if let Ok(bytes) = base64::engine::general_purpose::STANDARD.decode(data) {
        std::thread::spawn(move || {
            let _ = super::clipboard::set("text/plain", &bytes);
        });
    }
}

/// A small VT scanner. It finds OSC strings (title, OSC 52) and tells whether the
/// stream is between escape sequences, so that injected text cannot break one.
#[derive(Default)]
pub struct Scan {
    st: State,
    osc: Vec<u8>,
    pub title: Vec<u8>,
}

#[derive(Default, PartialEq, Clone, Copy)]
enum State {
    #[default]
    Ground,
    Esc,
    Csi,
    Osc,
    OscEsc,
    Str,
    StrEsc,
}

impl Scan {
    pub fn ground(&self) -> bool {
        self.st == State::Ground
    }

    pub fn feed(&mut self, data: &[u8], mut on_osc: impl FnMut(&[u8], &[u8])) {
        for &b in data {
            self.st = match self.st {
                State::Ground => {
                    if b == 0x1b {
                        State::Esc
                    } else {
                        State::Ground
                    }
                }
                State::Esc => self.after_esc(b),
                State::Csi => match b {
                    0x40..=0x7e | 0x18 | 0x1a => State::Ground,
                    0x1b => State::Esc,
                    _ => State::Csi,
                },
                State::Osc => match b {
                    0x07 => {
                        self.dispatch(&mut on_osc);
                        State::Ground
                    }
                    0x1b => State::OscEsc,
                    0x18 | 0x1a => State::Ground,
                    _ => {
                        if self.osc.len() < 16 << 20 {
                            self.osc.push(b);
                        }
                        State::Osc
                    }
                },
                State::OscEsc => {
                    if b == b'\\' {
                        self.dispatch(&mut on_osc);
                        State::Ground
                    } else {
                        self.after_esc(b)
                    }
                }
                State::Str => match b {
                    0x1b => State::StrEsc,
                    0x18 | 0x1a => State::Ground,
                    _ => State::Str,
                },
                State::StrEsc => match b {
                    b'\\' => State::Ground,
                    0x1b => State::StrEsc,
                    _ => State::Str,
                },
            };
        }
    }

    fn after_esc(&mut self, b: u8) -> State {
        match b {
            b'[' => State::Csi,
            b']' => {
                self.osc.clear();
                State::Osc
            }
            b'P' | b'_' | b'^' | b'X' => State::Str,
            0x1b => State::Esc,
            // ESC + intermediate (e.g. "ESC ( B"): the final byte is harmless in ground.
            _ => State::Ground,
        }
    }

    fn dispatch(&mut self, on_osc: &mut impl FnMut(&[u8], &[u8])) {
        let osc = std::mem::take(&mut self.osc);
        let (code, body) = match osc.iter().position(|&c| c == b';') {
            Some(i) => (&osc[..i], &osc[i + 1..]),
            None => (&osc[..], &[][..]),
        };
        if code == b"0" || code == b"2" {
            self.title = body.to_vec();
        }
        on_osc(code, body);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn kitty_keys() {
        let k = parse_csi_u(b"\x1b[54;6uxyz").unwrap();
        assert_eq!((k.len, k.key, k.mods, k.release), (7, 54, 6, false));
        assert!(is_prefix_key(&k));
        assert!(is_prefix_key(&parse_csi_u(b"\x1b[54:94;6:1u").unwrap()));
        assert!(parse_csi_u(b"\x1b[54;6:3u").unwrap().release);
        let d = parse_csi_u(b"\x1b[100u").unwrap();
        assert_eq!((d.key, d.mods), (100, 1));
        assert!(!is_prefix_key(&d));
        assert!(parse_csi_u(b"\x1b[A").is_none());
    }

    #[test]
    fn scan_title_and_osc52() {
        let mut s = Scan::default();
        let mut got = Vec::new();
        s.feed(b"hi\x1b]2;my ti", |_, _| {});
        assert!(!s.ground());
        s.feed(b"tle\x07\x1b[31m", |_, _| {});
        assert!(s.ground());
        assert_eq!(s.title, b"my title");
        s.feed(b"\x1b]52;c;aGVsbG8=\x1b\\", |c, b| got.push((c.to_vec(), b.to_vec())));
        assert_eq!(got, vec![(b"52".to_vec(), b"c;aGVsbG8=".to_vec())]);
        s.feed(b"\x1b_Gf=100;AAAA", |_, _| {});
        assert!(!s.ground());
        s.feed(b"\x1b\\", |_, _| {});
        assert!(s.ground());
    }
}
