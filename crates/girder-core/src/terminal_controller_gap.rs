//! A plain-text screen model for a terminal: it parses the ANSI/VT sequences a shell emits and
//! projects the result as lines a reader or a model can consume.
//!
//! Upstream's `packages/api/terminal-controller` keeps a headless `@xterm/headless` emulator per
//! terminal and derives everything a non-rendering reader sees from it (`screen.ts`):
//!
//! ```text
//! readScreen(request) = pageLines(renderLines(buffer, 0, lastContentRow(buffer) + 1), request)
//! ```
//!
//! This crate has no emulator at all. [`crate::terminal_buffer`] retains *bytes*, [`crate::terminal_attach`]
//! hands those bytes to followers by offset, and [`crate::terminal_tool`] returns the retained
//! transcript verbatim — so a model reading a terminal today sees `\u{1b}[2K\u{1b}[1;32m$ \u{1b}[0m`
//! where a person sees a fresh line reading `$ `. Raw bytes are the wrong shape twice over: they carry
//! cursor motion a reader cannot apply, and they count the escape sequences against the reader's
//! budget.
//!
//! # What is modelled, and what is deliberately not
//!
//! The model is a *minimal but correct* VT subset: a grid of cells, a cursor, scroll margins, the
//! alternate screen, the erase/insert/delete families, and the cursor motions that full-screen
//! programs use to paint. Colour and attributes (`SGR`) are parsed and discarded — the output of this
//! module is text, so a weight or a hue has nowhere to go. The ignored families are listed together
//! in [`Screen::feed`] so a reader can tell "not implemented" from "silently dropped".
//!
//! What is *not* approximated is the part that decides what text a reader sees: the deferred
//! wrap at the last column, wide (East Asian) characters occupying two columns, combining marks
//! attached to their base, and wrapped rows joined back into one logical line. Those four are the
//! difference between a screen that reads like the terminal and a strip of misaligned text.
//!
//! # Why the projection is paged rather than truncated
//!
//! [`crate::terminal_tool`] returns the transcript "whole, with what it dropped" because a model
//! asking for output wants the newest bytes. A screen is asked for by *position*: "the last 80
//! lines", "20 lines back". [`ScreenRequest`] therefore carries a newest-relative offset, a line
//! count and a byte bound, and [`ScreenPage`] answers with where the returned text sits relative to
//! the newest line — the same contract as upstream's `TerminalScreenRequest`/`TerminalScreenPage`,
//! including the fact that a byte bound drops the *oldest* selected lines rather than cutting the
//! newest.
//!
//! # Example
//!
//! ```text
//! let mut screen = Screen::new(20, 4, 100)?;
//! screen.feed(b"\x1b[1;32mhello\x1b[0m\r\nworld");
//! let page = screen.read_screen(&ScreenRequest::new(0, 10, 1024));
//! assert_eq!(page.text, "hello\nworld");
//! assert_eq!(page.total_lines, 2);
//! ```
//!
//! # Scope
//!
//! This module owns the emulator and the projection only: no process, no transport, no session. A
//! caller feeds the bytes it already retains and reads lines back.

use std::collections::VecDeque;
use std::fmt;

/// The largest column count a screen may be created with.
pub const MAX_COLS: usize = 4096;

/// The largest row count a screen may be created with.
pub const MAX_ROWS: usize = 1024;

/// The largest scrollback a screen may be asked to retain, in rows.
pub const MAX_SCROLLBACK: usize = 100_000;

/// The largest value a CSI parameter may take.
///
/// Bounding it here keeps every count parameter from turning into an unbounded loop: a sequence
/// claiming to move the cursor ten billion rows would otherwise be a denial of service in one line
/// of output.
const MAX_PARAM: u32 = 1_000_000;

/// Columns between the default tab stops.
const TAB_WIDTH: usize = 8;

/// Why a screen could not be created.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ScreenError {
    /// A terminal dimension is zero or beyond what this model allocates for.
    Size {
        /// Requested columns.
        cols: usize,
        /// Requested rows.
        rows: usize,
    },
    /// The requested scrollback is beyond what this model retains.
    Scrollback {
        /// Requested scrollback rows.
        scrollback: usize,
    },
}

impl fmt::Display for ScreenError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match *self {
            Self::Size { cols, rows } => write!(
                f,
                "a {cols}x{rows} terminal is not usable: both dimensions must be at least 1 and at \
                 most {MAX_COLS}x{MAX_ROWS}"
            ),
            Self::Scrollback { scrollback } => write!(
                f,
                "{scrollback} scrollback rows exceeds the {MAX_SCROLLBACK} this screen retains"
            ),
        }
    }
}

impl std::error::Error for ScreenError {}

/// What a caller asks a screen for: a newest-relative page of rendered lines.
///
/// The three fields mirror upstream's `TerminalScreenRequest` exactly, because a client that pages a
/// screen locally and a host that pages it remotely have to agree on what "20 lines back" means.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ScreenRequest {
    /// Lines to skip back from the newest line.
    pub offset: usize,
    /// Maximum lines to return.
    pub count: usize,
    /// Maximum UTF-8 bytes of returned text; the newest bytes are kept.
    pub max_bytes: usize,
}

impl ScreenRequest {
    /// A request for `count` lines starting `offset` lines back from the newest.
    pub fn new(offset: usize, count: usize, max_bytes: usize) -> Self {
        Self {
            offset,
            count,
            max_bytes,
        }
    }
}

impl Default for ScreenRequest {
    fn default() -> Self {
        Self {
            offset: 0,
            count: 24,
            max_bytes: 64 * 1024,
        }
    }
}

/// A newest-relative page of rendered screen lines.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct ScreenPage {
    /// Rendered lines joined by newlines; wrapped rows are already joined.
    pub text: String,
    /// Logical lines currently retained, including scrollback.
    pub total_lines: usize,
    /// Inclusive newest-relative offset of the page (`request.offset`).
    pub line_begin: usize,
    /// Exclusive newest-relative offset after the returned lines.
    pub line_end: usize,
    /// Whether the byte bound dropped lines or cut a line inside the requested range.
    pub truncated: bool,
}

