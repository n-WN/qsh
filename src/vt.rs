//! Terminal mirror: an alacritty_terminal grid that gets the same bytes as the
//! real terminal, and a serializer that turns the grid back into escape
//! sequences.
//!
//! The server uses it for snapshots (a client attaches, or is far behind). The
//! client uses it to repaint the cells under its overlay. The serializer keeps
//! hyperlinks (OSC 8), all underline styles and colors, strikeout, wide and
//! combining characters, soft wraps, the palette, the title, the cursor shape
//! and the input modes. Resizes reflow the text.

use std::sync::{Arc, Mutex};

use alacritty_terminal::{
    Grid, Term,
    event::{Event, EventListener},
    grid::{Dimensions, Row},
    index::{Column, Line},
    term::{
        Config, Osc52, TermMode,
        cell::{Cell, Flags, Hyperlink},
    },
    vte::ansi::{CharsetIndex, Color, CursorShape, CursorStyle, NamedColor, Processor, StandardCharset, Timeout},
};

#[derive(Clone, Default)]
struct Title(Arc<Mutex<Option<String>>>);

impl EventListener for Title {
    fn send_event(&self, e: Event) {
        match e {
            Event::Title(t) => *self.0.lock().unwrap() = Some(t),
            Event::ResetTitle => *self.0.lock().unwrap() = None,
            _ => {}
        }
    }
}

struct Size(usize, usize);

impl Dimensions for Size {
    fn total_lines(&self) -> usize {
        self.1
    }
    fn screen_lines(&self) -> usize {
        self.1
    }
    fn columns(&self) -> usize {
        self.0
    }
}

/// Sequences that switch to the alternate screen.
const ALT_ON: [&[u8]; 3] = [b"\x1b[?1049h", b"\x1b[?1047h", b"\x1b[?47h"];

fn find(h: &[u8], n: &[u8]) -> Option<usize> {
    h.windows(n.len()).position(|w| w == n)
}

fn first_alt_on(h: &[u8]) -> Option<(usize, usize)> {
    ALT_ON.iter().filter_map(|p| find(h, p).map(|i| (i, p.len()))).min()
}

/// Put the terminal into a known state: end a string or sequence in progress,
/// and switch off modes that a program can leave on.
const RESET: &str = concat!(
    "\x1b\\\x18",
    "\x1b]8;;\x1b\\",
    "\x1b[0m\x1b(B\x1b[?25h\x1b[?1l\x1b>",
    "\x1b[?1000l\x1b[?1002l\x1b[?1003l\x1b[?1005l\x1b[?1006l\x1b[?1015l\x1b[?2004l\x1b[?1004l",
    "\x1b[?7h\x1b[?6l\x1b[4l\x1b[20l\x1b[?1007h",
    "\x1b[<99u\x1b[r",
);

/// How a snapshot starts on the client's screen.
#[derive(Clone, Copy, PartialEq, Debug)]
pub enum Prep {
    /// The screen is blank with the cursor at the top left.
    Blank,
    /// Scroll the old screen into the terminal's scrollback first.
    Scroll,
    /// Write over the old screen (it is not kept). Not with ED 2: some
    /// terminals move the screen into the scrollback then.
    Clear,
}

/// One cell, as the local echo prediction sees it.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct CellView {
    pub c: char,
    /// The second half of a wide character.
    pub spacer: bool,
    pub wide: bool,
    /// No colors, no attributes, a space.
    pub blank: bool,
    /// Foreground is the default color (not, for example, a dim suggestion).
    pub plain_fg: bool,
}

/// What to draw in one column of a composed row.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Src {
    /// The cell of this row at that column (shifted text).
    Orig(usize),
    /// A typed character, in the pen at the cursor.
    Char(char),
    /// A space in the pen at the cursor.
    Blank,
    /// Second half of a wide character: nothing to draw.
    Skip,
}

pub struct Mirror {
    term: Term<Title>,
    parser: Processor,
    title: Title,
    /// Copy of the primary screen, taken when a program enters the alternate
    /// screen (the primary screen does not change after that).
    primary: Option<Grid<Cell>>,
    carry: Vec<u8>,
}

impl Mirror {
    pub fn new(cols: u16, rows: u16, history: usize) -> Mirror {
        let title = Title::default();
        let config = Config { scrolling_history: history, kitty_keyboard: true, osc52: Osc52::Disabled, ..Default::default() };
        let term = Term::new(config, &Size(cols.max(2) as usize, rows.max(1) as usize), title.clone());
        Mirror { term, parser: Processor::new(), title, primary: None, carry: Vec::new() }
    }

