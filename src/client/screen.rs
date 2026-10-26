//! The output thread. Session output goes to the local terminal as it is.
//! On top of it: an overlay (connection state, panels), local echo
//! predictions, and a view mode for a pty of another size.
//!
//! The thread keeps a mirror of the screen (crate::vt). Everything that is
//! not session output is drawn between escape sequences, in one synchronized
//! update, and removed again by drawing the mirror's cells, before more
//! output goes to the terminal.
//!
//! View mode: when another client sets the pty size and this terminal has
//! another size, the output cannot go to the terminal as it is. Then the
//! terminal shows its alternate screen, and the thread draws the mirror into
//! it (rows that changed). When this client sets the size again, the main
//! screen comes back with the current content.

use std::{
    hash::{Hash, Hasher},
    io::Write,
    sync::mpsc as smpsc,
    time::Duration,
};

use base64::Engine;
use tokio::sync::mpsc;

use super::{predict::Predictor, term::Event};
use crate::vt::{Mirror, Prep};

/// A small panel at the top right of the screen. It is not part of the session:
/// when it goes away, the cells under it are drawn again from the mirror.
#[derive(Clone, PartialEq, Debug)]
pub struct Overlay {
    /// (text, warning style)
    pub lines: Vec<(String, bool)>,
}

pub enum Out {
    /// Session output.
    Data(Vec<u8>),
    /// The pty size from here on, and whether this client sets it.
    Size { cols: u16, rows: u16, mine: bool },
    Overlay(Option<Overlay>),
    /// The local terminal has a new size.
    Resize(u16, u16),
    /// Keys that went to the server; `end` is the input count after them.
    Typed { data: Vec<u8>, end: u64 },
    /// The server's input acknowledgement.
    InputAck { n: u64, mode: u8 },
    /// Show predictions or not, and the round-trip time.
    Predict { show: bool, rtt: Duration },
    /// A new client identity (session switch): input counts start again.
    NewClient,
    Tick,
    Flush(smpsc::Sender<()>),
}

const SYNC_ON: &[u8] = b"\x1b[?2026h";
const SYNC_OFF: &[u8] = b"\x1b[?2026l";
/// Input modes off, before the view sets the session's modes.
const INPUT_OFF: &str = "\x1b[?1l\x1b>\x1b[?1000l\x1b[?1002l\x1b[?1003l\x1b[?1005l\x1b[?1006l\x1b[?2004l\x1b[?1004l\x1b[<99u";
/// Lines of scrollback in the output thread's mirror (for the view mode).
const VIEW_HISTORY: usize = 5000;
/// Outside the pty area in view mode.
const FILL: &str = "\x1b[0;38;5;238m";

#[derive(Clone, Copy)]
struct Region {
    col: usize,
    width: usize,
    height: usize,
}

