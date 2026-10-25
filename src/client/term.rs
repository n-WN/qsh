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
    /// Session keys after the prefix: c (new), n, p, w (list), i (info), 0-9.
    Key(char),
    /// A bracketed paste (without the markers).
    Paste(Vec<u8>),
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

fn find(h: &[u8], n: &[u8]) -> Option<usize> {
    h.windows(n.len()).position(|w| w == n)
}

/// Pastes larger than this go through as normal input (no file detection).
const PASTE_MAX: usize = 1 << 20;

pub fn spawn_stdin(tx: mpsc::UnboundedSender<Event>) {
    use super::drop::{PASTE_END, PASTE_START};
    std::thread::spawn(move || {
        let mut stdin = std::io::stdin().lock();
        let mut buf = vec![0u8; 16 * 1024];
        let mut prefix = false;
        let mut paste: Option<Vec<u8>> = None;
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
            let mut data = buf[..n].to_vec();
            let mut out = Vec::with_capacity(n);
            let mut i = 0;
            loop {
                if let Some(p) = &mut paste {
                    p.extend_from_slice(&data[i..]);
                    if let Some(k) = find(p, PASTE_END) {
                        let content = p[..k].to_vec();
                        let rest = p[k + PASTE_END.len()..].to_vec();
                        paste = None;
                        if !out.is_empty() {
                            let _ = tx.send(Event::Input(std::mem::take(&mut out)));
                        }
                        let _ = tx.send(Event::Paste(content));
                        data = rest;
                        i = 0;
                        continue;
                    }
                    if p.len() > PASTE_MAX {
                        let mut v = PASTE_START.to_vec();
                        v.append(p);
                        paste = None;
                        out.extend_from_slice(&v);
                    }
                    break;
                }
                if i >= data.len() {
                    break;
                }
                if !prefix && data[i..].starts_with(PASTE_START) {
                    paste = Some(Vec::new());
                    i += PASTE_START.len();
                    continue;
                }
                // One key: a plain byte, or a kitty protocol key event.
                let (len, ch, is_prefix, release) = match parse_csi_u(&data[i..]) {
                    Some(k) => {
                        let ch = char::from_u32(k.key).filter(|_| k.mods <= 1).unwrap_or('\0');
                        (k.len, ch, is_prefix_key(&k), k.release)
                    }
                    None => (1, data[i] as char, data[i] == PREFIX, false),
                };
                let raw = data[i..i + len].to_vec();
                i += len;
                if release {
                    if !(prefix || is_prefix) {
                        out.extend_from_slice(&raw);
                    }
                    continue;
                }
                if prefix {
                    prefix = false;
                    let ev = match ch {
                        '.' => Some(Event::Quit),
                        'd' | '\x04' => Some(Event::Detach),
                        '?' => Some(Event::Help),
                        'c' | 'n' | 'p' | 'w' | 'i' | '0'..='9' => Some(Event::Key(ch)),
                        _ if is_prefix => {
                            out.extend_from_slice(&raw);
                            None
                        }
                        _ => {
                            out.push(PREFIX);
                            out.extend_from_slice(&raw);
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
                    out.extend_from_slice(&raw);
                }
            }
            if !out.is_empty() && tx.send(Event::Input(out)).is_err() {
                return;
            }
        }
    });
}

/// A small panel at the top right of the screen. It is not part of the session:
/// when it goes away, the cells under it are repainted from the mirror.
#[derive(Clone, PartialEq, Debug)]
pub struct Overlay {
    /// (text, warning style)
    pub lines: Vec<(String, bool)>,
}

pub enum Out {
    /// Session output.
    Data(Vec<u8>),
    Overlay(Option<Overlay>),
    Resize(u16, u16),
    Flush(std::sync::mpsc::Sender<()>),
}

const SYNC_ON: &[u8] = b"\x1b[?2026h";
const SYNC_OFF: &[u8] = b"\x1b[?2026l";

#[derive(Clone, Copy)]
struct Region {
    col: usize,
    width: usize,
    height: usize,
}

fn erase(m: &crate::vt::Mirror, r: Region) -> String {
    m.repaint(0..r.height, r.col, r.width)
}

fn fit(s: &str, width: usize) -> String {
    use unicode_width::UnicodeWidthChar;
    let mut out = String::new();
    let mut w = 0;
    for c in s.chars() {
        let cw = c.width().unwrap_or(0);
        if w + cw > width {
            break;
        }
        out.push(c);
        w += cw;
    }
    out.push_str(&" ".repeat(width - w));
    out
}

fn draw(m: &crate::vt::Mirror, o: &Overlay) -> (String, Option<Region>) {
    use unicode_width::UnicodeWidthStr;
    let (cols, rows) = m.size();
    let inner = o.lines.iter().map(|(l, _)| l.width()).max().unwrap_or(0);
    let width = (inner + 2).min(cols);
    let height = o.lines.len().min(rows);
    // In origin mode the positions would be wrong: the overlay waits.
    if width < 3 || height == 0 || m.origin_mode() {
        return (String::new(), None);
    }
    // One empty column at the right edge, when there is space.
    let col = cols - width - usize::from(cols > width);
    let mut out = format!("\x1b[?25l\x1b]8;;\x1b\\{}", m.neutral());
    for (i, (text, warn)) in o.lines.iter().take(height).enumerate() {
        let style = if *warn { "\x1b[0;1;38;5;232;48;5;214m" } else { "\x1b[0;38;5;255;48;5;238m" };
        out.push_str(&format!("\x1b[{};{}H{style} {} \x1b[0m", i + 1, col + 1, fit(text, width - 2)));
    }
    out.push_str(&m.cursor_restore());
    (out, Some(Region { col, width, height }))
}

pub fn spawn_stdout(mut rx: mpsc::Receiver<Out>, osc52: bool, cols: u16, rows: u16) -> std::thread::JoinHandle<()> {
    std::thread::spawn(move || {
        let mut scan = Scan::default();
        let mut mirror = crate::vt::Mirror::new(cols, rows, 0);
        let mut overlay: Option<Overlay> = None;
        let mut drawn: Option<Region> = None;
        while let Some(m) = rx.blocking_recv() {
            let mut buf: Vec<u8> = Vec::new();
            // Our own synchronized update, so that erase, output and redraw of the
            // overlay show as one frame. Not when the program has an open one:
            // our end mark would end it early.
            let mut sync = false;
            match m {
                Out::Data(d) => {
                    // The overlay is only drawn between escape sequences and
                    // characters, so the stream is at such a point here too.
                    if (drawn.is_some() || overlay.is_some()) && !scan.sync {
                        buf.extend_from_slice(SYNC_ON);
                        sync = true;
                    }
                    if let Some(r) = drawn.take() {
                        buf.extend_from_slice(erase(&mirror, r).as_bytes());
                    }
                    scan.feed(&d, |code, body| {
                        if osc52 && code == b"52" {
                            set_clipboard_from_osc52(body);
                        }
                    });
                    mirror.feed(&d);
                    buf.extend_from_slice(&d);
                    if let Some(o) = overlay.as_ref().filter(|_| scan.ground()) {
                        let (s, r) = draw(&mirror, o);
                        buf.extend_from_slice(s.as_bytes());
                        drawn = r;
                    }
                }
                Out::Overlay(o) => {
                    if o == overlay {
                        continue;
                    }
                    overlay = o;
                    if scan.ground() {
                        if !scan.sync {
                            buf.extend_from_slice(SYNC_ON);
                            sync = true;
                        }
                        if let Some(r) = drawn.take() {
                            buf.extend_from_slice(erase(&mirror, r).as_bytes());
                        }
                        if let Some(o) = &overlay {
                            let (s, r) = draw(&mirror, o);
                            buf.extend_from_slice(s.as_bytes());
                            drawn = r;
                        }
                    }
                }
                Out::Resize(c, r) => {
                    // The terminal has already moved the cells; a program
                    // redraws after the resize.
                    mirror.resize(c, r);
                    drawn = None;
                    if let Some(o) = overlay.as_ref().filter(|_| scan.ground()) {
                        let (s, r) = draw(&mirror, o);
                        buf.extend_from_slice(s.as_bytes());
                        drawn = r;
                    }
                }
                Out::Flush(done) => {
                    if let Some(r) = drawn.take().filter(|_| scan.ground()) {
                        buf.extend_from_slice(erase(&mirror, r).as_bytes());
                    }
                    let mut out = std::io::stdout().lock();
                    let _ = out.write_all(&buf);
                    let _ = out.flush();
                    let _ = done.send(());
                    continue;
                }
            }
            if sync && !scan.sync {
                buf.extend_from_slice(SYNC_OFF);
            }
            if !buf.is_empty() {
                let mut out = std::io::stdout().lock();
                let _ = out.write_all(&buf);
                let _ = out.flush();
            }
        }
    })
}

/// Ask the terminal for the cursor position (DSR 6). Needs raw mode, and must
/// run before the input thread starts. Returns the position (row, column, both
/// from 1) and other input that arrived meanwhile.
pub fn query_cursor(timeout: std::time::Duration) -> (Option<(u16, u16)>, Vec<u8>) {
    let q = b"\x1b[6n";
    unsafe {
        libc::write(1, q.as_ptr() as *const libc::c_void, q.len());
    }
    let end = std::time::Instant::now() + timeout;
    let mut buf = Vec::new();
    loop {
        if let Some((pos, a, b)) = parse_cpr(&buf) {
            buf.drain(a..b);
            return (Some(pos), buf);
        }
        let left = end.saturating_duration_since(std::time::Instant::now());
        if left.is_zero() {
            return (None, buf);
        }
        let mut pfd = libc::pollfd { fd: 0, events: libc::POLLIN, revents: 0 };
        if unsafe { libc::poll(&mut pfd, 1, left.as_millis() as i32) } <= 0 {
            continue;
        }
        let mut tmp = [0u8; 256];
        let n = unsafe { libc::read(0, tmp.as_mut_ptr() as *mut libc::c_void, tmp.len()) };
        if n <= 0 {
            return (None, buf);
        }
        buf.extend_from_slice(&tmp[..n as usize]);
    }
}

fn parse_cpr(b: &[u8]) -> Option<((u16, u16), usize, usize)> {
    let mut i = 0;
    while let Some(p) = b[i..].windows(2).position(|w| w == b"\x1b[") {
        let start = i + p;
        let rest = &b[start + 2..];
        if let Some(e) = rest.iter().position(|c| !(c.is_ascii_digit() || *c == b';'))
            && rest[e] == b'R'
            && let Some((r, c)) = std::str::from_utf8(&rest[..e]).ok().and_then(|s| s.split_once(';'))
            && let (Ok(r), Ok(c)) = (r.parse(), c.parse())
        {
            return Some(((r, c), start, start + 2 + e + 1));
        }
        i = start + 1;
    }
    None
}

/// Scroll the lines above the cursor into the scrollback and put the cursor at
/// the top left of a blank screen. Then the mirror (blank) and the terminal
/// agree. Only lines with content go into the scrollback.
pub fn prepare_screen(rows: u16, pos: Option<(u16, u16)>) -> Vec<u8> {
    let n = match pos {
        Some((r, c)) => if c > 1 { r } else { r.saturating_sub(1) },
        None => rows,
    };
    let mut v = format!("\x1b[0m\x1b[{rows};1H").into_bytes();
    v.extend(std::iter::repeat_n(b'\n', n as usize));
    v.extend_from_slice(b"\x1b[H\x1b[J");
    v
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
    csi: Vec<u8>,
    /// Continuation bytes of a UTF-8 character that are still to come.
    utf8: u8,
    pub title: Vec<u8>,
    /// The program has an open synchronized update (mode 2026).
    pub sync: bool,
}

#[derive(Default, PartialEq, Clone, Copy)]
enum State {
    #[default]
    Ground,
    Esc,
    EscInt,
    Csi,
    Osc,
    OscEsc,
    Str,
    StrEsc,
}

impl Scan {
    pub fn ground(&self) -> bool {
        self.st == State::Ground && self.utf8 == 0
    }

    pub fn feed(&mut self, data: &[u8], mut on_osc: impl FnMut(&[u8], &[u8])) {
        for &b in data {
            self.st = match self.st {
                State::Ground => {
                    self.utf8 = match b {
                        0x80..=0xbf => self.utf8.saturating_sub(1),
                        0xc0..=0xdf => 1,
                        0xe0..=0xef => 2,
                        0xf0..=0xf7 => 3,
                        _ => 0,
                    };
                    if b == 0x1b { State::Esc } else { State::Ground }
                }
                State::Esc => self.after_esc(b),
                State::EscInt => match b {
                    0x20..=0x2f => State::EscInt,
                    0x1b => State::Esc,
                    _ => State::Ground,
                },
                State::Csi => match b {
                    0x40..=0x7e => {
                        self.csi_end(b);
                        State::Ground
                    }
                    0x18 | 0x1a => State::Ground,
                    0x1b => State::Esc,
                    _ => {
                        if self.csi.len() < 64 {
                            self.csi.push(b);
                        }
                        State::Csi
                    }
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

    fn csi_end(&mut self, fin: u8) {
        let p = std::mem::take(&mut self.csi);
        if (fin == b'h' || fin == b'l') && p.first() == Some(&b'?') && p[1..].split(|&c| c == b';').any(|x| x == b"2026") {
            self.sync = fin == b'h';
        }
    }

    fn after_esc(&mut self, b: u8) -> State {
        self.utf8 = 0;
        match b {
            b'[' => {
                self.csi.clear();
                State::Csi
            }
            b']' => {
                self.osc.clear();
                State::Osc
            }
            b'P' | b'_' | b'^' | b'X' => State::Str,
            0x1b => State::Esc,
            // "ESC ( B" and similar: wait for the final byte.
            0x20..=0x2f => State::EscInt,
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
    fn cursor_report() {
        assert_eq!(parse_cpr(b"ab\x1b[12;40Rcd"), Some(((12, 40), 2, 10)));
        assert_eq!(parse_cpr(b"\x1b[A\x1b[3;1R"), Some(((3, 1), 3, 9)));
        assert_eq!(parse_cpr(b"\x1b[12;40"), None);
    }

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

    #[test]
    fn scan_utf8_charset_and_sync() {
        let mut s = Scan::default();
        let euro = "€".as_bytes();
        s.feed(&euro[..1], |_, _| {});
        assert!(!s.ground());
        s.feed(&euro[1..], |_, _| {});
        assert!(s.ground());
        s.feed(b"\x1b(", |_, _| {});
        assert!(!s.ground());
        s.feed(b"0q", |_, _| {});
        assert!(s.ground());
        s.feed(b"\x1b[?2026h", |_, _| {});
        assert!(s.sync);
        s.feed(b"\x1b[?25;2026l", |_, _| {});
        assert!(!s.sync);
    }
}