/// The lines between a mark and the newest content, and whether the mark was trimmed away.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct MarkedLines {
    /// Rendered lines, oldest first.
    pub lines: Vec<String>,
    /// Whether scrollback trimming discarded the marked row, so rendering started at the oldest row.
    pub truncated: bool,
}

/// A screen position that follows scrollback trimming, used to render output written after it.
///
/// Upstream's `TerminalScreenMark` holds an `xterm` marker that moves with trimming. Here the mark is
/// the absolute index of the cursor's logical line plus the buffer it was taken in: a mark is
/// answered by comparing both against the screen it is used on, which is what makes a trimmed mark
/// report [`MarkedLines::truncated`] instead of silently rendering the wrong rows.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ScreenMark {
    row: usize,
    alternate: bool,
}

impl ScreenMark {
    /// The absolute row the mark was taken at.
    pub fn row(self) -> usize {
        self.row
    }

    /// Render from the marked row through the newest content row of `screen`.
    ///
    /// `truncated` is set in two cases, both matching upstream: the marked row was trimmed out of
    /// scrollback (rendering then starts at the oldest retained row), or the screen switched between
    /// the main and alternate buffers and the mark no longer names a row in the active one.
    pub fn lines(self, screen: &Screen) -> MarkedLines {
        let end = screen.last_content_row() + 1;
        if self.alternate != screen.modes.alternate {
            return MarkedLines {
                lines: screen.render_lines(0, end),
                truncated: false,
            };
        }
        if self.row < screen.dropped {
            return MarkedLines {
                lines: screen.render_lines(0, end),
                truncated: true,
            };
        }
        MarkedLines {
            lines: screen.render_lines(self.row, end),
            truncated: false,
        }
    }
}

/// One cell of the grid.
#[derive(Clone, Debug, PartialEq, Eq)]
enum Cell {
    /// A cell never written, or blanked by an erase.
    Blank,
    /// The second column of the wide character to its left; renders as nothing.
    Tail,
    /// One character occupying one column.
    Char(char),
    /// A character followed by combining marks that occupy no column.
    Cluster(char, String),
}

/// One row of the grid.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
struct Row {
    /// Cells, one per column up to the last one written.
    cells: Vec<Cell>,
    /// Whether this row continues the logical line above it.
    wrapped: bool,
}

impl Row {
    fn blank() -> Self {
        Self::default()
    }

    /// Whether nothing was ever written to this row, or everything was erased.
    fn is_blank(&self) -> bool {
        self.cells.iter().all(|cell| matches!(cell, Cell::Blank))
    }
}

/// Terminal modes that change how bytes are applied.
// Four independent VT modes rather than one state: each is set by its own `DECSET` number, none
// implies another, and a two-variant enum would have to name a combination instead of a fact.
#[allow(clippy::struct_excessive_bools)]
#[derive(Clone, Copy, Debug)]
struct Modes {
    auto_wrap: bool,
    cursor_visible: bool,
    alternate: bool,
    /// The last column was written and the next printable character wraps.
    pending_wrap: bool,
}

impl Default for Modes {
    fn default() -> Self {
        Self {
            auto_wrap: true,
            cursor_visible: true,
            alternate: false,
            pending_wrap: false,
        }
    }
}

/// Where the parser is in an escape sequence.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
enum State {
    #[default]
    Ground,
    Escape,
    /// A charset designator (`ESC ( B`): one byte is consumed and dropped.
    Designate,
    Csi,
    /// `OSC`, `DCS`, `SOS`, `PM` and `APC` payloads: consumed until `BEL` or `ST` and discarded.
    Ignore,
}

/// A cursor position inside the visible grid.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
struct Cursor {
    row: usize,
    col: usize,
}

/// The main buffer, parked while the alternate screen is active.
#[derive(Debug)]
struct Parked {
    grid: Vec<Row>,
    history: VecDeque<Row>,
    cursor: Cursor,
    saved: Option<Cursor>,
    margins: (usize, usize),
    /// The absolute origin of the parked history: restoring it must restore the row numbering too.
    dropped: usize,
}

/// The parser: where it is, and the parameters of the sequence it is reading.
#[derive(Debug)]
struct Parser {
    state: State,
    params: Vec<Option<u32>>,
    pending: Option<u32>,
    started: bool,
    private: Option<char>,
    /// UTF-8 bytes of a character split across two `feed` calls.
    partial_utf8: Vec<u8>,
}

impl Default for Parser {
    fn default() -> Self {
        Self {
            state: State::Ground,
            params: Vec::new(),
            pending: None,
            started: false,
            private: None,
            partial_utf8: Vec::new(),
        }
    }
}

/// A terminal screen: a bounded grid, its scrollback, and the parser that fills them.
///
/// The grid always holds exactly [`Screen::rows`] rows; rows scrolled off the top move to a bounded
/// scrollback so a reader can page back, and are counted in [`Screen::dropped_rows`] once the
/// scrollback itself overflows. That count is what makes a [`ScreenMark`] able to say "the row you
/// marked is gone" instead of rendering whatever now sits at that index.
#[derive(Debug)]
pub struct Screen {
    cols: usize,
    rows: usize,
    scrollback: usize,
    grid: Vec<Row>,
    history: VecDeque<Row>,
    /// Rows discarded from the front of the retained buffer; absolute indices still count them.
    dropped: usize,
    cursor: Cursor,
    saved: Option<Cursor>,
    /// Inclusive top and bottom rows a line feed scrolls within.
    margins: (usize, usize),
    modes: Modes,
    main: Option<Parked>,
    /// The last printable character, for `REP`.
    last_printed: Option<char>,
    /// Replies a caller has not collected yet: cursor-position and device-attribute reports.
    responses: Vec<String>,
    parser: Parser,
}

