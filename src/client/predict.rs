//! Local echo prediction. When the round trip is long, typed characters show
//! at once, before the server's echo arrives.
//!
//! A prediction is a set of expected cells on the cursor row, and an expected
//! cursor column. The server acknowledges input when the program has had it
//! for a moment (and after the output that holds the echo): then every
//! expected cell of that input must be on the screen. If one is not, the
//! prediction was wrong, and all predictions go away.
//!
//! Predictions are shown only where an echo is sure: the kernel echoes (line
//! mode), or a shell's line editor runs; in other programs only after the
//! first prediction of a typing burst came true. Nothing is predicted while the
//! terminal does not echo in line mode (a password prompt), or while the
//! cursor is hidden (the program draws its own cursor).

use std::{
    collections::BTreeMap,
    time::{Duration, Instant},
};

use unicode_width::UnicodeWidthChar;

use crate::{
    proto::mode,
    vt::{CellView, Mirror, Src},
};

/// One column of the predicted row.
#[derive(Clone, Copy, Debug, PartialEq)]
struct VCell {
    src: Src,
    /// The character the server should put there ('\0': do not check).
    want: char,
    wide: bool,
    spacer: bool,
    blank: bool,
    plain_fg: bool,
}

impl VCell {
    fn of(col: usize, c: &CellView) -> VCell {
        VCell {
            src: Src::Orig(col),
            want: if c.spacer { '\0' } else { c.c },
            wide: c.wide,
            spacer: c.spacer,
            blank: c.blank,
            plain_fg: c.plain_fg,
        }
    }
    fn blank() -> VCell {
        VCell { src: Src::Blank, want: ' ', wide: false, spacer: false, blank: true, plain_fg: true }
    }
}

#[derive(Clone, Copy, Debug)]
struct Expect {
    cell: VCell,
    /// Input count after the key that set this cell.
    seq: u64,
    /// The screen's character before the prediction: a match that changes
    /// it proves that the program echoes.
    orig: char,
}

#[derive(Debug, PartialEq)]
enum Key {
    Char(char),
    Back,
    /// Enter, Ctrl-C, Ctrl-D: a new line or prompt follows.
    Line,
    /// Anything else: its effect is not known.
    Other,
    /// A key release (kitty keyboard protocol): no effect.
    None,
}

/// What to draw for the predictions.
pub struct Plan {
    pub row: usize,
    pub col: usize,
    pub end: usize,
    pub items: Vec<Src>,
    pub cursor: Option<usize>,
    pub mark: bool,
}

#[derive(Default)]
pub struct Predictor {
    /// Show predictions (the round trip is long, or the link is stalled).
    pub show: bool,
    rtt: Duration,
    mode: u8,
    /// Input count that the server acknowledged.
    acked: u64,
    /// Input count at the end of the last typed keys.
    typed: u64,
    row: usize,
    cells: BTreeMap<usize, Expect>,
    cursor: Option<(usize, u64)>,
    first_at: Option<Instant>,
    /// A prediction of this typing burst came true.
    confirmed: bool,
    /// No predictions until the server acknowledged input up to here.
    pause_until: u64,
    /// Start of the typed text on the row: Backspace does not go left of it.
    line_start: Option<(usize, usize)>,
    /// Input counts after Enter (and similar) keys. The program can change
    /// the screen after them, so predictions before them are not checked.
    lines: std::collections::VecDeque<u64>,
}