    pub fn alt_screen(&self) -> bool {
        self.term.mode().contains(TermMode::ALT_SCREEN)
    }

    /// (columns, rows)
    pub fn size(&self) -> (usize, usize) {
        (self.term.columns(), self.term.screen_lines())
    }

    pub fn feed(&mut self, data: &[u8]) {
        let was_alt = self.alt_screen();
        let mut captured = false;
        // A switch sequence that started in the previous chunk.
        if !was_alt && !self.carry.is_empty() {
            let mut j = self.carry.clone();
            j.extend_from_slice(&data[..data.len().min(8)]);
            if first_alt_on(&j).is_some_and(|(i, _)| i < self.carry.len()) {
                self.primary = Some(self.term.grid().clone());
                captured = true;
            }
        }
        let mut rest = data;
        while let Some((i, n)) = first_alt_on(rest) {
            self.advance(&rest[..i]);
            if !self.alt_screen() {
                self.primary = Some(self.term.grid().clone());
                captured = true;
            }
            self.advance(&rest[i..i + n]);
            rest = &rest[i + n..];
        }
        self.advance(rest);
        if data.len() >= 8 {
            self.carry = data[data.len() - 8..].to_vec();
        } else {
            self.carry.extend_from_slice(data);
            let k = self.carry.len().saturating_sub(8);
            self.carry.drain(..k);
        }
        let now_alt = self.alt_screen();
        if !now_alt || (!was_alt && !captured) {
            self.primary = None;
        }
    }

    /// The mirror is never shown, so synchronized output (mode 2026) has no
    /// use here: apply the bytes at once, and the grid is always current.
    fn advance(&mut self, b: &[u8]) {
        self.parser.advance(&mut self.term, b);
        if self.parser.sync_timeout().pending_timeout() {
            self.parser.stop_sync(&mut self.term);
        }
    }

    /// Lines of scrollback.
    pub fn history_size(&self) -> usize {
        self.term.grid().history_size()
    }

    /// Cursor row and column, visible, and waiting to wrap.
    pub fn cursor_info(&self) -> (usize, usize, bool, bool) {
        let g = self.term.grid();
        let p = g.cursor.point;
        (p.line.0.max(0) as usize, p.column.0, self.term.mode().contains(TermMode::SHOW_CURSOR), g.cursor.input_needs_wrap)
    }

    /// The cells of a screen row.
    pub fn row_view(&self, row: usize) -> Vec<CellView> {
        let g = self.term.grid();
        if row >= g.screen_lines() {
            return Vec::new();
        }
        let r = &g[Line(row as i32)];
        (0..g.columns())
            .map(|c| {
                let x = &r[Column(c)];
                CellView {
                    c: x.c,
                    spacer: x.flags.contains(Flags::WIDE_CHAR_SPACER),
                    wide: x.flags.contains(Flags::WIDE_CHAR),
                    blank: blank(x) && x.flags & STYLE == Flags::empty() && x.fg == Color::Named(NamedColor::Foreground),
                    plain_fg: x.fg == Color::Named(NamedColor::Foreground) && !x.flags.contains(Flags::DIM),
                }
            })
            .collect()
    }

    /// Draw `items` on `row` from column `col`: original cells, or characters
    /// in the pen at the cursor (underlined when `mark`). Ends with the default
    /// pen and no link; the caller restores the cursor.
    pub fn compose(&self, row: usize, col: usize, items: &[Src], mark: bool) -> String {
        let g = self.term.grid();
        if row >= g.screen_lines() {
            return String::new();
        }
        let r = &g[Line(row as i32)];
        let mut typed = Pen::of(&g.cursor.template);
        if mark {
            typed.flags.insert(Flags::UNDERLINE);
        }
        let mut out = format!("\x1b[{};{}H", row + 1, col + 1);
        let (mut pen, mut link) = (Pen::default(), None);
        sgr(&mut out, &pen);
        for it in items {
            match *it {
                Src::Orig(c) if c < g.columns() => cell(&mut out, &r[Column(c)], &mut pen, &mut link),
                Src::Orig(_) | Src::Skip => {}
                Src::Char(_) | Src::Blank => {
                    if link.is_some() {
                        out.push_str("\x1b]8;;\x1b\\");
                        link = None;
                    }
                    if pen != typed {
                        sgr(&mut out, &typed);
                        pen = typed.clone();
                    }
                    out.push(if let Src::Char(ch) = *it { ch } else { ' ' });
                }
            }
        }
        end_line(&mut out, &mut pen, &mut link);
        out
    }