impl Screen {
    /// Create a screen `cols` by `rows` that retains `scrollback` rows above the grid.
    ///
    /// A zero dimension is refused rather than clamped: a screen with no columns renders every
    /// character as an overwrite, which would look like corrupted output rather than a bad request.
    pub fn new(cols: usize, rows: usize, scrollback: usize) -> Result<Self, ScreenError> {
        if cols == 0 || rows == 0 || cols > MAX_COLS || rows > MAX_ROWS {
            return Err(ScreenError::Size { cols, rows });
        }
        if scrollback > MAX_SCROLLBACK {
            return Err(ScreenError::Scrollback { scrollback });
        }
        Ok(Self {
            cols,
            rows,
            scrollback,
            grid: vec![Row::blank(); rows],
            history: VecDeque::new(),
            dropped: 0,
            cursor: Cursor::default(),
            saved: None,
            margins: (0, rows - 1),
            modes: Modes::default(),
            main: None,
            last_printed: None,
            responses: Vec::new(),
            parser: Parser::default(),
        })
    }

    /// Columns in the grid.
    pub fn cols(&self) -> usize {
        self.cols
    }

    /// Rows in the grid.
    pub fn rows(&self) -> usize {
        self.rows
    }

    /// Rows discarded from the front of the retained buffer over this screen's whole life.
    pub fn dropped_rows(&self) -> usize {
        self.dropped
    }

    /// Absolute row count of the retained buffer: scrollback plus grid.
    pub fn total_rows(&self) -> usize {
        self.dropped + self.history.len() + self.grid.len()
    }

    /// Cursor position inside the grid, zero-based.
    pub fn cursor(&self) -> (usize, usize) {
        (self.cursor.row, self.cursor.col)
    }

    /// Whether the cursor is currently shown (`DECTCEM`).
    pub fn cursor_visible(&self) -> bool {
        self.modes.cursor_visible
    }

    /// Whether the alternate screen is active.
    pub fn alternate_screen(&self) -> bool {
        self.modes.alternate
    }

    /// Apply terminal output.
    ///
    /// The parser accepts the control families a shell and a full-screen program emit: `CSI`
    /// (cursor motion, erase, insert/delete, scroll, mode set/reset, device status), `ESC` (save and
    /// restore cursor, index, next line, reverse index, reset), `OSC`/`DCS` and the C1 single-byte
    /// forms. Sequences this model does not act on are consumed and dropped rather than printed:
    /// `SGR` (colour and attributes), charset designators, keypad mode, mouse and focus reporting,
    /// bracketed paste, cursor style, window manipulation and tab-stop setting. Printing them would
    /// put escape codes in front of a reader; buffering them would buy nothing, because nothing here
    /// renders attributes.
    ///
    /// Invalid UTF-8 is replaced by `U+FFFD`, and a character split across two calls is completed by
    /// the next one instead of being replaced now.
    pub fn feed(&mut self, bytes: &[u8]) {
        let mut buf = std::mem::take(&mut self.parser.partial_utf8);
        buf.extend_from_slice(bytes);
        let split = utf8_complete_prefix(&buf);
        let (ready, rest) = buf.split_at(split);
        self.parser.partial_utf8 = rest.to_vec();
        let text = String::from_utf8_lossy(ready).into_owned();
        for ch in text.chars() {
            self.feed_char(ch);
        }
    }

    /// Apply terminal output that is already valid UTF-8.
    pub fn feed_str(&mut self, text: &str) {
        self.feed(text.as_bytes());
    }

    /// Change the grid size, keeping the content that still fits.
    ///
    /// Growth adds blank rows below and blank columns to the right. Shrinking rows moves the rows cut
    /// off the top into scrollback, as a terminal does when its window gets shorter, so the content
    /// a reader was looking at survives. Columns are truncated without reflow: this model keeps no
    /// record of where a logical line was wrapped before, and guessing would re-break lines that
    /// still fit.
    pub fn resize(&mut self, cols: usize, rows: usize) -> Result<(), ScreenError> {
        if cols == 0 || rows == 0 || cols > MAX_COLS || rows > MAX_ROWS {
            return Err(ScreenError::Size { cols, rows });
        }
        for row in &mut self.grid {
            row.cells.truncate(cols);
        }
        if rows < self.grid.len() {
            let cut = self.grid.len() - rows;
            for _ in 0..cut {
                if let Some(row) = (!self.grid.is_empty()).then(|| self.grid.remove(0)) {
                    self.push_history(row);
                }
            }
        } else {
            self.grid.resize(rows, Row::blank());
        }
        self.cols = cols;
        self.rows = rows;
        self.cursor.row = self.cursor.row.min(rows - 1);
        // A pending wrap was about the old width: after a resize the cursor moves to the column that
        // character would have occupied, so a widened pane continues the line instead of
        // overwriting the character that filled the last column.
        if self.modes.pending_wrap {
            self.cursor.col = (self.cursor.col + 1).min(cols - 1);
        }
        self.cursor.col = self.cursor.col.min(cols - 1);
        self.margins = (0, rows - 1);
        self.modes.pending_wrap = false;
        Ok(())
    }

    /// Rows a caller can address: absolute indices `0..total_rows()`.
    ///
    /// A range that starts before the oldest retained row is clamped to it, so a caller that kept an
    /// old index gets the oldest text instead of a panic.
    pub fn render_lines(&self, from: usize, to: usize) -> Vec<String> {
        let begin = from.max(self.dropped);
        let end = to.min(self.total_rows());
        let mut lines = Vec::new();
        let mut current: Option<String> = None;
        for index in begin..end {
            let Some(row) = self.row(index) else { continue };
            let text = self.row_text(row);
            match &mut current {
                Some(line) if row.wrapped => line.push_str(&text),
                Some(line) => {
                    lines.push(line.trim_end().to_string());
                    current = Some(text);
                }
                None => current = Some(text),
            }
        }
        if let Some(line) = current {
            lines.push(line.trim_end().to_string());
        }
        lines
    }