fn decode(b: &[u8]) -> Vec<(Key, usize)> {
    let mut out = Vec::new();
    let mut i = 0;
    while i < b.len() {
        let c = b[i];
        let (k, n) = match c {
            0x1b => {
                let rest = &b[i + 1..];
                match rest.first() {
                    Some(b'[') => {
                        let end = rest.iter().skip(1).position(|x| (0x40..=0x7e).contains(x)).map(|p| p + 2);
                        match end {
                            Some(e) => (csi_key(&rest[1..e]), e + 1),
                            None => (Key::Other, b.len() - i),
                        }
                    }
                    Some(b'O') => (Key::Other, 3.min(b.len() - i)),
                    Some(_) => (Key::Other, 2),
                    None => (Key::Other, 1),
                }
            }
            b'\r' | b'\n' | 0x03 | 0x04 => (Key::Line, 1),
            0x7f | 0x08 => (Key::Back, 1),
            0..=0x1f => (Key::Other, 1),
            _ => {
                let len = match c {
                    0xc0..=0xdf => 2,
                    0xe0..=0xef => 3,
                    0xf0..=0xf7 => 4,
                    _ => 1,
                };
                let len = len.min(b.len() - i);
                match std::str::from_utf8(&b[i..i + len]).ok().and_then(|s| s.chars().next()) {
                    Some(ch) => (Key::Char(ch), len),
                    None => (Key::Other, len),
                }
            }
        };
        out.push((k, n));
        i += n;
    }
    out
}

/// A kitty keyboard protocol key (`CSI code[:alt] ; mods[:event] u`): a plain
/// press of a text key is a character.
fn csi_key(body: &[u8]) -> Key {
    let Some((&fin, params)) = body.split_last() else { return Key::Other };
    if fin != b'u' {
        return Key::Other;
    }
    let s = std::str::from_utf8(params).unwrap_or("");
    let mut f = s.split(';');
    let code: u32 = f.next().and_then(|k| k.split(':').next()).and_then(|k| k.parse().ok()).unwrap_or(0);
    let (mods, event) = match f.next() {
        Some(m) => {
            let mut it = m.split(':');
            (it.next().and_then(|x| x.parse().ok()).unwrap_or(1u32), it.next().and_then(|x| x.parse().ok()).unwrap_or(1u32))
        }
        None => (1, 1),
    };
    if event == 3 {
        return Key::None;
    }
    match (code, mods) {
        (13, 1) => Key::Line,
        (127, 1) => Key::Back,
        (c, 1) if c >= 32 && !(57344..=63743).contains(&c) => char::from_u32(c).map_or(Key::Other, Key::Char),
        _ => Key::Other,
    }
}

impl Predictor {
    fn trusted(&self) -> bool {
        self.mode & mode::KNOWN != 0 && (self.mode & mode::ECHO != 0 || (self.mode & mode::ICANON == 0 && self.mode & mode::SHELL != 0))
    }

    /// The terminal reads a line without echo: a password.
    fn secret(&self) -> bool {
        self.mode & mode::KNOWN != 0 && self.mode & mode::ECHO == 0 && self.mode & mode::ICANON != 0
    }

    pub fn set_rtt(&mut self, rtt: Duration) {
        self.rtt = rtt;
    }

    /// Forget everything (a new client identity, a resize, a new screen).
    pub fn reset(&mut self) {
        self.clear();
        self.lines.clear();
        self.acked = 0;
        self.typed = 0;
        self.pause_until = 0;
        self.line_start = None;
    }

    /// Drop the predictions (the screen moved: a resize, a new size).
    pub fn forget(&mut self) {
        self.wrong();
        self.line_start = None;
        self.pause(self.typed);
    }

    fn clear(&mut self) {
        self.cells.clear();
        self.cursor = None;
        self.first_at = None;
    }

    fn wrong(&mut self) {
        self.clear();
        self.confirmed = false;
    }

    fn pause(&mut self, seq: u64) {
        self.pause_until = self.pause_until.max(seq);
    }

    pub fn active(&self) -> bool {
        !self.cells.is_empty() || self.cursor.is_some()
    }