    /// Cells `col..col+width` of a row, blanks included, for drawing a view
    /// of the screen at another place. A wide character cut by an edge
    /// becomes a space. Starts and ends with the default pen.
    pub fn render_row(&self, row: usize, col: usize, width: usize) -> String {
        let g = self.term.grid();
        let mut out = String::new();
        let (mut pen, mut link) = (Pen::default(), None);
        sgr(&mut out, &pen);
        if row >= g.screen_lines() {
            out.push_str(&" ".repeat(width));
            return out;
        }
        let r = &g[Line(row as i32)];
        let cols = g.columns();
        let end = col + width;
        let mut c = col;
        while c < end {
            if c >= cols {
                end_line(&mut out, &mut pen, &mut link);
                out.push_str(&" ".repeat(end - c));
                break;
            }
            let x = &r[Column(c)];
            let cut = (x.flags.contains(Flags::WIDE_CHAR_SPACER) && c == col)
                || (x.flags.contains(Flags::WIDE_CHAR) && c + 1 >= end)
                || x.flags.contains(Flags::LEADING_WIDE_CHAR_SPACER);
            if cut {
                let mut blank_cell = x.clone();
                blank_cell.c = ' ';
                blank_cell.flags.remove(Flags::WIDE_CHAR | Flags::WIDE_CHAR_SPACER);
                cell(&mut out, &blank_cell, &mut pen, &mut link);
            } else {
                cell(&mut out, x, &mut pen, &mut link);
            }
            c += 1;
        }
        end_line(&mut out, &mut pen, &mut link);
        out
    }

    /// Origin mode (DECOM) is set. Then cursor positions are relative to the
    /// scroll region, which alacritty does not expose.
    pub fn origin_mode(&self) -> bool {
        self.term.mode().contains(TermMode::ORIGIN)
    }

    /// Bytes that let us draw at absolute positions with plain ASCII: no
    /// insert mode, the ASCII character set. `cursor_restore` undoes them.
    pub fn neutral(&self) -> String {
        let mut out = String::new();
        if self.term.mode().contains(TermMode::INSERT) {
            out.push_str("\x1b[4l");
        }
        if self.term.grid().cursor.charsets[CharsetIndex::G0] != StandardCharset::Ascii {
            out.push_str("\x1b(B");
        }
        out
    }

    pub fn resize(&mut self, cols: u16, rows: u16) {
        let (cols, rows) = (cols.max(2) as usize, rows.max(1) as usize);
        if (cols, rows) == self.size() {
            return;
        }
        self.term.resize(Size(cols, rows));
        if let Some(g) = &mut self.primary {
            g.resize::<Color>(true, rows, cols);
        }
    }

    /// Escape sequences that bring a terminal to the current state.
    ///
    /// `history`: None: the screen only. Some(n): also up to n lines of
    /// scrollback, and (on the alternate screen) the main screen under it.
    pub fn snapshot(&mut self, history: Option<usize>, prep: Prep) -> Vec<u8> {
        let rows = self.term.screen_lines();
        let mut out = String::from(RESET);
        let prepare = |out: &mut String| match prep {
            Prep::Blank => out.push_str("\x1b[H"),
            Prep::Clear => out.push_str("\x1b[H"),
            Prep::Scroll => {
                out.push_str(&format!("\x1b[{rows};1H"));
                out.push_str(&"\n".repeat(rows));
                out.push_str("\x1b[H");
            }
        };
        if self.alt_screen() {
            if let Some(n) = history {
                out.push_str("\x1b[?1049l");
                prepare(&mut out);
                if let Some(p) = &self.primary {
                    flow(&mut out, p, n, prep == Prep::Clear);
                    cursor_pos(&mut out, p);
                    out.push_str("\x1b[0m");
                }
            }
            out.push_str("\x1b[?1049h\x1b[H\x1b[2J");
            let g = self.term.grid();
            let (mut pen, mut link) = (Pen::default(), None);
            for r in 0..rows {
                out.push_str(&format!("\x1b[{};1H", r + 1));
                let _ = line(&mut out, &g[Line(r as i32)], g.columns(), &mut pen, &mut link, false);
            }
            end_line(&mut out, &mut pen, &mut link);
        } else {
            out.push_str("\x1b[?1049l");
            prepare(&mut out);
            flow(&mut out, self.term.grid(), history.unwrap_or(0), prep == Prep::Clear);
        }
        out.push_str(&palette(&self.term));
        out.push_str(&self.state());
        out.into_bytes()
    }