    /// Render the whole retained buffer, as a reader asking for "the screen" would see it.
    pub fn render_all(&self) -> Vec<String> {
        self.render_lines(0, self.last_content_row() + 1)
    }

    /// Logical lines currently retained, which is what a page reports as `total_lines`.
    pub fn total_lines(&self) -> usize {
        self.render_all().len()
    }

    /// Last absolute row carrying content: the cursor row, or a later non-blank row.
    ///
    /// Rows *below* the cursor are trimmed while blank, which is what makes a cleared screen render
    /// as one empty line instead of as `rows` empty lines. The cursor row is always included — that
    /// is upstream's `lastContentRow` too — so a screen that has printed nothing renders as one
    /// empty line rather than as nothing at all.
    pub fn last_content_row(&self) -> usize {
        let cursor = self.cursor_absolute_row();
        let mut row = self.total_rows() - 1;
        while row > cursor && self.row(row).is_some_and(Row::is_blank) {
            row -= 1;
        }
        row
    }

    /// Absolute row where the cursor's logical line begins, following wrapped rows upwards.
    pub fn cursor_line_start(&self) -> usize {
        let mut row = self.cursor_absolute_row();
        while row > 0 && self.row(row).is_some_and(|line| line.wrapped) {
            row -= 1;
        }
        row
    }

    /// The absolute row the cursor is on.
    pub fn cursor_absolute_row(&self) -> usize {
        self.dropped + self.history.len() + self.cursor.row
    }

    /// Render a newest-relative page of the screen, exactly as upstream's `readScreen` does.
    pub fn read_screen(&self, request: &ScreenRequest) -> ScreenPage {
        page_lines(&self.render_all(), request)
    }

    /// Mark the start of the cursor's logical line, so output written afterwards can be rendered
    /// from it without re-reading the whole screen.
    pub fn mark(&self) -> ScreenMark {
        ScreenMark {
            row: self.cursor_line_start(),
            alternate: self.modes.alternate,
        }
    }

    /// Take the replies the screen owes for `DSR` and `DA` requests.
    ///
    /// A program that asks "where is the cursor?" and gets no answer hangs, so the reply is formed
    /// here even though this model has no write path of its own: a caller with a PTY hands these
    /// back to it.
    pub fn take_responses(&mut self) -> Vec<String> {
        std::mem::take(&mut self.responses)
    }

    // ---- rows and cells -------------------------------------------------------------------

    /// The row at an absolute index, if it is still retained.
    fn row(&self, index: usize) -> Option<&Row> {
        let relative = index.checked_sub(self.dropped)?;
        if relative < self.history.len() {
            self.history.get(relative)
        } else {
            self.grid.get(relative - self.history.len())
        }
    }

    /// The text of one row, padded to the grid width the way a terminal reads a full row out.
    ///
    /// Padding matters mid-line: a wrapped row is full, but a row whose tail was erased must not
    /// shorten the logical line it continues.
    fn row_text(&self, row: &Row) -> String {
        let mut out = String::with_capacity(self.cols);
        let mut columns = 0;
        for cell in &row.cells {
            match cell {
                Cell::Blank => out.push(' '),
                Cell::Tail => {}
                Cell::Char(ch) => out.push(*ch),
                Cell::Cluster(ch, marks) => {
                    out.push(*ch);
                    out.push_str(marks);
                }
            }
            columns += 1;
        }
        while columns < self.cols {
            out.push(' ');
            columns += 1;
        }
        out
    }

    /// Push a row out of the grid into scrollback, honouring the scrollback bound.
    ///
    /// The alternate screen has no scrollback: a full-screen program's repaint is not history.
    fn push_history(&mut self, row: Row) {
        if self.modes.alternate || self.scrollback == 0 {
            self.dropped += 1;
            return;
        }
        self.history.push_back(row);
        while self.history.len() > self.scrollback {
            self.history.pop_front();
            self.dropped += 1;
        }
    }

    /// Make `row` long enough to address `col`, filling the gap with blanks.
    fn ensure_width(row: &mut Row, col: usize) {
        if row.cells.len() <= col {
            row.cells.resize(col + 1, Cell::Blank);
        }
    }

    /// Clear the halves of any wide character that an insert, delete or overwrite split.
    fn repair(row: &mut Row) {
        for index in 0..row.cells.len() {
            let left_wide = index > 0 && cell_width(&row.cells[index - 1]) == 2;
            let right_tail = matches!(row.cells.get(index + 1), Some(Cell::Tail));
            let wide_here = cell_width(&row.cells[index]) == 2;
            match row.cells[index] {
                Cell::Tail if !left_wide => row.cells[index] = Cell::Blank,
                Cell::Char(_) | Cell::Cluster(_, _) if wide_here && !right_tail => {
                    row.cells[index] = Cell::Blank;
                }
                _ => {}
            }
        }
    }

    // ---- parser ---------------------------------------------------------------------------

    fn feed_char(&mut self, ch: char) {
        match self.parser.state {
            State::Ground => self.ground(ch),
            State::Escape => self.escape(ch),
            State::Designate => self.parser.state = State::Ground,
            State::Csi => self.csi(ch),
            State::Ignore => match ch {
                '\u{7}' | '\u{9c}' => self.parser.state = State::Ground,
                '\u{1b}' => self.parser.state = State::Escape,
                _ => {}
            },
        }
    }

    fn ground(&mut self, ch: char) {
        match ch {
            '\u{1b}' => self.parser.state = State::Escape,
            '\u{9b}' => self.csi_start(),
            '\u{90}' | '\u{98}' | '\u{9d}' | '\u{9e}' | '\u{9f}' => {
                self.parser.state = State::Ignore;
            }
            '\r' => self.carriage_return(),
            '\n' | '\u{b}' | '\u{c}' => self.line_feed(),
            '\u{8}' => self.backspace(),
            '\t' => self.tab(1),
            '\u{7}' => {}
            ch if u32::from(ch) < 0x20 => {}
            ch if ('\u{7f}'..='\u{9f}').contains(&ch) => {}
            ch => self.print_char(ch),
        }
    }

