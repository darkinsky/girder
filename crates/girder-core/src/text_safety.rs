//! Finding and defusing characters that can make text lie about itself.
//!
//! A tool result reaches the model verbatim: `agent_loop` appends the content the tool returned to
//! the session, and the next request carries that text into the context. Nothing between those two
//! points asks what the characters *do*. Most of them only render, which is fine, but four classes
//! of character change what a reader sees while leaving the underlying bytes intact:
//!
//! - a **control character** (`\u{0}`) is invisible or mangles the line a terminal draws, so
//!   `pass\u{0}word` and `password` read the same to a person and differ to a program;
//! - an **ANSI escape sequence** (`\u{1b}[31m`) rewrites the colour, the cursor position, or the
//!   whole line of whatever renders it, so a build log can paint "0 failures" over a real one;
//! - a **bidirectional override** (`\u{202e}`) reverses the display order of what follows, so the
//!   file name `invoice\u{202e}fdp.exe` is drawn as `invoice exe.pdf` while the file that runs is
//!   an `.exe`;
//! - a **zero-width character** (`\u{200b}`) occupies no cell at all, so it can sit inside an
//!   identifier or a host name and make two different strings look identical.
//!
//! # Why sanitizing replaces instead of deleting
//!
//! The tempting fix — drop the offending characters — is exactly wrong for the last two, and this
//! module refuses it. Deleting `\u{202e}` from `invoice\u{202e}fdp.exe` yields `invoicefdp.exe`,
//! which reads as an ordinary file name: the disguise succeeds *because* the evidence was removed.
//! Replacing it with the visible marker `[RLO]` yields `invoice[RLO]fdp.exe`, which shows a person
//! that the name was constructed. The same holds for zero-width characters, whose whole purpose is
//! to be invisible: `admin\u{200b}` and `admin` must not sanitize to the same string, or the
//! sanitizer has merely reproduced the confusion it was meant to expose.
//!
//! Control characters and escape sequences are the case where removal *is* right. They carry
//! terminal instructions, not text, and keeping a rendered form of `\u{1b}[31m` would preserve the
//! instruction in an innocuous-looking wrapper. The escape sequence goes as a whole `\x1b[...m`
//! run, not as its `\u{1b}` byte followed by five ordinary characters — a per-character filter
//! would leave `[31m` behind as though the tool had printed it.
//!
//! # Why this scans and reports as well as rewrites
//!
//! [`sanitize`] is lossy by design, and a caller that wants to tell "this tool printed an escape
//! sequence" from "this tool printed text" needs the loss to be visible. [`scan`] answers that
//! without changing the text, reporting every hazard with its byte offset so a caller can quote
//! the exact position in a log. [`is_safe`] is the cheap predicate for a caller that only gates.
//! None of them are wired into the pipeline yet; they are the primitive a wiring change will call.
//!
//! # What this does not do
//!
//! It does not normalize Unicode. Two visually similar characters from different scripts (a
//! Latin `a` and a Cyrillic `а`) are ordinary text with ordinary meanings, and rewriting them
//! would corrupt legitimate content; homoglyph detection is a comparison between two strings, not
//! a scan of one. It does not remove `\n`, `\t`, or `\r` — those are how text is formatted, and a
//! sanitizer that flattened a file to one line would be reporting a problem it created.

/// One dangerous character.
///
/// The byte offset and the character are both reported because they answer different questions:
/// the offset locates the hazard in the original text, and the character identifies *which*
/// override or zero-width space was found, which matters when a name holds several.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Hazard {
    /// Byte offset of the hazard in the scanned text.
    ///
    /// This is a byte offset rather than a character index so it can be used to slice the original
    /// `&str` directly. For an escape sequence it is the offset of the `\u{1b}` that starts it.
    pub at: usize,
    /// The character found.
    ///
    /// For [`Kind::Escape`] this is the introducing `\u{1b}`; the sequence it begins is consumed as
    /// a unit by [`sanitize`], but the hazard is reported where the reader would see it begin.
    pub character: char,
    /// What makes the character dangerous.
    pub kind: Kind,
}

/// The category of a [`Hazard`].
///
/// The variants are separate because the correct response differs: control characters and escape
/// sequences are removed, overrides and zero-width characters are replaced with a visible marker.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Kind {
    /// A control character other than `\n`, `\t`, and `\r`.
    Control,
    /// An ANSI escape sequence, beginning with `\u{1b}`.
    Escape,
    /// A bidirectional override or isolate, which can make text display in a different order.
    BidiOverride,
    /// A zero-width character, which can hide content inside otherwise identical text.
    ZeroWidth,
}

