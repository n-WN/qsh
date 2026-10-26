//! Local terminal: raw mode, size, input and output threads.

use std::{io::Read, sync::Mutex};

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
    /// From the output thread: another client sets the pty size (this size),
    /// and the terminal shows a view of it; None: back to normal.
    View(Option<(u16, u16)>),
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

}