    /// Modes, title, cursor shape, cursor position, pen and cursor visibility.
    fn state(&self) -> String {
        let mut out = self.input_modes();
        out.push_str(&cursor_shape(self.term.cursor_style()));
        if let Some(t) = self.title() {
            out.push_str(&format!("\x1b]2;{t}\x07"));
        }
        out.push_str(&self.cursor_restore());
        out
    }

    pub fn title(&self) -> Option<String> {
        self.title.0.lock().unwrap().as_ref().map(|t| t.chars().filter(|c| !c.is_control()).collect())
    }

    pub fn cursor_shape(&self) -> String {
        cursor_shape(self.term.cursor_style())
    }

    /// The modes that change what the terminal sends: keys, mouse, paste,
    /// focus. Also wrap and new-line modes. (Sets modes that are on; a
    /// terminal after RESET has the rest off.)
    pub fn input_modes(&self) -> String {
        let mut out = String::new();
        let mode = *self.term.mode();
        for (m, on) in [
            (TermMode::APP_CURSOR, "\x1b[?1h"),
            (TermMode::APP_KEYPAD, "\x1b="),
            (TermMode::MOUSE_REPORT_CLICK, "\x1b[?1000h"),
            (TermMode::MOUSE_DRAG, "\x1b[?1002h"),
            (TermMode::MOUSE_MOTION, "\x1b[?1003h"),
            (TermMode::UTF8_MOUSE, "\x1b[?1005h"),
            (TermMode::SGR_MOUSE, "\x1b[?1006h"),
            (TermMode::BRACKETED_PASTE, "\x1b[?2004h"),
            (TermMode::FOCUS_IN_OUT, "\x1b[?1004h"),
            (TermMode::LINE_FEED_NEW_LINE, "\x1b[20h"),
        ] {
            if mode.contains(m) {
                out.push_str(on);
            }
        }
        if !mode.contains(TermMode::LINE_WRAP) {
            out.push_str("\x1b[?7l");
        }
        if !mode.contains(TermMode::ALTERNATE_SCROLL) {
            out.push_str("\x1b[?1007l");
        }
        let kitty = [
            (TermMode::DISAMBIGUATE_ESC_CODES, 1),
            (TermMode::REPORT_EVENT_TYPES, 2),
            (TermMode::REPORT_ALTERNATE_KEYS, 4),
            (TermMode::REPORT_ALL_KEYS_AS_ESC, 8),
            (TermMode::REPORT_ASSOCIATED_TEXT, 16),
        ]
        .iter()
        .filter(|(m, _)| mode.contains(*m))
        .map(|(_, v)| v)
        .sum::<u32>();
        if kitty != 0 {
            out.push_str(&format!("\x1b[>{kitty}u"));
        }
        out
    }

    /// Cursor position, pen and visibility (after a repaint or an overlay).
    pub fn cursor_restore(&self) -> String {
        let g = self.term.grid();
        let mut out = String::new();
        cursor_pos(&mut out, g);
        let t = &g.cursor.template;
        sgr(&mut out, &Pen::of(t));
        if let Some(h) = t.hyperlink() {
            open_link(&mut out, &h);
        }
        if g.cursor.charsets[CharsetIndex::G0] == StandardCharset::SpecialCharacterAndLineDrawing {
            out.push_str("\x1b(0");
        }
        if self.term.mode().contains(TermMode::INSERT) {
            out.push_str("\x1b[4h");
        }
        out.push_str(if self.term.mode().contains(TermMode::SHOW_CURSOR) { "\x1b[?25h" } else { "\x1b[?25l" });
        out
    }