/// Every bidirectional formatting character that reorders text.
///
/// This is the set Unicode calls the explicit directional embeddings, overrides, and isolates.
/// Marks (`\u{200e}`, `\u{200f}`) are deliberately absent: U+200F is required for writing Arabic
/// and Hebrew, so treating it as an attack would flag legitimate text and teach an operator to
/// ignore the check.
const BIDI_OVERRIDES: &[char] = &[
    '\u{202a}', // LEFT-TO-RIGHT EMBEDDING
    '\u{202b}', // RIGHT-TO-LEFT EMBEDDING
    '\u{202c}', // POP DIRECTIONAL FORMATTING
    '\u{202d}', // LEFT-TO-RIGHT OVERRIDE
    '\u{202e}', // RIGHT-TO-LEFT OVERRIDE -- the one that turns "exe.pdf" into "pdf.exe"
    '\u{2066}', // LEFT-TO-RIGHT ISOLATE
    '\u{2067}', // RIGHT-TO-LEFT ISOLATE
    '\u{2068}', // FIRST STRONG ISOLATE
    '\u{2069}', // POP DIRECTIONAL ISOLATE
];

/// Every zero-width character that can sit inside a string without occupying a cell.
///
/// The byte-order mark is included: in the middle of a string it is not a mark at all, it is an
/// invisible character a writer can use to make `admin` and `admin\u{feff}` look the same.
const ZERO_WIDTH: &[char] = &[
    '\u{200b}', // ZERO WIDTH SPACE
    '\u{200c}', // ZERO WIDTH NON-JOINER
    '\u{200d}', // ZERO WIDTH JOINER
    '\u{2060}', // WORD JOINER
    '\u{feff}', // ZERO WIDTH NO-BREAK SPACE / BYTE ORDER MARK
];

/// The text substituted for a bidirectional override.
///
/// The spelling names the character rather than reusing it, because a marker the reader cannot see
/// is not a marker. `[RLO]` is the Unicode name abbreviated, which a reader who knows the attack
/// recognizes and a reader who does not can still search for.
const BIDI_MARKER: &str = "[BIDI]";

/// The text substituted for a zero-width character.
const ZERO_WIDTH_MARKER: &str = "[ZWSP]";

/// The longest ANSI control sequence [`scan`] will treat as a unit.
///
/// Real sequences are a handful of bytes. The bound exists so that a lone `\u{1b}` in binary
/// output, or a hostile run of hundreds of parameters, cannot make the scan quadratic by
/// re-walking the rest of a large buffer looking for a terminator that is not there.
const MAX_ESCAPE_LEN: usize = 32;

/// The longest control *string* (OSC, DCS, SOS, PM, APC) that will be consumed as a unit.
///
/// These carry a payload rather than parameters — an OSC 8 hyperlink is a URL — so the bound has
/// to be larger than a parameter list's. It is still a bound: a stream that opens a string and
/// never closes it must not be able to swallow an entire build log.
const MAX_CONTROL_STRING_LEN: usize = 4096;

/// Whether `c` is formatting rather than dangerous.
///
/// Newline, tab, and carriage return are how text is laid out. `\r` belongs with them rather than
/// with the control characters: it is how a Windows file ends a line, and a sanitizer that ate it
/// would turn every `\r\n` into `\n` and make a diff of the sanitized text disagree with the tool.
const fn is_layout(c: char) -> bool {
    matches!(c, '\n' | '\t' | '\r')
}

/// The visible marker substituted for `kind`, or `None` when the kind is removed outright.
const fn marker_for(kind: Kind) -> Option<&'static str> {
    match kind {
        Kind::BidiOverride => Some(BIDI_MARKER),
        Kind::ZeroWidth => Some(ZERO_WIDTH_MARKER),
        Kind::Control | Kind::Escape => None,
    }
}

/// Classifies a single character that is not part of an escape sequence.
///
/// Returns `None` for text that is simply text, including the layout characters.
fn classify(c: char) -> Option<Kind> {
    if BIDI_OVERRIDES.contains(&c) {
        return Some(Kind::BidiOverride);
    }
    if ZERO_WIDTH.contains(&c) {
        return Some(Kind::ZeroWidth);
    }
    if c == '\u{1b}' {
        return Some(Kind::Escape);
    }
    if is_layout(c) {
        return None;
    }
    if c.is_control() {
        return Some(Kind::Control);
    }
    None
}