    fn escape(&mut self, ch: char) {
        self.parser.state = State::Ground;
        match ch {
            '[' => self.csi_start(),
            ']' | 'P' | 'X' | '^' | '_' => self.parser.state = State::Ignore,
            '(' | ')' | '*' | '+' | '#' | '%' => self.parser.state = State::Designate,
            '7' => self.save_cursor(),
            '8' => self.restore_cursor(),
            'D' => self.line_feed(),
            'E' => {
                self.carriage_return();
                self.line_feed();
            }
            'M' => self.reverse_index(),
            'c' => self.reset(),
            _ => {}
        }
    }

    fn csi_start(&mut self) {
        self.parser.state = State::Csi;
        self.parser.params.clear();
        self.parser.pending = None;
        self.parser.started = false;
        self.parser.private = None;
    }

    fn csi(&mut self, ch: char) {
        let code = u32::from(ch);
        match ch {
            '0'..='9' => {
                let digit = code - u32::from('0');
                let accumulated = self.parser.pending.unwrap_or(0);
                self.parser.pending = Some(
                    accumulated
                        .saturating_mul(10)
                        .saturating_add(digit)
                        .min(MAX_PARAM),
                );
                self.parser.started = true;
            }
            ';' | ':' => {
                self.parser.params.push(self.parser.pending.take());
                self.parser.started = true;
            }
            '?' | '>' | '<' | '='
                if self.parser.params.is_empty()
                    && self.parser.pending.is_none()
                    && self.parser.private.is_none() =>
            {
                self.parser.private = Some(ch);
            }
            ' '..='/' => {}
            '\u{1b}' => self.parser.state = State::Escape,
            _ if (0x40..=0x7e).contains(&code) => {
                let pending = self.parser.pending.take();
                if pending.is_some() || self.parser.started {
                    self.parser.params.push(pending);
                }
                self.parser.state = State::Ground;
                self.dispatch(ch);
            }
            _ => {}
        }
    }

    /// The `index`-th parameter, treating an absent or zero count as one.
    fn count(&self, index: usize) -> usize {
        match self.param(index) {
            None | Some(0) => 1,
            Some(value) => usize::try_from(value).unwrap_or(usize::MAX),
        }
    }

    /// The `index`-th parameter as a selector, where zero and absence both mean the first mode.
    fn selector(&self, index: usize) -> u32 {
        self.param(index).unwrap_or(0)
    }

    fn param(&self, index: usize) -> Option<u32> {
        self.parser.params.get(index).copied().flatten()
    }

    #[allow(clippy::too_many_lines)] // The dispatch table is the terminal's grammar; splitting it would
    // scatter one sequence per function and hide which families are handled together.
    fn dispatch(&mut self, final_byte: char) {
        let private = self.parser.private;
        let first = self.count(0);
        match final_byte {
            'A' => self.cursor_up(first),
            'B' => self.cursor_down(first),
            'C' | 'a' | 'e' => self.cursor_forward(first),
            'D' => self.cursor_back(first),
            'E' => {
                self.cursor_down(first);
                self.carriage_return();
            }
            'F' => {
                self.cursor_up(first);
                self.carriage_return();
            }
            'G' | '`' => self.set_column(first),
            'H' | 'f' => {
                let row = self.count(0);
                let col = self.count(1);
                self.set_position(row, col);
            }
            'd' => self.set_row(first),
            'I' => self.tab(first),
            'J' => self.erase_display(self.selector(0)),
            'K' => self.erase_line(self.selector(0)),
            'L' => self.insert_lines(first),
            'M' => self.delete_lines(first),
            'P' => self.delete_chars(first),
            'S' => self.scroll_up(first),
            'T' => self.scroll_down(first),
            'X' => self.erase_chars(first),
            '@' => self.insert_chars(first),
            'Z' => self.back_tab(first),
            'b' => self.repeat_last(first),
            'c' if private.is_none() => self.responses.push("\u{1b}[?1;2c".to_string()),
            'n' => self.device_status(self.selector(0), private),
            'r' => self.set_margins(first, self.count(1)),
            's' => self.save_cursor(),
            'u' => self.restore_cursor(),
            'h' => self.set_mode(true),
            'l' => self.set_mode(false),
            _ => {}
        }
    }

    // ---- cursor ---------------------------------------------------------------------------

    fn carriage_return(&mut self) {
        self.cursor.col = 0;
        self.modes.pending_wrap = false;
    }

    fn backspace(&mut self) {
        self.cursor.col = self.cursor.col.saturating_sub(1);
        self.modes.pending_wrap = false;
    }

    fn tab(&mut self, count: usize) {
        for _ in 0..count {
            let next = (self.cursor.col / TAB_WIDTH + 1) * TAB_WIDTH;
            self.cursor.col = next.min(self.cols - 1);
        }
        self.modes.pending_wrap = false;
    }

    fn back_tab(&mut self, count: usize) {
        for _ in 0..count {
            let mut col = self.cursor.col.saturating_sub(1);
            while col > 0 && col % TAB_WIDTH != 0 {
                col -= 1;
            }
            self.cursor.col = col;
        }
        self.modes.pending_wrap = false;
    }

    fn line_feed(&mut self) {
        self.modes.pending_wrap = false;
        let bottom = self.margins.1;
        if self.cursor.row == bottom {
            self.scroll_up(1);
        } else if self.cursor.row < self.rows - 1 {
            self.cursor.row += 1;
        }
    }

    fn reverse_index(&mut self) {
        self.modes.pending_wrap = false;
        if self.cursor.row == self.margins.0 {
            self.scroll_down(1);
        } else {
            self.cursor.row = self.cursor.row.saturating_sub(1);
        }
    }