    /// Redraw `width` cells of screen row `row` from column `col`, then restore
    /// the cursor. Used to remove an overlay.
    /// Bytes that draw the cells of `rows` x `col..col+width` again (wide
    /// characters at the edges are drawn whole) and then restore the cursor.
    pub fn repaint(&self, rows: std::ops::Range<usize>, col: usize, width: usize) -> String {
        let g = self.term.grid();
        let (cols, lines) = (g.columns(), g.screen_lines());
        let mut out = format!("\x1b]8;;\x1b\\{}", self.neutral());
        for row in rows.start..rows.end.min(lines) {
            if col >= cols || width == 0 {
                break;
            }
            let r = &g[Line(row as i32)];
            let mut start = col;
            while start > 0 && r[Column(start)].flags.contains(Flags::WIDE_CHAR_SPACER) {
                start -= 1;
            }
            let mut end = (col + width).min(cols);
            if end < cols && r[Column(end - 1)].flags.contains(Flags::WIDE_CHAR) {
                end += 1;
            }
            out.push_str(&format!("\x1b[{};{}H", row + 1, start + 1));
            let (mut pen, mut link) = (Pen::default(), None);
            sgr(&mut out, &pen);
            for c in start..end {
                cell(&mut out, &r[Column(c)], &mut pen, &mut link);
            }
            end_line(&mut out, &mut pen, &mut link);
        }
        out.push_str(&self.cursor_restore());
        out
    }

    /// The visible text, one line per row (for tests).
    #[cfg(test)]
    pub fn text(&self) -> String {
        let g = self.term.grid();
        (0..g.screen_lines())
            .map(|r| row_text(&g[Line(r as i32)], g.columns()))
            .collect::<Vec<_>>()
            .join("\n")
    }

    /// All text, scrollback first (for tests).
    #[cfg(test)]
    pub fn all_text(&self) -> String {
        let g = self.term.grid();
        let h = g.history_size() as i32;
        (-h..g.screen_lines() as i32).map(|r| row_text(&g[Line(r)], g.columns())).collect::<Vec<_>>().join("\n")
    }

    #[cfg(test)]
    pub fn cursor(&self) -> (i32, usize) {
        let p = self.term.grid().cursor.point;
        (p.line.0, p.column.0)
    }

    #[cfg(test)]
    pub fn cell_at(&self, row: i32, col: usize) -> Cell {
        self.term.grid()[Line(row)][Column(col)].clone()
    }

    #[cfg(test)]
    pub fn mode(&self) -> TermMode {
        *self.term.mode()
    }
}

#[cfg(test)]
fn row_text(r: &Row<Cell>, cols: usize) -> String {
    let mut s = String::new();
    for c in 0..cols {
        let cell = &r[Column(c)];
        if !cell.flags.intersects(Flags::WIDE_CHAR_SPACER | Flags::LEADING_WIDE_CHAR_SPACER) {
            s.push(cell.c);
        }
    }
    s.trim_end().to_string()
}

#[derive(Clone, PartialEq)]
struct Pen {
    fg: Color,
    bg: Color,
    flags: Flags,
    ul: Option<Color>,
}

const STYLE: Flags = Flags::BOLD
    .union(Flags::DIM)
    .union(Flags::ITALIC)
    .union(Flags::ALL_UNDERLINES)
    .union(Flags::INVERSE)
    .union(Flags::HIDDEN)
    .union(Flags::STRIKEOUT);

impl Default for Pen {
    fn default() -> Self {
        Pen { fg: Color::Named(NamedColor::Foreground), bg: Color::Named(NamedColor::Background), flags: Flags::empty(), ul: None }
    }
}

impl Pen {
    fn of(c: &Cell) -> Pen {
        Pen { fg: c.fg, bg: c.bg, flags: c.flags & STYLE, ul: c.underline_color() }
    }
}

fn color(out: &mut String, c: Color, base: u8, bright: u8, ext: u8) {
    match c {
        Color::Named(n) => {
            let i = n as usize;
            let i = if (NamedColor::DimBlack as usize..=NamedColor::DimWhite as usize).contains(&i) {
                i - NamedColor::DimBlack as usize
            } else {
                i
            };
            if i < 8 {
                out.push_str(&format!(";{}", base as usize + i));
            } else if i < 16 {
                out.push_str(&format!(";{}", bright as usize + i - 8));
            }
        }
        Color::Indexed(i) => out.push_str(&format!(";{ext};5;{i}")),
        Color::Spec(rgb) => out.push_str(&format!(";{ext};2;{};{};{}", rgb.r, rgb.g, rgb.b)),
    }
}