/// The number of bytes to consume for the ANSI escape sequence starting at `rest`.
///
/// `rest` begins with `\u{1b}`. Four forms are recognized, and each has to be consumed whole: a
/// partial removal is worse than no removal, because what is left (`[31m`, `(B`, `8;;url`) reads as
/// though the tool had printed it.
///
/// - a **control string**: `\u{1b}` followed by `]`, `P`, `X`, `^`, or `_`, a payload, and a
///   terminator (`BEL`, or `ESC \`). An OSC 8 hyperlink is this form, and its payload is a URL;
/// - a **CSI sequence**: `\u{1b}[`, parameter bytes, any intermediate bytes in `0x20..=0x2f`, and
///   a final byte in `0x40..=0x7e`. [`MAX_ESCAPE_LEN`] bounds the walk;
/// - a **three-byte sequence**: `\u{1b}`, an intermediate in `0x20..=0x2f`, and a final byte, as in
///   `ESC ( B` (select ASCII) and `ESC # 8` (alignment test);
/// - a **two-byte escape**: `\u{1b}` followed by one further byte.
///
/// A bare `\u{1b}` with nothing after it consumes one byte, so a terminator never has to be
/// invented.
fn escape_len(rest: &str) -> usize {
    debug_assert!(rest.starts_with('\u{1b}'));
    let Some(introducer) = rest.chars().next() else {
        return 0;
    };
    let mut len = introducer.len_utf8();
    let mut chars = rest.chars();
    let _ = chars.next();

    let Some(next) = chars.next() else {
        return len;
    };

    match next {
        // A control string: a payload, then BEL or `ESC \`.
        ']' | 'P' | 'X' | '^' | '_' => {
            len += next.len_utf8();
            let mut previous_was_escape = false;
            for c in chars {
                len += c.len_utf8();
                if c == '\u{7}' || (previous_was_escape && c == '\\') {
                    return len;
                }
                if len >= MAX_CONTROL_STRING_LEN {
                    return MAX_CONTROL_STRING_LEN;
                }
                previous_was_escape = c == '\u{1b}';
            }
            len.min(MAX_CONTROL_STRING_LEN)
        }
        // A CSI sequence: parameters, intermediates, then exactly one final byte.
        '[' => {
            len += next.len_utf8();
            for c in chars {
                len += c.len_utf8();
                let byte = u8::try_from(u32::from(c)).unwrap_or(0);
                // The final byte ends the sequence. Anything above it is malformed, and is
                // consumed anyway: over-consuming one byte beats leaving a fragment that reads as
                // text the tool printed.
                if c.is_ascii() && (0x40..=0x7e).contains(&byte) {
                    return len;
                }
                if len >= MAX_ESCAPE_LEN {
                    return len;
                }
            }
            len
        }
        // `ESC` followed by one or more intermediates and then a final byte: the three-byte
        // control sequence, as in `ESC ( B` (select ASCII) or `ESC # 8` (alignment test).
        ' '..='/' => {
            len += next.len_utf8();
            for c in chars {
                len += c.len_utf8();
                let byte = u8::try_from(u32::from(c)).unwrap_or(0);
                if !(c.is_ascii() && (0x20..=0x2f).contains(&byte)) {
                    return len;
                }
                if len >= MAX_ESCAPE_LEN {
                    return len;
                }
            }
            len
        }
        // `ESC` followed by a single byte.
        _ => len + next.len_utf8(),
    }
}

/// Scans a text for characters that can make it display differently from its bytes.
///
/// Returns one [`Hazard`] per dangerous character or escape sequence, in the order they appear.
/// The text is not modified, so the offsets index the argument exactly.
///
/// An escape sequence is reported once, at its `\u{1b}`, rather than once per byte.
#[must_use]
pub fn scan(text: &str) -> Vec<Hazard> {
    let mut hazards = Vec::new();
    let mut offset = 0;
    while let Some(c) = text[offset..].chars().next() {
        if c == '\u{1b}' {
            hazards.push(Hazard {
                at: offset,
                character: c,
                kind: Kind::Escape,
            });
            offset += escape_len(&text[offset..]).max(c.len_utf8());
            continue;
        }
        if let Some(kind) = classify(c) {
            hazards.push(Hazard {
                at: offset,
                character: c,
                kind,
            });
        }
        offset += c.len_utf8();
    }
    hazards
}

/// Returns the text with dangerous characters defused.
///
/// Control characters and ANSI escape sequences are removed, because they are instructions to a
/// terminal rather than text. Bidirectional overrides and zero-width characters are **replaced**
/// with a visible marker, because removing them would make a disguised string look ordinary and
/// destroy the only evidence that a disguise was attempted. Newline, tab, and carriage return are
/// left alone.
#[must_use]
pub fn sanitize(text: &str) -> String {
    // Sized for the common case: a clean text is copied whole, and the growth of a text full of
    // markers is bounded by the markers' own length rather than by a reallocation per hazard.
    let mut out = String::with_capacity(text.len());
    let mut offset = 0;
    while let Some(c) = text[offset..].chars().next() {
        if c == '\u{1b}' {
            offset += escape_len(&text[offset..]).max(c.len_utf8());
            continue;
        }
        match classify(c) {
            Some(kind) => {
                if let Some(marker) = marker_for(kind) {
                    out.push_str(marker);
                }
                // Otherwise the character is dropped: a control character is not text.
            }
            None => out.push(c),
        }
        offset += c.len_utf8();
    }
    out
}

/// Whether a text contains no hazards at all.
///
/// Equivalent to `scan(text).is_empty()`, without allocating the report. A caller that gates on
/// safety and never needs the details should use this.
#[must_use]
pub fn is_safe(text: &str) -> bool {
    let mut offset = 0;
    while let Some(c) = text[offset..].chars().next() {
        if c == '\u{1b}' {
            return false;
        }
        if classify(c).is_some() {
            return false;
        }
        offset += c.len_utf8();
    }
    true
}