    fn cursor_up(&mut self, count: usize) {
        let limit = if self.cursor.row >= self.margins.0 {
            self.margins.0
        } else {
            0
        };
        self.cursor.row = self.cursor.row.saturating_sub(count).max(limit);
        self.modes.pending_wrap = false;
    }

    fn cursor_down(&mut self, count: usize) {
        let limit = if self.cursor.row <= self.margins.1 {
            self.margins.1
        } else {
            self.rows - 1
        };
        self.cursor.row = self.cursor.row.saturating_add(count).min(limit);
        self.modes.pending_wrap = false;
    }

    fn cursor_forward(&mut self, count: usize) {
        self.cursor.col = self.cursor.col.saturating_add(count).min(self.cols - 1);
        self.modes.pending_wrap = false;
    }

    fn cursor_back(&mut self, count: usize) {
        self.cursor.col = self.cursor.col.saturating_sub(count);
        self.modes.pending_wrap = false;
    }

    fn set_column(&mut self, col: usize) {
        self.cursor.col = col.saturating_sub(1).min(self.cols - 1);
        self.modes.pending_wrap = false;
    }

    fn set_row(&mut self, row: usize) {
        self.cursor.row = row.saturating_sub(1).min(self.rows - 1);
        self.modes.pending_wrap = false;
    }

    fn set_position(&mut self, row: usize, col: usize) {
        self.set_row(row);
        self.set_column(col);
    }

    fn set_margins(&mut self, top: usize, bottom: usize) {
        let top = top.saturating_sub(1).min(self.rows - 1);
        let bottom = bottom.min(self.rows).saturating_sub(1);
        if top < bottom {
            self.margins = (top, bottom);
        } else {
            self.margins = (0, self.rows - 1);
        }
        self.set_position(1, 1);
    }

    fn save_cursor(&mut self) {
        self.saved = Some(self.cursor);
    }

    fn restore_cursor(&mut self) {
        if let Some(cursor) = self.saved {
            self.cursor = Cursor {
                row: cursor.row.min(self.rows - 1),
                col: cursor.col.min(self.cols - 1),
            };
        }
        self.modes.pending_wrap = false;
    }

    // ---- printing -------------------------------------------------------------------------

    fn print_char(&mut self, ch: char) {
        let width = char_width(ch);
        if width == 0 {
            self.attach_combining(ch);
            return;
        }
        if self.modes.pending_wrap {
            self.modes.pending_wrap = false;
            if self.modes.auto_wrap {
                self.carriage_return();
                self.line_feed();
                self.grid[self.cursor.row].wrapped = true;
            }
        }
        if width == 2 && self.cursor.col + 2 > self.cols {
            // The last column cannot hold the first half of a wide character. A terminal blanks that
            // column and continues the character on the next row; splitting it would misalign every
            // column after it.
            self.write_cell(Cell::Blank);
            if !self.modes.auto_wrap || self.cols < 2 {
                self.cursor.col = self.cols - 1;
                self.modes.pending_wrap = self.modes.auto_wrap;
                return;
            }
            self.grid[self.cursor.row].wrapped = true;
            self.cursor.col = 0;
            self.line_feed();
        }
        let col = self.cursor.col;
        let row = &mut self.grid[self.cursor.row];
        Self::ensure_width(row, col);
        row.cells[col] = Cell::Char(ch);
        if width == 2 {
            Self::ensure_width(row, col + 1);
            row.cells[col + 1] = Cell::Tail;
        }
        Self::repair(row);
        // `repair` may have split the pair just written only if the write overwrote a tail; both
        // halves were set above, so the glyph survives.
        self.last_printed = Some(ch);
        self.advance(width);
    }

    /// Append a zero-width combining mark to the character before the cursor.
    fn attach_combining(&mut self, ch: char) {
        let target = if self.modes.pending_wrap {
            Some(self.cursor.col)
        } else {
            self.cursor.col.checked_sub(1)
        };
        let Some(col) = target else { return };
        let row = &mut self.grid[self.cursor.row];
        let Some(cell) = row.cells.get_mut(col) else {
            return;
        };
        match cell {
            Cell::Char(base) => {
                let base = *base;
                *cell = Cell::Cluster(base, ch.to_string());
            }
            Cell::Cluster(_, marks) => marks.push(ch),
            Cell::Blank | Cell::Tail => {}
        }
    }

    fn advance(&mut self, width: usize) {
        let next = self.cursor.col + width;
        if next >= self.cols {
            self.cursor.col = self.cols - 1;
            self.modes.pending_wrap = self.modes.auto_wrap;
        } else {
            self.cursor.col = next;
        }
    }

    /// Write one cell at the cursor without moving it, clearing any wide-character half it replaces.
    fn write_cell(&mut self, cell: Cell) {
        let col = self.cursor.col;
        let row = &mut self.grid[self.cursor.row];
        Self::ensure_width(row, col);
        row.cells[col] = cell;
        Self::repair(row);
    }

    fn repeat_last(&mut self, count: usize) {
        if let Some(ch) = self.last_printed {
            for _ in 0..count.min(self.cols) {
                self.print_char(ch);
            }
        }
    }

    // ---- erasing, inserting, deleting -----------------------------------------------------

    /// Blank the cells `from..to` of row `index`, clamped to the grid width.
    fn blank_cells(&mut self, index: usize, from: usize, to: usize) {
        let cols = self.cols;
        let Some(row) = self.grid.get_mut(index) else {
            return;
        };
        let from = from.min(cols);
        let to = to.min(cols);
        if from >= to {
            return;
        }
        Self::ensure_width(row, to - 1);
        for cell in &mut row.cells[from..to] {
            *cell = Cell::Blank;
        }
        Self::repair(row);
    }