fn sgr(out: &mut String, p: &Pen) {
    out.push_str("\x1b[0");
    let f = p.flags;
    for (flag, code) in [(Flags::BOLD, ";1"), (Flags::DIM, ";2"), (Flags::ITALIC, ";3")] {
        if f.contains(flag) {
            out.push_str(code);
        }
    }
    if f.contains(Flags::UNDERCURL) {
        out.push_str(";4:3");
    } else if f.contains(Flags::DOUBLE_UNDERLINE) {
        out.push_str(";4:2");
    } else if f.contains(Flags::DOTTED_UNDERLINE) {
        out.push_str(";4:4");
    } else if f.contains(Flags::DASHED_UNDERLINE) {
        out.push_str(";4:5");
    } else if f.contains(Flags::UNDERLINE) {
        out.push_str(";4");
    }
    for (flag, code) in [(Flags::INVERSE, ";7"), (Flags::HIDDEN, ";8"), (Flags::STRIKEOUT, ";9")] {
        if f.contains(flag) {
            out.push_str(code);
        }
    }
    color(out, p.fg, 30, 90, 38);
    color(out, p.bg, 40, 100, 48);
    if let Some(u) = p.ul {
        match u {
            Color::Named(n) if (n as usize) < 16 => out.push_str(&format!(";58;5;{}", n as usize)),
            Color::Named(_) => {}
            other => color(out, other, 0, 0, 58),
        }
    }
    out.push('m');
}

fn open_link(out: &mut String, h: &Hyperlink) {
    // alacritty makes up "<n>_alacritty" ids for links without one: leave those out.
    match h.id() {
        id if id.ends_with("_alacritty") => out.push_str(&format!("\x1b]8;;{}\x1b\\", h.uri())),
        id => out.push_str(&format!("\x1b]8;id={id};{}\x1b\\", h.uri())),
    }
}

fn blank(c: &Cell) -> bool {
    c.c == ' '
        && c.bg == Color::Named(NamedColor::Background)
        && !c.flags.intersects(Flags::INVERSE | Flags::ALL_UNDERLINES | Flags::STRIKEOUT)
        && c.extra.is_none()
}

fn cell(out: &mut String, c: &Cell, pen: &mut Pen, link: &mut Option<Hyperlink>) {
    if c.flags.intersects(Flags::WIDE_CHAR_SPACER | Flags::LEADING_WIDE_CHAR_SPACER) {
        return;
    }
    let p = Pen::of(c);
    if p != *pen {
        sgr(out, &p);
        *pen = p;
    }
    let h = c.hyperlink();
    if h != *link {
        match &h {
            Some(h) => open_link(out, h),
            None => out.push_str("\x1b]8;;\x1b\\"),
        }
        *link = h;
    }
    out.push(c.c);
    if let Some(z) = c.zerowidth() {
        out.extend(z.iter());
    }
}

fn end_line(out: &mut String, pen: &mut Pen, link: &mut Option<Hyperlink>) {
    if link.is_some() {
        out.push_str("\x1b]8;;\x1b\\");
        *link = None;
    }
    if *pen != Pen::default() {
        out.push_str("\x1b[0m");
        *pen = Pen::default();
    }
}

/// Write one row. Trailing blank cells are left out, except when the row
/// continues on the next row (soft wrap) or `full` is set. Returns true for a
/// soft wrap, and the number of columns written.
fn line(out: &mut String, r: &Row<Cell>, cols: usize, pen: &mut Pen, link: &mut Option<Hyperlink>, full: bool) -> (bool, usize) {
    let wrapped = r[Column(cols - 1)].flags.contains(Flags::WRAPLINE);
    let end = if wrapped || full { cols } else { (0..cols).rev().find(|&c| !blank(&r[Column(c)])).map_or(0, |c| c + 1) };
    for c in 0..end {
        cell(out, &r[Column(c)], pen, link);
    }
    (wrapped, end)
}

/// Print rows top to bottom from a blank screen with CR LF, so that the
/// terminal scrolls the first rows into its own scrollback.
/// `erase`: write over old text (erase the rest of each line).
fn flow(out: &mut String, g: &Grid<Cell>, history: usize, erase: bool) {
    let rows = g.screen_lines() as i32;
    let start = -(g.history_size().min(history) as i32);
    let (mut pen, mut link) = (Pen::default(), None);
    // A full row leaves the cursor waiting to wrap at the last column: EL
    // there would erase the last character.
    let mut full = false;
    for l in start..rows {
        let (wrapped, end) = line(out, &g[Line(l)], g.columns(), &mut pen, &mut link, false);
        full = end >= g.columns();
        if !wrapped && l != rows - 1 {
            // A new line gets the current background in many terminals.
            end_line(out, &mut pen, &mut link);
            if erase && !full {
                out.push_str("\x1b[K");
            }
            out.push_str("\r\n");
        }
    }
    end_line(out, &mut pen, &mut link);
    // All screen rows were written: the last one is the bottom row.
    if erase && !full {
        out.push_str("\x1b[K");
    }
}

