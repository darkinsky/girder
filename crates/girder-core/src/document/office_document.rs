//! Identification of office documents, and an explanation of why their bytes are not their text.
//!
//! The `read` tool returns a file's lines, so a `.docx` handed to it comes back as deflated
//! archive bytes: the model sees noise, and nothing in the result tells it whether the file is
//! corrupt, compressed, or simply not text. Two things are missing, and this module is the first
//! one: name the format, and say plainly what a reader would have to do to get the words. The
//! second one — extraction — is deliberately absent, for the reason below.
//!
//! # Why this build extracts nothing
//!
//! DOCX, XLSX, PPTX, ODT, ODS, and ODP are zip archives of XML parts, and PDF is a sequence of
//! compressed object streams. Text needs a zip reader, an XML parser, and (for PDF) font encoding
//! tables. A parser written here without those would work on whichever sample prompted it and
//! return garbage — or panic — on the next document. That is worse than an honest refusal that
//! names the format, because the caller can turn the format into advice ("unzip it and read
//! `word/document.xml`") that stays correct for every document of that format.
//!
//! Upstream takes the third route: it shells out to `LibreOffice` through its `office-kit` service.
//! That route is not available here, so [`is_readable`] answers `false` for every format and
//! [`explain`] carries the per-format workaround.
//!
//! # Detection reads content first, the name second
//!
//! [`detect`] looks at signatures: `%PDF-` and `{\rtf` at offset zero, and the zip header for the
//! rest, whose family comes from the part names stored in the archive. A file called `notes.docx`
//! that is really a PDF is a PDF, and a `.docx` zip that carries `xl/workbook.xml` is a workbook.
//! [`from_extension`] exists for the cases content cannot answer — a head slice that stops before
//! the distinguishing part, or a file whose bytes the caller never read — and never overrides
//! [`detect`]. [`is_document`] combines the two.
//!
//! # What a head slice must contain
//!
//! The zip signatures are at offset zero, so a four-byte head identifies the container. The part
//! names come from local file headers, which are stored uncompressed early in the archive, so a
//! head of a few kilobytes normally reaches `word/document.xml` or the `OpenDocument` `mimetype`
//! entry. A shorter head is not an error: [`detect`] returns `None` rather than guess, and
//! [`from_extension`] remains available to the caller.
//!
//! # Non-goals
//!
//! Legacy binary `.doc`, `.xls`, and `.ppt` are OLE compound files (`D0 CF 11 E0`), not zip, and
//! upstream can only convert them with `LibreOffice`. They are not in [`Format`] because this build
//! would have nothing useful to say about them beyond "convert it elsewhere", and a variant with
//! no advice is a variant a consumer must special-case anyway.

/// One office or document format this build can name.
///
/// The set matches upstream's accepted inputs, minus the legacy OLE formats described in the
/// module documentation. `None` from [`detect`] or [`from_extension`] means "not one of these",
/// which includes every source file and every archive that is not an office package.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Format {
    /// `WordprocessingML` (`.docx`).
    Docx,
    /// `SpreadsheetML` (`.xlsx`).
    Xlsx,
    /// `PresentationML` (`.pptx`).
    Pptx,
    /// Portable Document Format (`.pdf`).
    Pdf,
    /// `OpenDocument` text (`.odt`).
    Odt,
    /// `OpenDocument` spreadsheet (`.ods`).
    Ods,
    /// `OpenDocument` presentation (`.odp`).
    Odp,
    /// Rich Text Format (`.rtf`).
    Rtf,
}

/// Formats whose text this build can extract.
///
/// Empty on purpose: every format above needs a reader this build does not carry, and a table says
/// that in one line where a hardcoded `false` would leave a reader wondering whether a format was
/// forgotten. Adding an extractor means adding its format here and updating [`explain`] so the two
/// answers cannot disagree.
const READABLE: &[Format] = &[];

/// How many leading bytes the text heuristic reads.
const TEXT_SAMPLE: usize = 512;

/// Percentage of control bytes above which a head is treated as binary rather than as text.
const BINARY_CONTROL_PERCENT: usize = 30;

/// Identifies a format from the content of a file.
///
/// Signatures are checked at offset zero, because a `%PDF-` that appears in the middle of a file is
/// a string in some other format, not a PDF header. For zip-based formats the family comes from
/// the part names stored in the archive and the `OpenDocument` `mimetype` entry, so the extension is
/// never consulted: content decides.
///
/// `bytes` is the head of the file, not necessarily all of it. The signatures sit at offset zero,
/// so four bytes name the container, but the part names that separate one zip family from another
/// come from local file headers and need a head of a few kilobytes to be reached. A head cut before
/// them yields `None`, which callers must treat as "unknown", not as "text".
#[must_use]
pub fn detect(bytes: &[u8]) -> Option<Format> {
    if bytes.starts_with(b"%PDF-") {
        return Some(Format::Pdf);
    }
    if bytes.starts_with(b"{\\rtf") {
        return Some(Format::Rtf);
    }
    if !is_zip(bytes) {
        return None;
    }
    zip_format(bytes)
}