    /// The input acknowledgement: input up to `n` reached the program, and the
    /// output before this message holds its echo.
    pub fn ack(&mut self, n: u64, mode: u8, m: &Mirror) {
        if mode & mode::KNOWN != 0 {
            self.mode = mode;
        }
        if self.secret() {
            self.wrong();
        }
        if n <= self.acked {
            return;
        }
        self.acked = n;
        let mut done = 0;
        while self.lines.front().is_some_and(|&l| l <= n) {
            done = self.lines.pop_front().unwrap();
        }
        let (crow, ccol, _, _) = m.cursor_info();
        let row = m.row_view(self.row);
        let mut bad = false;
        self.cells.retain(|&col, e| {
            if e.seq > n {
                return true;
            }
            if e.seq > done && e.cell.want != '\0' && row.get(col).map(|c| c.c) != Some(e.cell.want) {
                bad = true;
            }
            false
        });
        if let Some((c, seq)) = self.cursor
            && seq <= n
        {
            if seq > done && (crow != self.row || ccol != c) {
                bad = true;
            }
            self.cursor = None;
        }
        if bad {
            crate::paths::log(format!("prediction wrong at input {n}"));
            self.wrong();
        } else if !self.active() {
            self.first_at = None;
        }
    }

    /// After output: a typed character that appeared proves the echo.
    pub fn after_output(&mut self, m: &Mirror) {
        if self.cells.is_empty() {
            return;
        }
        let row = m.row_view(self.row);
        for (&col, e) in &self.cells {
            if matches!(e.cell.src, Src::Char(_)) && e.orig != e.cell.want && row.get(col).map(|c| c.c) == Some(e.cell.want) {
                self.confirmed = true;
            }
        }
    }

    /// Keys that went to the server. `end` is the input count after them.
    pub fn typed(&mut self, data: &[u8], end: u64, m: &Mirror, enabled: bool) {
        let start = end.saturating_sub(data.len() as u64);
        if start != self.typed {
            // Keys that we did not see.
            self.pause(start);
        }
        self.typed = end;
        let mut seq = start;
        for (k, n) in decode(data) {
            seq += n as u64;
            self.key(k, seq, m, enabled);
        }
    }

    fn key(&mut self, k: Key, seq: u64, m: &Mirror, enabled: bool) {
        if k == Key::None {
            return;
        }
        if !enabled || self.secret() {
            self.wrong();
            self.pause(seq);
            return;
        }
        match k {
            Key::Line => {
                self.lines.push_back(seq);
                self.pause(seq);
                self.confirmed = false;
                self.line_start = None;
            }
            Key::Other | Key::None => {
                self.pause(seq);
                self.confirmed = false;
            }
            Key::Char(_) | Key::Back => {
                if self.acked < self.pause_until || !self.apply(&k, seq, m) {
                    self.pause(seq);
                }
            }
        }
    }

    /// Predict one key. False if its effect is not clear.
    fn apply(&mut self, k: &Key, seq: u64, m: &Mirror) -> bool {
        let (crow, ccol, visible, wrap) = m.cursor_info();
        if !visible || wrap {
            return false;
        }
        if self.active() && crow != self.row {
            return false;
        }
        let (row_n, mut x) = match self.cursor {
            Some((c, _)) => (self.row, c),
            None => (crow, ccol),
        };
        let view = m.row_view(row_n);
        let cols = view.len();
        if cols < 2 {
            return false;
        }
        // The row as predicted so far.
        let mut row: Vec<VCell> = view.iter().enumerate().map(|(i, c)| VCell::of(i, c)).collect();
        if row_n == self.row {
            for (&c, e) in &self.cells {
                if c < cols {
                    row[c] = e.cell;
                }
            }
        }
        if self.line_start.is_none_or(|(r, _)| r != row_n) {
            self.line_start = Some((row_n, x));
        }
        match *k {
            Key::Char(ch) => {
                let w = match ch.width() {
                    Some(w @ 1..=2) => w,
                    _ => return false,
                };
                // Never at the last column: the terminal would wait to wrap.
                if x + w > cols - 1 || row[x].spacer {
                    return false;
                }
                let suggestion = row[x].want == ch && !row[x].plain_fg;
                let insert = !suggestion && row[x..].iter().any(|c| !c.blank);
                if insert {
                    if row[cols - w..].iter().any(|c| !c.blank) {
                        return false;
                    }
                    for i in (x + w..cols).rev() {
                        row[i] = row[i - w];
                    }
                } else {
                    // Do not leave half of a wide character.
                    if row[x + w - 1].wide || (x + w < cols && row[x + w].spacer) {
                        row[x + w] = VCell::blank();
                    }
                }
                row[x] = VCell { src: Src::Char(ch), want: ch, wide: w == 2, spacer: false, blank: false, plain_fg: true };
                if w == 2 {
                    row[x + 1] = VCell { src: Src::Skip, want: '\0', wide: false, spacer: true, blank: false, plain_fg: true };
                }
                x += w;
            }
            Key::Back => {
                let start = self.line_start.map_or(x, |(_, c)| c);
                let w = if x >= 2 && row[x - 1].spacer { 2 } else { 1 };
                if x < start + w {
                    return false;
                }
                for i in x - w..cols - w {
                    row[i] = row[i + w];
                }
                for c in row.iter_mut().skip(cols - w) {
                    *c = VCell::blank();
                }
                x -= w;
            }
            _ => return false,
        }
        if row_n != self.row {
            self.clear();
            self.row = row_n;
        }
        // Keep the cells that differ from the screen.
        let mut cells = BTreeMap::new();
        for (i, c) in row.iter().enumerate() {
            if c.src == Src::Orig(i) {
                continue;
            }
            let seq = match self.cells.get(&i) {
                Some(e) if e.cell == *c => e.seq,
                _ => seq,
            };
            let orig = self.cells.get(&i).map_or(view[i].c, |e| e.orig);
            cells.insert(i, Expect { cell: *c, seq, orig });
        }
        self.cells = cells;
        self.cursor = Some((x, seq));
        self.first_at.get_or_insert_with(Instant::now);
        true
    }