fn cursor_pos(out: &mut String, g: &Grid<Cell>) {
    let p = g.cursor.point;
    let row = p.line.0 + 1;
    if g.cursor.input_needs_wrap {
        // Print the last cell again: the terminal then also waits to wrap.
        let cols = g.columns();
        let r = &g[p.line];
        let last = if cols >= 2 && r[Column(cols - 1)].flags.contains(Flags::WIDE_CHAR_SPACER) { cols - 2 } else { cols - 1 };
        out.push_str(&format!("\x1b[{row};{}H", last + 1));
        let (mut pen, mut link) = (Pen::default(), None);
        cell(out, &r[Column(last)], &mut pen, &mut link);
        end_line(out, &mut pen, &mut link);
    } else {
        out.push_str(&format!("\x1b[{row};{}H", p.column.0 + 1));
    }
}

fn cursor_shape(s: CursorStyle) -> String {
    if s == CursorStyle::default() {
        return "\x1b[0 q".to_string();
    }
    let n = match (s.shape, s.blinking) {
        (CursorShape::Block | CursorShape::HollowBlock, true) => 1,
        (CursorShape::Block | CursorShape::HollowBlock, false) => 2,
        (CursorShape::Underline, true) => 3,
        (CursorShape::Underline, false) => 4,
        (CursorShape::Beam, true) => 5,
        (CursorShape::Beam, false) => 6,
        (CursorShape::Hidden, _) => return String::new(),
    };
    format!("\x1b[{n} q")
}