/// Identifies a format from a file name, ignoring its directory and the case of the extension.
///
/// The answer is the name's claim, not the file's content: a `.docx` that is really a PDF is still
/// a `Docx` by name. [`detect`] is the authority and callers that have bytes should ask it first.
/// A name with no dot, or a dotfile such as `.docx`, has no extension and yields `None`.
#[must_use]
pub fn from_extension(path: &str) -> Option<Format> {
    let name = path.rsplit(['/', '\\']).next().unwrap_or(path);
    let (stem, extension) = name.rsplit_once('.')?;
    // `.docx` and `..docx` are names, not names with extensions: there is no stem to carry one.
    if stem.is_empty() {
        return None;
    }
    Some(match extension.to_ascii_lowercase().as_str() {
        "docx" => Format::Docx,
        "xlsx" => Format::Xlsx,
        "pptx" => Format::Pptx,
        "pdf" => Format::Pdf,
        "odt" => Format::Odt,
        "ods" => Format::Ods,
        "odp" => Format::Odp,
        "rtf" => Format::Rtf,
        _ => return None,
    })
}

/// Whether this build can extract the format's text.
///
/// Always `false` today, and `true` for nothing: see the module documentation. The call exists so
/// that a consumer asks the question instead of hardcoding its own answer, and so that enabling an
/// extractor later changes one table rather than every caller.
#[must_use]
pub fn is_readable(format: Format) -> bool {
    READABLE.contains(&format)
}

/// A model-facing paragraph saying why the format's bytes are not its text, and what to do instead.
///
/// The text is stable for a format (it names no path) and always non-empty, so it can be embedded
/// in a tool result directly. [`is_readable`] and this function answer the same question from two
/// sides and must be changed together.
#[must_use]
pub fn explain(format: Format) -> String {
    format!(
        "{} is a document, not text: reading its bytes returns markup or binary noise instead of the \
         words, and this build does not extract document text. {}",
        label(format),
        extraction_recipe(format),
    )
}

/// Whether a file is a document rather than source code.
///
/// Content wins, in both directions. A `report.docx` that is really a PDF is a document because it
/// *is* a PDF, and a `.docx` saved as plain text is not a document because its bytes are text. The
/// name is consulted only when the bytes carry no signature at all — an empty head, or a head cut
/// before the signature — and then only an untextlike head makes the name believed.
///
/// A binary archive that detection cannot place keeps its name's claim. A `.docx` that is really a
/// jar is not an Office package, but it is still a binary file whose bytes no line-oriented reader
/// can turn into words, so reporting it as a document is what lets the caller refuse it instead of
/// handing the model a screenful of noise. The caller distinguishes the two cases with [`detect`]:
/// a named format earns [`explain`], an unnamed one earns a plain "not text" refusal.
///
/// `head` should be the first few kilobytes of the file. An empty slice answers `false` for every
/// document, because a caller that supplied no bytes has not shown a document.
#[must_use]
pub fn is_document(path: &str, head: &[u8]) -> bool {
    if detect(head).is_some() {
        return true;
    }
    from_extension(path).is_some() && is_probably_binary(head)
}

/// Whether the bytes open a zip archive.
///
/// The local-file-header signature is what a non-empty archive starts with. The empty-archive and
/// spanned markers are accepted too, so that a renamed empty zip is reported as an unrecognized
/// archive instead of falling through to the name-only path.
fn is_zip(bytes: &[u8]) -> bool {
    bytes.starts_with(b"PK\x03\x04")
        || bytes.starts_with(b"PK\x05\x06")
        || bytes.starts_with(b"PK\x07\x08")
}

/// Distinguishes the zip-based document families by the parts the archive stores.
///
/// `OpenDocument` first: its `mimetype` entry is required to be the first member and stored without
/// compression, so its media type appears verbatim in the head. Office Open XML has no such entry,
/// so it is recognized by the main part of its package, and failing that by a top-level part
/// directory — the fallback covers a head slice that ends before the main part, at the cost of
/// treating a zip that merely contains a `word/` directory as a document.
fn zip_format(bytes: &[u8]) -> Option<Format> {
    if contains(bytes, b"application/vnd.oasis.opendocument.spreadsheet") {
        return Some(Format::Ods);
    }
    if contains(bytes, b"application/vnd.oasis.opendocument.presentation") {
        return Some(Format::Odp);
    }
    // The suffix guard rejects `.text-master` and `.text-web`, which share this prefix but are not
    // the word-processing format this enum names.
    if contains(bytes, b"application/vnd.oasis.opendocument.text")
        && !contains(bytes, b"application/vnd.oasis.opendocument.text-")
    {
        return Some(Format::Odt);
    }
    if contains(bytes, b"word/document.xml") {
        return Some(Format::Docx);
    }
    if contains(bytes, b"xl/workbook.xml") {
        return Some(Format::Xlsx);
    }
    if contains(bytes, b"ppt/presentation.xml") {
        return Some(Format::Pptx);
    }
    // Weaker signal, tried last so it cannot override an exact part name. A package cannot belong
    // to two families, so the first match is the answer.
    if contains(bytes, b"word/") {
        return Some(Format::Docx);
    }
    if contains(bytes, b"xl/") {
        return Some(Format::Xlsx);
    }
    if contains(bytes, b"ppt/") {
        return Some(Format::Pptx);
    }
    None
}