    /// What to draw now, if anything.
    pub fn plan(&self, m: &Mirror) -> Option<Plan> {
        if !self.show || !self.active() || !(self.trusted() || self.confirmed) {
            return None;
        }
        let (crow, _, visible, _) = m.cursor_info();
        if crow != self.row || !visible {
            return None;
        }
        let view = m.row_view(self.row);
        let cols = view.len();
        // Columns whose screen content is not yet as predicted.
        let differ: Vec<usize> = self
            .cells
            .iter()
            .filter(|(c, e)| **c < cols && e.cell.want != '\0' && view[**c].c != e.cell.want)
            .map(|(c, _)| *c)
            .collect();
        let item = |c: usize| self.cells.get(&c).map_or(Src::Orig(c), |e| e.cell.src);
        let cont = |c: usize| matches!(item(c), Src::Skip) || matches!(item(c), Src::Orig(o) if view.get(o).is_some_and(|v| v.spacer));
        let (mut col, mut end) = match (differ.first(), differ.last()) {
            (Some(&a), Some(&b)) => (a, b + 1),
            _ => (0, 0),
        };
        if end > col {
            while col > 0 && cont(col) {
                col -= 1;
            }
            while end < cols && cont(end) {
                end += 1;
            }
        }
        let late = self.first_at.is_some_and(|t| t.elapsed() > self.rtt * 2 + Duration::from_millis(100));
        Some(Plan { row: self.row, col, end, items: (col..end).map(item).collect(), cursor: self.cursor.map(|(c, _)| c), mark: late })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn mirror(text: &[u8]) -> Mirror {
        let mut m = Mirror::new(20, 4, 0);
        m.feed(text);
        m
    }

    const SHELL: u8 = mode::KNOWN | mode::SHELL;

    #[test]
    fn keys() {
        assert_eq!(decode(b"a\x7f\r").into_iter().map(|k| k.0).collect::<Vec<_>>(), vec![Key::Char('a'), Key::Back, Key::Line]);
        assert_eq!(decode(b"\x1b[A").len(), 1);
        assert_eq!(decode("é".as_bytes())[0].0, Key::Char('é'));
        assert_eq!(decode(b"\x1b[97u")[0].0, Key::Char('a'));
        assert_eq!(decode(b"\x1b[97;1:3u")[0].0, Key::None);
        assert_eq!(decode(b"\x1b[97;5u")[0].0, Key::Other);
    }

    #[test]
    fn typing_at_a_prompt() {
        let mut m = mirror(b"$ ");
        let mut p = Predictor { show: true, ..Default::default() };
        p.ack(0, SHELL, &m);
        p.typed(b"ls", 2, &m, true);
        let plan = p.plan(&m).unwrap();
        assert_eq!((plan.row, plan.col, plan.end, plan.cursor), (0, 2, 4, Some(4)));
        assert_eq!(plan.items, vec![Src::Char('l'), Src::Char('s')]);
        // The echo of "l" arrives: only "s" is drawn.
        m.feed(b"l");
        let plan = p.plan(&m).unwrap();
        assert_eq!((plan.col, plan.end), (3, 4));
        m.feed(b"s");
        p.ack(2, SHELL, &m);
        assert!(!p.active());
    }

    #[test]
    fn wrong_prediction_goes() {
        let mut m = mirror(b"$ ");
        let mut p = Predictor { show: true, ..Default::default() };
        p.ack(0, SHELL, &m);
        p.typed(b"x", 1, &m, true);
        assert!(p.plan(&m).is_some());
        // The program printed something else.
        m.feed(b"y");
        p.ack(1, SHELL, &m);
        assert!(!p.active());
        assert!(!p.confirmed);
    }

    #[test]
    fn untrusted_program_needs_a_confirmation() {
        let mut m = mirror(b"> ");
        let mut p = Predictor { show: true, ..Default::default() };
        p.ack(0, mode::KNOWN, &m);
        p.typed(b"a", 1, &m, true);
        assert!(p.plan(&m).is_none());
        m.feed(b"a");
        p.after_output(&m);
        p.typed(b"b", 2, &m, true);
        assert!(p.plan(&m).is_some());
    }

    #[test]
    fn no_prediction_for_passwords_or_hidden_cursor() {
        let m = mirror(b"Password: ");
        let mut p = Predictor { show: true, ..Default::default() };
        p.ack(0, mode::KNOWN | mode::ICANON, &m);
        p.typed(b"secret", 6, &m, true);
        assert!(!p.active());
        let m = mirror(b"\x1b[?25l> ");
        let mut p = Predictor { show: true, ..Default::default() };
        p.ack(0, SHELL, &m);
        p.typed(b"a", 1, &m, true);
        assert!(p.plan(&m).is_none());
    }

    #[test]
    fn insert_and_backspace() {
        // Cursor in the middle of "$ ac": typing "b" shifts "c".
        let m = mirror(b"$ ac\x1b[D");
        let mut p = Predictor { show: true, ..Default::default() };
        p.ack(0, SHELL, &m);
        p.typed(b"b", 1, &m, true);
        let plan = p.plan(&m).unwrap();
        assert_eq!(plan.items, vec![Src::Char('b'), Src::Orig(3)]);
        assert_eq!(plan.cursor, Some(4));
        p.typed(b"\x7f", 2, &m, true);
        let plan = p.plan(&m).unwrap();
        assert_eq!(plan.cursor, Some(3));
        // Not left of where typing started.
        p.typed(b"\x7f", 3, &m, true);
        assert_eq!(p.plan(&m).unwrap().cursor, Some(3));
    }

    #[test]
    fn a_command_can_clear_its_own_line() {
        let mut m = mirror(b"$ ");
        let mut p = Predictor { show: true, ..Default::default() };
        p.ack(0, SHELL, &m);
        p.typed(b"clear\r", 6, &m, true);
        m.feed(b"\x1b[H\x1b[2J$ ");
        p.ack(6, SHELL, &m);
        assert!(!p.active());
        assert!(p.lines.is_empty());
    }

    #[test]
    fn enter_pauses_until_acknowledged() {
        let mut m = mirror(b"$ ");
        let mut p = Predictor { show: true, ..Default::default() };
        p.ack(0, SHELL, &m);
        p.typed(b"\r", 1, &m, true);
        p.typed(b"a", 2, &m, true);
        assert!(!p.active());
        m.feed(b"\r\n$ a");
        p.ack(2, SHELL, &m);
        p.typed(b"b", 3, &m, true);
        assert_eq!(p.plan(&m).unwrap().items, vec![Src::Char('b')]);
    }
}