    fn erase_display(&mut self, mode: u32) {
        match mode {
            0 => {
                let row = self.cursor.row;
                let col = self.cursor.col;
                self.blank_cells(row, col, self.cols);
                for index in row + 1..self.rows {
                    self.blank_cells(index, 0, self.cols);
                }
            }
            1 => {
                let row = self.cursor.row;
                let col = self.cursor.col;
                for index in 0..row {
                    self.blank_cells(index, 0, self.cols);
                }
                self.blank_cells(row, 0, col + 1);
            }
            2 => {
                for index in 0..self.rows {
                    self.blank_cells(index, 0, self.cols);
                }
            }
            3 => {
                // Clearing scrollback also advances the absolute origin: a mark taken in the
                // cleared history must report that its row is gone, not render the row that
                // now occupies the same index.
                let cleared = self.history.len();
                self.history.clear();
                self.dropped += cleared;
                for index in 0..self.rows {
                    self.blank_cells(index, 0, self.cols);
                }
            }
            _ => {}
        }
        self.modes.pending_wrap = false;
    }

    fn erase_line(&mut self, mode: u32) {
        let row = self.cursor.row;
        let col = self.cursor.col;
        match mode {
            0 => self.blank_cells(row, col, self.cols),
            1 => self.blank_cells(row, 0, col + 1),
            2 => self.blank_cells(row, 0, self.cols),
            _ => {}
        }
        self.modes.pending_wrap = false;
    }

    fn erase_chars(&mut self, count: usize) {
        let row = self.cursor.row;
        let col = self.cursor.col;
        self.blank_cells(row, col, col.saturating_add(count));
        self.modes.pending_wrap = false;
    }

    fn insert_chars(&mut self, count: usize) {
        let cols = self.cols;
        let col = self.cursor.col;
        let row = &mut self.grid[self.cursor.row];
        Self::ensure_width(row, cols - 1);
        let count = count.min(cols - col);
        for _ in 0..count {
            row.cells.insert(col, Cell::Blank);
        }
        row.cells.truncate(cols);
        Self::repair(row);
        self.modes.pending_wrap = false;
    }

    fn delete_chars(&mut self, count: usize) {
        let cols = self.cols;
        let col = self.cursor.col;
        let row = &mut self.grid[self.cursor.row];
        Self::ensure_width(row, cols - 1);
        let count = count.min(cols - col);
        for _ in 0..count {
            row.cells.remove(col);
        }
        row.cells.resize(cols, Cell::Blank);
        Self::repair(row);
        self.modes.pending_wrap = false;
    }

    // ---- scrolling ------------------------------------------------------------------------

    /// Scroll the margin region up by `count` rows, moving the rows that leave the top to scrollback.
    fn scroll_up(&mut self, count: usize) {
        let (top, bottom) = self.margins;
        let count = count.min(bottom - top + 1);
        for _ in 0..count {
            let row = self.grid.remove(top);
            if top == 0 {
                self.push_history(row);
            }
            self.grid.insert(bottom, Row::blank());
        }
        self.modes.pending_wrap = false;
    }

    /// Scroll the margin region down by `count` rows, dropping the rows that leave the bottom.
    fn scroll_down(&mut self, count: usize) {
        let (top, bottom) = self.margins;
        let count = count.min(bottom - top + 1);
        for _ in 0..count {
            self.grid.remove(bottom);
            self.grid.insert(top, Row::blank());
            // A row pushed out of the bottom of the grid is gone: it is below the cursor's world,
            // and keeping it would make the screen read bottom-to-top.
        }
        self.modes.pending_wrap = false;
    }

    fn insert_lines(&mut self, count: usize) {
        let row = self.cursor.row;
        let bottom = self.margins.1;
        if row < self.margins.0 || row > bottom {
            return;
        }
        let count = count.min(bottom - row + 1);
        for _ in 0..count {
            self.grid.remove(bottom);
            self.grid.insert(row, Row::blank());
        }
        self.modes.pending_wrap = false;
    }

    fn delete_lines(&mut self, count: usize) {
        let row = self.cursor.row;
        let bottom = self.margins.1;
        if row < self.margins.0 || row > bottom {
            return;
        }
        let count = count.min(bottom - row + 1);
        for _ in 0..count {
            self.grid.remove(row);
            self.grid.insert(bottom, Row::blank());
        }
        self.modes.pending_wrap = false;
    }

    // ---- modes ----------------------------------------------------------------------------

    fn set_mode(&mut self, set: bool) {
        if self.parser.private.is_none() {
            return;
        }
        let params: Vec<Option<u32>> = self.parser.params.clone();
        for param in params {
            match param {
                Some(7) => self.modes.auto_wrap = set,
                Some(25) => self.modes.cursor_visible = set,
                Some(47 | 1047 | 1049) => {
                    if set {
                        self.enter_alternate();
                    } else {
                        self.exit_alternate();
                    }
                }
                Some(1048) => {
                    if set {
                        self.save_cursor();
                    } else {
                        self.restore_cursor();
                    }
                }
                _ => {}
            }
        }
    }

    fn enter_alternate(&mut self) {
        if self.modes.alternate {
            return;
        }
        let grid = std::mem::replace(&mut self.grid, vec![Row::blank(); self.rows]);
        let history = std::mem::take(&mut self.history);
        let saved = self.saved;
        let margins = self.margins;
        let dropped = std::mem::replace(&mut self.dropped, 0);
        self.main = Some(Parked {
            grid,
            history,
            cursor: self.cursor,
            saved,
            margins,
            dropped,
        });
        self.cursor = Cursor::default();
        self.saved = None;
        self.margins = (0, self.rows - 1);
        self.modes.alternate = true;
        self.modes.pending_wrap = false;
    }

    fn exit_alternate(&mut self) {
        if !self.modes.alternate {
            return;
        }
        let parked = self.main.take();
        self.grid = vec![Row::blank(); self.rows];
        self.history = VecDeque::new();
        self.modes.alternate = false;
        if let Some(parked) = parked {
            self.grid = parked.grid;
            self.history = parked.history;
            self.cursor = parked.cursor;
            self.saved = parked.saved;
            self.margins = parked.margins;
            self.dropped = parked.dropped;
        } else {
            self.cursor = Cursor::default();
            self.margins = (0, self.rows - 1);
        }
        self.cursor.row = self.cursor.row.min(self.rows - 1);
        self.cursor.col = self.cursor.col.min(self.cols - 1);
        self.modes.pending_wrap = false;
    }