fn cup(row: usize, col: usize) -> String {
    format!("\x1b[{};{}H", row + 1, col + 1)
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


/// The overlay at the top right of a `cols` x `rows` area. One empty column
/// stays at the right edge when there is space.
fn place(o: &Overlay, cols: usize, rows: usize) -> Option<(String, Region)> {
    use unicode_width::UnicodeWidthStr;
    let inner = o.lines.iter().map(|(l, _)| l.width()).max().unwrap_or(0);
    let width = (inner + 2).min(cols);
    let height = o.lines.len().min(rows);
    if width < 3 || height == 0 {
        return None;
    }
    let col = cols - width - usize::from(cols > width);
    let mut out = String::new();
    for (i, (text, warn)) in o.lines.iter().take(height).enumerate() {
        let style = if *warn { "\x1b[0;1;38;5;232;48;5;214m" } else { "\x1b[0;38;5;255;48;5;238m" };
        out.push_str(&format!("{}{style} {} \x1b[0m", cup(i, col), fit(text, width - 2)));
    }
    Some((out, Region { col, width, height }))
}

#[derive(Default)]
struct View {
    /// Scrollback lines of the mirror when the view started: the lines after
    /// it go into the terminal's scrollback when the view ends.
    hist0: usize,
    top: usize,
    left: usize,
    /// Hash of what each terminal row shows.
    front: Vec<Option<u64>>,
    modes: String,
    title: Option<String>,
    shape: String,
}

struct Screen {
    out: Vec<u8>,
    mirror: Mirror,
    scan: Scan,
    osc52: bool,
    /// Predictions are on (not `--predict never`).
    predict: bool,
    /// Local terminal size (columns, rows).
    term: (usize, usize),
    /// The server tells the pty size (SIZE frames).
    sized: bool,
    /// This client sets the pty size.
    mine: bool,
    view: Option<View>,
    overlay: Option<Overlay>,
    drawn: Option<Region>,
    pred: Predictor,
    /// Row and columns of drawn predictions (the cursor moved too).
    pred_drawn: Option<(usize, usize, usize)>,
    pred_mark: bool,
    events: mpsc::UnboundedSender<Event>,
}

impl Screen {
    /// Remove the overlay and predictions: draw the mirror's cells and put the
    /// cursor back. Only between escape sequences (the only place they are).
    fn erase(&mut self) {
        if let Some((row, c0, c1)) = self.pred_drawn.take() {
            self.out.extend_from_slice(self.mirror.repaint(row..row + 1, c0, c1 - c0).as_bytes());
        }
        if let Some(r) = self.drawn.take() {
            self.out.extend_from_slice(self.mirror.repaint(0..r.height, r.col, r.width).as_bytes());
        }
    }

    /// Draw predictions and the overlay (passthrough mode).
    fn draw(&mut self) {
        if !self.scan.ground() || self.view.is_some() {
            return;
        }
        let plan = self.pred.plan(&self.mirror);
        let (mc, mr) = self.mirror.size();
        let ov = match &self.overlay {
            Some(o) if !self.mirror.origin_mode() => place(o, self.term.0.min(mc), self.term.1.min(mr)),
            _ => None,
        };
        if plan.is_none() && ov.is_none() {
            return;
        }
        let mut s = format!("\x1b[?25l\x1b]8;;\x1b\\{}", self.mirror.neutral());
        if let Some(p) = &plan {
            if p.end > p.col {
                s.push_str(&self.mirror.compose(p.row, p.col, &p.items, p.mark));
            }
            self.pred_drawn = Some((p.row, p.col, p.end));
            self.pred_mark = p.mark;
        }
        if let Some((o, r)) = ov {
            s.push_str(&o);
            self.drawn = Some(r);
        }
        s.push_str(&self.mirror.cursor_restore());
        if let Some(p) = &plan
            && let Some(c) = p.cursor
        {
            s.push_str(&cup(p.row, c));
        }
        self.out.extend_from_slice(s.as_bytes());
    }

    /// Erase the layers, change something, draw the layers: one frame.
    fn frame(&mut self, f: impl FnOnce(&mut Self)) {
        if self.view.is_some() {
            f(self);
            return;
        }
        let ground = self.scan.ground();
        let sync = ground && !self.scan.sync && (self.drawn.is_some() || self.pred_drawn.is_some() || self.overlay.is_some() || self.pred.active());
        if sync {
            self.out.extend_from_slice(SYNC_ON);
        }
        if ground {
            self.erase();
        }
        f(self);
        self.draw();
        if sync && !self.scan.sync {
            self.out.extend_from_slice(SYNC_OFF);
        }
    }

    fn want_view(&self) -> bool {
        let (c, r) = self.mirror.size();
        self.sized && !self.mine && (c, r) != self.term
    }

    fn update_view(&mut self) {
        match (self.view.is_some(), self.want_view()) {
            (false, true) => {
                if self.scan.ground() {
                    self.erase();
                } else {
                    self.drawn = None;
                    self.pred_drawn = None;
                }
                self.pred.forget();
                self.out.extend_from_slice(b"\x1b[?1049h");
                self.view = Some(View { hist0: self.mirror.history_size(), ..Default::default() });
                self.render(true);
                let (c, r) = self.mirror.size();
                let _ = self.events.send(Event::View(Some((c as u16, r as u16))));
            }
            (true, false) => {
                let v = self.view.take().unwrap_or_default();
                self.drawn = None;
                // The main screen as it was, then the lines that scrolled during
                // the view (they start with that screen), then the screen.
                let n = self.mirror.history_size().saturating_sub(v.hist0);
                self.out.extend_from_slice(b"\x1b[?1049l");
                self.out.extend_from_slice(&self.mirror.snapshot(Some(n), Prep::Clear));
                self.draw();
                let _ = self.events.send(Event::View(None));
            }
            (true, true) => self.render(true),
            (false, false) => {}
        }
    }

    /// Draw the mirror into the terminal (view mode): the rows that changed.
    fn render(&mut self, full: bool) {
        let Some(v) = self.view.as_mut() else { return };
        let (tc, tr) = self.term;
        let (pc, pr) = self.mirror.size();
        let (crow, ccol, visible, _) = self.mirror.cursor_info();
        // Keep the cursor in the view.
        if pr > tr {
            if crow < v.top {
                v.top = crow;
            } else if crow >= v.top + tr {
                v.top = crow + 1 - tr;
            }
            v.top = v.top.min(pr - tr);
        } else {
            v.top = 0;
        }
        if pc > tc {
            if ccol < v.left {
                v.left = ccol;
            } else if ccol >= v.left + tc {
                v.left = ccol + 1 - tc;
            }
            v.left = v.left.min(pc - tc);
        } else {
            v.left = 0;
        }
        let mut s = String::from("\x1b[?2026h\x1b[?25l");
        if full || v.front.len() != tr {
            v.front = vec![None; tr];
            s.push_str("\x1b[0m\x1b[H\x1b[2J");
        }
        // Rows under the overlay are drawn again; the overlay goes on top.
        if let Some(r) = self.drawn.take() {
            for f in v.front.iter_mut().take(r.height) {
                *f = None;
            }
        }
        let w = tc.min(pc.saturating_sub(v.left));
        for r in 0..tr {
            let mut line = String::new();
            let fill = if v.top + r < pr {
                line.push_str(&self.mirror.render_row(v.top + r, v.left, w));
                tc - w
            } else {
                tc
            };
            if fill > 0 {
                line.push_str(FILL);
                line.push_str(&"·".repeat(fill));
                line.push_str("\x1b[0m");
            }
            let mut h = std::hash::DefaultHasher::new();
            line.hash(&mut h);
            let h = h.finish();
            if v.front[r] != Some(h) {
                s.push_str(&cup(r, 0));
                s.push_str(&line);
                v.front[r] = Some(h);
            }
        }
        let modes = self.mirror.input_modes();
        if modes != v.modes {
            s.push_str(INPUT_OFF);
            s.push_str(&modes);
            v.modes = modes;
        }
        let title = self.mirror.title();
        if title != v.title {
            if let Some(t) = &title {
                s.push_str(&format!("\x1b]2;{t}\x07"));
            }
            v.title = title;
        }
        let shape = self.mirror.cursor_shape();
        if shape != v.shape {
            s.push_str(&shape);
            v.shape = shape;
        }
        if let Some((o, r)) = self.overlay.as_ref().and_then(|o| place(o, tc, tr)) {
            s.push_str(&o);
            self.drawn = Some(r);
        }
        if visible && (v.top..v.top + tr).contains(&crow) && (v.left..v.left + tc).contains(&ccol) {
            s.push_str(&cup(crow - v.top, ccol - v.left));
            s.push_str("\x1b[?25h");
        }
        s.push_str("\x1b[?2026l");
        self.out.extend_from_slice(s.as_bytes());
    }

    fn handle(&mut self, m: Out) {
        match m {
            Out::Data(d) => {
                let osc52 = self.osc52;
                if self.view.is_some() {
                    self.scan.feed(&d, |code, body| {
                        if osc52 && code == b"52" {
                            set_clipboard_from_osc52(body);
                        }
                    });
                    self.mirror.feed(&d);
                    self.render(false);
                } else {
                    self.frame(|s| {
                        s.scan.feed(&d, |code, body| {
                            if osc52 && code == b"52" {
                                set_clipboard_from_osc52(body);
                            }
                        });
                        s.mirror.feed(&d);
                        s.out.extend_from_slice(&d);
                        s.pred.after_output(&s.mirror);
                    });
                }
            }
            Out::Size { cols, rows, mine } => {
                self.sized = true;
                self.mine = mine;
                if self.mirror.size() != (cols as usize, rows as usize) {
                    // A resize reflows the scrollback; keep the count of lines
                    // that came during the view.
                    let added = self.view.as_ref().map(|v| self.mirror.history_size().saturating_sub(v.hist0));
                    self.frame(|s| {
                        s.mirror.resize(cols, rows);
                        s.pred.forget();
                    });
                    if let (Some(v), Some(a)) = (self.view.as_mut(), added) {
                        v.hist0 = self.mirror.history_size().saturating_sub(a);
                    }
                }
                self.update_view();
            }
            Out::Overlay(o) => {
                if o != self.overlay {
                    if self.view.is_some() {
                        self.overlay = o;
                        self.render(false);
                    } else {
                        self.frame(|s| s.overlay = o);
                    }
                }
            }
            Out::Resize(c, r) => {
                self.term = (c as usize, r as usize);
                // The terminal moved the cells itself: forget what was drawn.
                self.drawn = None;
                self.pred_drawn = None;
                self.pred.forget();
                if !self.sized {
                    self.mirror.resize(c, r);
                }
                self.update_view();
                if self.view.is_none() {
                    self.frame(|_| {});
                }
            }
            Out::Typed { data, end } => {
                let enabled = self.predict && self.view.is_none();
                self.frame(|s| s.pred.typed(&data, end, &s.mirror, enabled));
            }
            Out::InputAck { n, mode } => self.frame(|s| s.pred.ack(n, mode, &s.mirror)),
            Out::Predict { show, rtt } => {
                self.pred.set_rtt(rtt);
                if show != self.pred.show {
                    self.frame(|s| s.pred.show = show);
                }
            }
            Out::NewClient => self.frame(|s| s.pred.reset()),
            Out::Tick => {
                if self.pred_drawn.is_some() && self.pred.plan(&self.mirror).is_some_and(|p| p.mark != self.pred_mark) {
                    self.frame(|_| {});
                }
            }
            Out::Flush(_) => {}
        }
    }
}

pub fn spawn_stdout(
    mut rx: mpsc::Receiver<Out>,
    osc52: bool,
    predict: bool,
    cols: u16,
    rows: u16,
    events: mpsc::UnboundedSender<Event>,
) -> std::thread::JoinHandle<()> {
    std::thread::spawn(move || {
        let mut s = Screen {
            out: Vec::new(),
            mirror: Mirror::new(cols, rows, VIEW_HISTORY),
            scan: Scan::default(),
            osc52,
            predict,
            term: (cols as usize, rows as usize),
            sized: false,
            mine: true,
            view: None,
            overlay: None,
            drawn: None,
            pred: Predictor::default(),
            pred_drawn: None,
            pred_mark: false,
            events,
        };
        while let Some(m) = rx.blocking_recv() {
            if let Out::Flush(done) = m {
                if s.scan.ground() {
                    s.erase();
                }
                let mut o = std::io::stdout().lock();
                let _ = o.write_all(&s.out);
                let _ = o.flush();
                s.out.clear();
                let _ = done.send(());
                continue;
            }
            s.handle(m);
            // More messages that are waiting go into the same write.
            while s.out.len() < 1 << 20
                && let Ok(m) = rx.try_recv()
            {
                if let Out::Flush(done) = m {
                    if s.scan.ground() {
                        s.erase();
                    }
                    let mut o = std::io::stdout().lock();
                    let _ = o.write_all(&s.out);
                    let _ = o.flush();
                    s.out.clear();
                    let _ = done.send(());
                    continue;
                }
                s.handle(m);
            }
            if !s.out.is_empty() {
                let mut o = std::io::stdout().lock();
                let _ = o.write_all(&s.out);
                let _ = o.flush();
                s.out.clear();
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

    fn screen(cols: u16, rows: u16) -> (Screen, mpsc::UnboundedReceiver<Event>) {
        let (tx, rx) = mpsc::unbounded_channel();
        let s = Screen {
            out: Vec::new(),
            mirror: Mirror::new(cols, rows, VIEW_HISTORY),
            scan: Scan::default(),
            osc52: false,
            predict: true,
            term: (cols as usize, rows as usize),
            sized: false,
            mine: true,
            view: None,
            overlay: None,
            drawn: None,
            pred: Predictor::default(),
            pred_drawn: None,
            pred_mark: false,
            events: tx,
        };
        (s, rx)
    }

    /// What a terminal shows after the bytes that the screen wrote.
    fn shown(s: &mut Screen, cols: u16, rows: u16, before: &[u8]) -> Mirror {
        let mut t = Mirror::new(cols, rows, 0);
        t.feed(before);
        t.feed(&std::mem::take(&mut s.out));
        t
    }

    #[test]
    fn overlay_comes_and_goes() {
        let (mut s, _rx) = screen(30, 4);
        let mut all = Vec::new();
        s.handle(Out::Data(b"hello world, this is a long line".to_vec()));
        all.extend(std::mem::take(&mut s.out));
        s.handle(Out::Overlay(Some(Overlay { lines: vec![("note".into(), false)] })));
        all.extend(std::mem::take(&mut s.out));
        let mut t = Mirror::new(30, 4, 0);
        t.feed(&all);
        assert!(t.text().contains(" note "));
        s.handle(Out::Data(b"!".to_vec()));
        s.handle(Out::Overlay(None));
        all.extend(std::mem::take(&mut s.out));
        let mut t = Mirror::new(30, 4, 0);
        t.feed(&all);
        assert_eq!(t.text(), s.mirror.text());
        assert_eq!(t.cursor(), s.mirror.cursor());
    }

    #[test]
    fn view_of_a_smaller_pty() {
        let (mut s, mut rx) = screen(20, 6);
        s.handle(Out::Size { cols: 10, rows: 3, mine: false });
        assert!(matches!(rx.try_recv(), Ok(Event::View(Some((10, 3))))));
        s.handle(Out::Data(b"abc\r\ndef".to_vec()));
        let t = shown(&mut s, 20, 6, b"");
        let text = t.text();
        // The pty area (10 columns), then the fill.
        assert!(text.starts_with("abc       ·"), "{text:?}");
        assert!(text.contains("\ndef       ·"));
        assert_eq!(t.cursor(), (1, 3));
        // This client sets the size again: the normal screen, up to date.
        s.handle(Out::Size { cols: 20, rows: 6, mine: true });
        assert!(matches!(rx.try_recv(), Ok(Event::View(None))));
        let t = shown(&mut s, 20, 6, b"");
        assert!(t.text().starts_with("abc\ndef"));
        assert!(!t.alt_screen());
    }

    #[test]
    fn lines_during_a_view_reach_the_scrollback() {
        let (mut s, _rx) = screen(20, 4);
        let mut all = std::mem::take(&mut s.out);
        s.handle(Out::Size { cols: 20, rows: 4, mine: true });
        s.handle(Out::Data(b"a1\r\na2\r\n".to_vec()));
        all.extend(std::mem::take(&mut s.out));
        s.handle(Out::Size { cols: 15, rows: 4, mine: false });
        for i in 0..10 {
            s.handle(Out::Data(format!("b{i}\r\n").into_bytes()));
        }
        s.handle(Out::Size { cols: 20, rows: 4, mine: true });
        all.extend(std::mem::take(&mut s.out));
        let mut t = Mirror::new(20, 4, 100);
        t.feed(&all);
        let text = t.all_text();
        let lines: Vec<&str> = text.lines().filter(|l| !l.trim().is_empty()).collect();
        assert_eq!(lines, ["a1", "a2", "b0", "b1", "b2", "b3", "b4", "b5", "b6", "b7", "b8", "b9"], "{text:?}");
    }

    #[test]
    fn view_of_a_larger_pty_follows_the_cursor() {
        let (mut s, _rx) = screen(8, 3);
        s.handle(Out::Size { cols: 20, rows: 10, mine: false });
        s.handle(Out::Data(b"\x1b[9;15Hxy".to_vec()));
        let t = shown(&mut s, 8, 3, b"");
        assert!(t.text().contains("xy"), "{:?}", t.text());
    }

    #[test]
    fn typed_keys_show_at_once() {
        let (mut s, _rx) = screen(20, 3);
        s.handle(Out::Data(b"$ ".to_vec()));
        s.handle(Out::InputAck { n: 0, mode: crate::proto::mode::KNOWN | crate::proto::mode::SHELL });
        s.handle(Out::Predict { show: true, rtt: Duration::from_millis(100) });
        let mut all = b"$ ".to_vec();
        all.extend(std::mem::take(&mut s.out));
        s.handle(Out::Typed { data: b"ls".to_vec(), end: 2 });
        all.extend(std::mem::take(&mut s.out));
        let mut t = Mirror::new(20, 3, 0);
        t.feed(&all);
        assert_eq!(t.text().lines().next(), Some("$ ls"));
        assert_eq!(t.cursor(), (0, 4));
        // The echo comes; the acknowledgement removes the prediction.
        s.handle(Out::Data(b"ls".to_vec()));
        s.handle(Out::InputAck { n: 2, mode: crate::proto::mode::KNOWN | crate::proto::mode::SHELL });
        all.extend(std::mem::take(&mut s.out));
        let mut t = Mirror::new(20, 3, 0);
        t.feed(&all);
        assert_eq!(t.text().lines().next(), Some("$ ls"));
        assert_eq!(t.cursor(), (0, 4));
        assert!(s.pred_drawn.is_none());
    }

    #[test]
    fn cursor_report() {
        assert_eq!(parse_cpr(b"ab\x1b[12;40Rcd"), Some(((12, 40), 2, 10)));
        assert_eq!(parse_cpr(b"\x1b[A\x1b[3;1R"), Some(((3, 1), 3, 9)));
        assert_eq!(parse_cpr(b"\x1b[12;40"), None);
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