/// Palette changes that programs made (OSC 4, 10, 11, 12).
fn palette(t: &Term<Title>) -> String {
    let colors = t.colors();
    let mut s = String::new();
    for i in 0..256 {
        if let Some(c) = colors[i] {
            s.push_str(&format!("\x1b]4;{i};rgb:{:02x}/{:02x}/{:02x}\x1b\\", c.r, c.g, c.b));
        }
    }
    for (n, code) in [(NamedColor::Foreground, 10), (NamedColor::Background, 11), (NamedColor::Cursor, 12)] {
        if let Some(c) = colors[n] {
            s.push_str(&format!("\x1b]{code};rgb:{:02x}/{:02x}/{:02x}\x1b\\", c.r, c.g, c.b));
        }
    }
    if s.is_empty() { s } else { format!("\x1b]104\x1b\\\x1b]110\x1b\\\x1b]111\x1b\\\x1b]112\x1b\\{s}") }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn roundtrip(m: &mut Mirror, history: bool) -> Mirror {
        let (c, r) = m.size();
        let snap = m.snapshot(history.then_some(usize::MAX), Prep::Scroll);
        let mut n = Mirror::new(c as u16, r as u16, 1000);
        n.feed(b"old local text that must not stay\r\n$ ");
        n.feed(&snap);
        n
    }

    #[test]
    fn text_cursor_and_history() {
        let mut m = Mirror::new(20, 5, 1000);
        for i in 0..12 {
            m.feed(format!("line {i}\r\n").as_bytes());
        }
        m.feed(b"\x1b[31mred\x1b[0m $ ");
        let n = roundtrip(&mut m, true);
        assert_eq!(n.text(), m.text());
        assert_eq!(n.cursor(), m.cursor());
        assert!(n.all_text().contains("line 0"));
        assert!(!n.text().contains("old local"));
    }

    #[test]
    fn attributes_links_and_wide_chars() {
        let mut m = Mirror::new(30, 4, 100);
        m.feed(b"\x1b[1;3;4:3;58:2::255:0:0;9;38;2;1;2;3;48;5;17mX\x1b[0m");
        m.feed(b" \x1b]8;id=a;https://example.com\x1b\\link\x1b]8;;\x1b\\ ");
        m.feed("宽字e\u{301}".as_bytes());
        let n = roundtrip(&mut m, false);
        assert_eq!(n.text(), m.text());
        let (a, b) = (m.cell_at(0, 0), n.cell_at(0, 0));
        assert_eq!((a.fg, a.bg, a.flags, a.underline_color()), (b.fg, b.bg, b.flags, b.underline_color()));
        assert!(b.flags.contains(Flags::UNDERCURL | Flags::STRIKEOUT | Flags::BOLD | Flags::ITALIC));
        assert_eq!(n.cell_at(0, 3).hyperlink().map(|h| h.uri().to_string()), Some("https://example.com".into()));
        assert!(n.cell_at(0, 7).flags.contains(Flags::WIDE_CHAR));
        assert_eq!(n.cell_at(0, 11).zerowidth(), Some(&['\u{301}'][..]));
    }

    #[test]
    fn soft_wrap_and_pending_wrap() {
        let mut m = Mirror::new(10, 4, 100);
        m.feed(b"0123456789abcdefghij");
        let n = roundtrip(&mut m, false);
        assert_eq!(n.text(), m.text());
        assert_eq!(n.cursor(), m.cursor());
        assert!(n.cell_at(0, 9).flags.contains(Flags::WRAPLINE));
        let (mut a, mut b) = (m, n);
        a.feed(b"Z");
        b.feed(b"Z");
        assert_eq!(a.text(), b.text());
    }

    #[test]
    fn alt_screen_keeps_primary() {
        let mut m = Mirror::new(20, 4, 100);
        m.feed(b"shell 1\r\nshell 2\r\n$ vim");
        m.feed(b"\x1b[?10");
        m.feed(b"49h\x1b[H\x1b[2Jeditor text\x1b[?2004h\x1b[>1u");
        assert!(m.alt_screen());
        let mut n = roundtrip(&mut m, true);
        assert!(n.alt_screen());
        assert_eq!(n.text(), m.text());
        assert!(n.mode().contains(TermMode::BRACKETED_PASTE | TermMode::DISAMBIGUATE_ESC_CODES));
        n.feed(b"\x1b[?1049l");
        assert!(n.text().contains("shell 2"));
        assert!(n.text().contains("$ vim"));
    }

    #[test]
    fn reflow_on_resize() {
        let mut m = Mirror::new(20, 4, 100);
        m.feed(b"aaaaaaaaaabbbbbbbbbbcccc\r\n$ ");
        m.resize(10, 4);
        let n = roundtrip(&mut m, true);
        // The old local text goes into the scrollback first.
        assert!(n.all_text().replace('\n', "").ends_with(&m.all_text().replace('\n', "")));
        assert!(n.all_text().contains("aaaaaaaaaa\nbbbbbbbbbb\ncccc"));
    }

    #[test]
    fn repaint_restores_cells() {
        let mut m = Mirror::new(20, 3, 0);
        m.feed(b"\x1b[32mhello world\x1b[0m\r\n$ ");
        let mut real = Mirror::new(20, 3, 0);
        real.feed(b"\x1b[32mhello world\x1b[0m\r\n$ ");
        real.feed(b"\x1b[1;5H\x1b[7m OVERLAY \x1b[0m\x1b[2;3H");
        real.feed(m.repaint(0..1, 4, 9).as_bytes());
        assert_eq!(real.text(), m.text());
        assert_eq!(real.cursor(), m.cursor());
        assert_eq!(real.cell_at(0, 6).fg, m.cell_at(0, 6).fg);
    }

    #[test]
    fn repaint_in_insert_mode_and_line_drawing() {
        let feed = b"abcdefghij\r\n\x1b(0qqqq\x1b[4h";
        let mut m = Mirror::new(20, 3, 0);
        m.feed(feed);
        let mut real = Mirror::new(20, 3, 0);
        real.feed(feed);
        real.feed(format!("{}\x1b[1;3H XY {}", m.neutral(), m.cursor_restore()).as_bytes());
        real.feed(m.repaint(0..1, 2, 4).as_bytes());
        real.feed(b"q");
        m.feed(b"q");
        assert_eq!(real.text(), m.text());
        assert!(m.text().contains("abcdefghij"));
        assert!(m.text().contains("─────"));
    }

    #[test]
    fn title_palette_and_cursor_shape() {
        let mut m = Mirror::new(20, 3, 0);
        m.feed(b"\x1b]2;my title\x07\x1b]4;1;rgb:12/34/56\x1b\\\x1b[5 q");
        let snap = String::from_utf8(m.snapshot(None, Prep::Blank)).unwrap();
        assert!(snap.contains("\x1b]2;my title\x07"));
        assert!(snap.contains("\x1b]4;1;rgb:12/34/56"));
        assert!(snap.contains("\x1b[5 q"));
    }
}