    fn reset(&mut self) {
        self.grid = vec![Row::blank(); self.rows];
        self.history.clear();
        self.dropped = 0;
        self.cursor = Cursor::default();
        self.saved = None;
        self.margins = (0, self.rows - 1);
        self.modes = Modes::default();
        self.main = None;
        self.last_printed = None;
        self.parser = Parser::default();
    }

    fn device_status(&mut self, kind: u32, private: Option<char>) {
        match kind {
            5 => self.responses.push("\u{1b}[0n".to_string()),
            6 => {
                let row = self.cursor.row + 1;
                let col = self.cursor.col + 1;
                let prefix = if private == Some('?') { "?" } else { "" };
                self.responses.push(format!("\u{1b}[{prefix}{row};{col}R"));
            }
            _ => {}
        }
    }
}

/// Page logical lines the way upstream's `pageLines` does.
///
/// `offset` counts back from the newest line, `count` bounds the window, and `max_bytes` is applied
/// from the newest line backwards so the *newest* bytes survive — a reader asking for the tail of a
/// long transcript gets the tail, not its first kilobyte. When not even one line fits, the newest
/// line is cut to its last whole characters and `truncated` says so; a caller can therefore always
/// show something rather than an empty page.
///
/// `line_begin` is the requested offset rather than the offset of the first returned line, which is
/// upstream's choice: after a byte cut the page still begins where it was asked to.
pub fn page_lines(lines: &[String], request: &ScreenRequest) -> ScreenPage {
    let end = lines.len().saturating_sub(request.offset);
    let begin = end.saturating_sub(request.count);
    let selected = &lines[begin..end];
    let mut bytes = 0;
    let mut first = selected.len();
    while first > 0 {
        let separator = usize::from(first != selected.len());
        let next = selected[first - 1].len() + separator;
        if bytes + next > request.max_bytes {
            break;
        }
        bytes += next;
        first -= 1;
    }
    let mut kept: Vec<String> = selected[first..].to_vec();
    let mut truncated = first > 0;
    if kept.is_empty() && !selected.is_empty() {
        if let Some(last) = selected.last() {
            kept.push(tail_within(last, request.max_bytes));
        }
        truncated = true;
    }
    ScreenPage {
        text: kept.join("\n"),
        total_lines: lines.len(),
        line_begin: request.offset,
        line_end: request.offset + kept.len(),
        truncated,
    }
}

/// The tail of `text` that fits in `max_bytes` whole characters.
///
/// Upstream segments by grapheme cluster; this model cuts at character boundaries and then drops
/// leading zero-width marks, so a cluster that lost its base does not reappear as a stray accent.
fn tail_within(text: &str, max_bytes: usize) -> String {
    let mut start = text.len();
    for (index, _) in text.char_indices().rev() {
        if text.len() - index > max_bytes {
            break;
        }
        start = index;
    }
    let tail = &text[start..];
    tail.trim_start_matches(|ch: char| char_width(ch) == 0)
        .to_string()
}

/// The length of the longest prefix of `bytes` that ends on a character boundary or with a complete
/// character, so an incomplete sequence can be completed by the next `feed`.
fn utf8_complete_prefix(bytes: &[u8]) -> usize {
    let mut index = bytes.len();
    let mut back = 0;
    while back < 4 && index > 0 {
        index -= 1;
        back += 1;
        let byte = bytes[index];
        if byte & 0xC0 == 0x80 {
            continue;
        }
        let needed = match byte {
            0x00..=0x7f => 1,
            0xc0..=0xdf => 2,
            0xe0..=0xef => 3,
            0xf0..=0xf7 => 4,
            _ => return bytes.len(),
        };
        return if bytes.len() - index < needed {
            index
        } else {
            bytes.len()
        };
    }
    bytes.len()
}

/// The number of columns a character occupies.
///
/// Zero for combining marks and variation selectors, two for East Asian wide and fullwidth
/// characters, one otherwise. The table is the part of Unicode's East Asian Width property that a
/// terminal's alignment depends on; a character outside it is one column, which is the safe default
/// because it keeps the cursor exactly where the writer expects for Latin text.
pub fn char_width(ch: char) -> usize {
    let code = u32::from(ch);
    if matches!(
        code,
        0x0300..=0x036f
            | 0x0483..=0x0489
            | 0x0591..=0x05bd
            | 0x0610..=0x061a
            | 0x064b..=0x065f
            | 0x1ab0..=0x1aff
            | 0x1dc0..=0x1dff
            | 0x200b..=0x200f
            | 0x20d0..=0x20ff
            | 0xfe00..=0xfe0f
            | 0xfe20..=0xfe2f
            | 0xe0100..=0xe01ef
    ) {
        return 0;
    }
    if matches!(
        code,
        0x1100..=0x115f
            | 0x2e80..=0x303e
            | 0x3041..=0x33ff
            | 0x3400..=0x4dbf
            | 0x4e00..=0x9fff
            | 0xa000..=0xa4cf
            | 0xa960..=0xa97f
            | 0xac00..=0xd7a3
            | 0xf900..=0xfaff
            | 0xfe10..=0xfe19
            | 0xfe30..=0xfe6f
            | 0xff00..=0xff60
            | 0xffe0..=0xffe6
            | 0x1b000..=0x1b2ff
            | 0x1f300..=0x1f64f
            | 0x1f900..=0x1f9ff
            | 0x20000..=0x2fffd
            | 0x30000..=0x3fffd
    ) {
        return 2;
    }
    1
}

/// The display width of a cell: a wide character occupies two columns, a tail one, a blank one.
fn cell_width(cell: &Cell) -> usize {
    match cell {
        Cell::Blank | Cell::Tail => 1,
        Cell::Char(ch) | Cell::Cluster(ch, _) => char_width(*ch),
    }
}