/// Finds a literal byte string in a slice.
///
/// A zero-length needle is rejected rather than passed to `windows`, which panics on a zero window
/// size; no caller passes one, and an empty needle could not distinguish anything anyway.
fn contains(haystack: &[u8], needle: &[u8]) -> bool {
    !needle.is_empty()
        && haystack
            .windows(needle.len())
            .any(|window| window == needle)
}

/// Whether a head looks like bytes a line-oriented reader would mangle.
///
/// A single NUL byte is decisive: no text format in the table stores one. Otherwise a high share of
/// control bytes marks binary. The test deliberately avoids `str::from_utf8`, because a head slice
/// is usually cut mid-character and a truncated multi-byte sequence would be reported as invalid
/// text for a perfectly good UTF-8 file.
fn is_probably_binary(head: &[u8]) -> bool {
    let sample = &head[..head.len().min(TEXT_SAMPLE)];
    if sample.is_empty() {
        return false;
    }
    if sample.contains(&0) {
        return true;
    }
    let control = sample.iter().filter(|&&byte| is_control(byte)).count();
    control * 100 >= sample.len() * BINARY_CONTROL_PERCENT
}

/// Whether a byte is a control character no line-oriented text file needs.
///
/// Tab, newline, carriage return, and form feed are legitimate in text and are therefore excluded.
const fn is_control(byte: u8) -> bool {
    matches!(byte, 0x00..=0x08 | 0x0B | 0x0C | 0x0E..=0x1F | 0x7F)
}

/// The model-facing name of a format, with the extension such a file normally carries.
fn label(format: Format) -> &'static str {
    match format {
        Format::Docx => "A Word document (.docx)",
        Format::Xlsx => "An Excel workbook (.xlsx)",
        Format::Pptx => "A PowerPoint presentation (.pptx)",
        Format::Pdf => "A PDF document (.pdf)",
        Format::Odt => "An OpenDocument text document (.odt)",
        Format::Ods => "An OpenDocument spreadsheet (.ods)",
        Format::Odp => "An OpenDocument presentation (.odp)",
        Format::Rtf => "An RTF document (.rtf)",
    }
}

/// The concrete way to get text out of a format without an extractor in this build.
fn extraction_recipe(format: Format) -> &'static str {
    match format {
        Format::Docx => {
            "Unzip the package and read `word/document.xml`: the visible text is the character data \
             of its `<w:t>` elements, one paragraph per `<w:p>`. Where `python-docx` is installed, \
             importing the file and joining `paragraph.text` gives the same words with less markup."
        }
        Format::Xlsx => {
            "Unzip the package and read `xl/sharedStrings.xml` for the strings and \
             `xl/worksheets/sheet1.xml` for the cells that reference them; `<f>` holds formulas, \
             not computed values. Where `openpyxl` or `pandas` is installed, either opens the \
             workbook directly."
        }
        Format::Pptx => {
            "Unzip the package and read `ppt/slides/slide1.xml` onward: each run's text is the \
             character data of an `<a:t>` element, and `ppt/_rels/presentation.xml.rels` gives the \
             slide order."
        }
        Format::Odt => {
            "Unzip the package and read `content.xml`: the text is the character data of `<text:p>` \
             and `<text:span>` elements, and `meta.xml` holds the document title."
        }
        Format::Ods => {
            "Unzip the package and read `content.xml`: cell text is the character data of \
             `<text:p>` inside `<table:table-cell>`, and a formula is a `table:formula` attribute."
        }
        Format::Odp => {
            "Unzip the package and read `content.xml`: each slide's text is the character data of \
             `<text:p>` elements inside a `<draw:frame>`, in document order."
        }
        Format::Pdf => {
            "PDF keeps text in compressed content streams, drawn with font-encoded byte codes, so \
             extracting it takes a real reader: `pdftotext` from poppler, or a PDF library. \
             Rendering a page to an image is the alternative when layout matters more than the \
             characters."
        }
        Format::Rtf => {
            "RTF is 7-bit ASCII markup rather than a container: braces group the document and \
             backslash control words such as `\\par` and `\\b` carry the layout, with the text \
             between them. Reading it as text shows that markup; `libreoffice --convert-to txt` or \
             `pandoc` removes it."
        }
    }
}
