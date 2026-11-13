//! Tests for office document detection.
//!
//! The module is exercised through the crate's public aliases, so a rename in the re-export breaks
//! this build rather than leaving the tests pointed at an internal path that no longer matches what
//! consumers see. Every sample below is assembled from signature bytes, never from a real document,
//! so each test states exactly which evidence drives its answer.

use girder_core::{
    DocumentFormat as Format, detect_document as detect, document_from_extension as from_extension,
    document_is_readable as is_readable, explain_document as explain, is_document,
};

/// Every format, so the tests that must cover all of them cannot silently miss a new variant.
const ALL: &[Format] = &[
    Format::Docx,
    Format::Xlsx,
    Format::Pptx,
    Format::Pdf,
    Format::Odt,
    Format::Ods,
    Format::Odp,
    Format::Rtf,
];

/// A zip head carrying the given part names in local file headers.
///
/// The real layout stores a 30-byte header before each name; the bytes between are not what
/// detection reads, so they are kept to the fields that make the sample structurally plausible.
fn zip_head(parts: &[&str]) -> Vec<u8> {
    let mut bytes = b"PK\x03\x04".to_vec();
    for part in parts {
        bytes.extend_from_slice(&[0x14, 0x00, 0x00, 0x00, 0x08, 0x00, 0x00, 0x00]);
        bytes.extend_from_slice(part.as_bytes());
        bytes.push(0);
    }
    bytes
}

/// A zip head whose first member is the `OpenDocument` `mimetype` entry, as the specification
/// requires: stored uncompressed so the media type appears verbatim.
fn odf_head(media_type: &str) -> Vec<u8> {
    zip_head(&["mimetype", media_type])
}

/// Bytes of a small plain-text source file.
fn source_text() -> Vec<u8> {
    b"fn main() {\n    println!(\"hello\");\n}\n".to_vec()
}

// ---- magic-number detection ------------------------------------------------------------------

#[test]
fn a_word_package_is_detected_by_its_main_part() {
    let bytes = zip_head(&["[Content_Types].xml", "_rels/.rels", "word/document.xml"]);

    assert_eq!(detect(&bytes), Some(Format::Docx));
}

#[test]
fn an_excel_package_is_detected_by_its_workbook_part() {
    let bytes = zip_head(&[
        "[Content_Types].xml",
        "xl/workbook.xml",
        "xl/worksheets/sheet1.xml",
    ]);

    assert_eq!(detect(&bytes), Some(Format::Xlsx));
}

#[test]
fn a_powerpoint_package_is_detected_by_its_presentation_part() {
    let bytes = zip_head(&[
        "[Content_Types].xml",
        "ppt/presentation.xml",
        "ppt/slides/slide1.xml",
    ]);

    assert_eq!(detect(&bytes), Some(Format::Pptx));
}

#[test]
fn the_open_document_families_are_detected_by_their_media_type() {
    // One table-driven test rather than three: the three media types differ by one word, and the
    // value of the test is that the longer `presentation` type is not read as the `text` one.
    assert_eq!(
        detect(&odf_head("application/vnd.oasis.opendocument.text")),
        Some(Format::Odt)
    );
    assert_eq!(
        detect(&odf_head("application/vnd.oasis.opendocument.spreadsheet")),
        Some(Format::Ods)
    );
    assert_eq!(
        detect(&odf_head("application/vnd.oasis.opendocument.presentation")),
        Some(Format::Odp)
    );
}

#[test]
fn an_open_document_master_is_not_reported_as_plain_text() {
    // `text-master` and `text-web` share the word-processing prefix but are not `.odt` files, and
    // naming them `Odt` would send the caller to `content.xml` for a document that is a shell.
    assert_eq!(
        detect(&odf_head("application/vnd.oasis.opendocument.text-master")),
        None
    );
}

#[test]
fn a_pdf_is_detected_by_its_header() {
    assert_eq!(detect(b"%PDF-1.7\n%\xE2\xE3\xCF\xD3\n"), Some(Format::Pdf));
}

#[test]
fn an_rtf_document_is_detected_by_its_opening_group() {
    assert_eq!(
        detect(b"{\\rtf1\\ansi\\deff0{\\fonttbl{\\f0 Times;}}\\f0 hello\\par}"),
        Some(Format::Rtf)
    );
}

#[test]
fn a_signature_is_only_read_at_offset_zero() {
    // A `%PDF-` inside another file is a string in that file, not a PDF, so it must not be detected.
    assert_eq!(detect(b"const marker = b\"%PDF-1.4\";\n"), None);
}

#[test]
fn a_zip_that_is_not_an_office_package_is_not_detected() {
    // The container alone is not enough: a jar, a plain zip, and an epub all start with the same
    // four bytes, and reporting one of them as a document would be a wrong answer, not a cautious one.
    let bytes = zip_head(&["META-INF/MANIFEST.MF", "com/example/Main.class"]);

    assert_eq!(detect(&bytes), None);
}

#[test]
fn a_head_shorter_than_the_signature_is_not_detected() {
    assert_eq!(detect(b""), None);
    assert_eq!(detect(b"PK"), None);
    assert_eq!(detect(b"%PD"), None);
}

#[test]
fn a_zip_head_that_stops_before_the_part_names_is_not_detected() {
    // Only the local-file-header signature is present. Detection declines rather than guessing, so
    // the caller can fall back to the extension instead of receiving a wrong family.
    assert_eq!(detect(b"PK\x03\x04"), None);
}

#[test]
fn a_plain_text_file_is_not_detected() {
    assert_eq!(detect(&source_text()), None);
}

// ---- extension detection ---------------------------------------------------------------------

#[test]
fn every_extension_maps_to_its_format() {
    let cases = [
        ("report.docx", Format::Docx),
        ("report.xlsx", Format::Xlsx),
        ("report.pptx", Format::Pptx),
        ("report.pdf", Format::Pdf),
        ("report.odt", Format::Odt),
        ("report.ods", Format::Ods),
        ("report.odp", Format::Odp),
        ("report.rtf", Format::Rtf),
    ];

    for (path, format) in cases {
        assert_eq!(from_extension(path), Some(format), "{path}");
    }
}

#[test]
fn the_extension_is_matched_without_its_case() {
    assert_eq!(from_extension("REPORT.DOCX"), Some(Format::Docx));
    assert_eq!(from_extension("Quarterly.XlsX"), Some(Format::Xlsx));
}

#[test]
fn a_directory_in_the_path_is_not_read_as_an_extension() {
    assert_eq!(
        from_extension("/home/user/reports.v2/summary"),
        None,
        "a dot in a directory must not supply the extension"
    );
    assert_eq!(
        from_extension("C:\\Users\\me\\budget.xlsx"),
        Some(Format::Xlsx),
        "a Windows separator ends the directory as a slash does"
    );
}

#[test]
fn a_name_without_an_extension_has_no_format() {
    assert_eq!(from_extension("Makefile"), None);
    assert_eq!(from_extension(""), None);
    assert_eq!(
        from_extension(".docx"),
        None,
        "a dotfile has no stem to extend"
    );
}

#[test]
fn an_unrelated_extension_has_no_format() {
    assert_eq!(from_extension("main.rs"), None);
    assert_eq!(from_extension("archive.zip"), None);
    // The legacy OLE formats are not in the enum; they must not be mapped to a neighbour.
    assert_eq!(from_extension("old.doc"), None);
    assert_eq!(from_extension("old.xls"), None);
    assert_eq!(from_extension("old.ppt"), None);
}

// ---- content wins over the name --------------------------------------------------------------

#[test]
fn a_pdf_named_docx_is_detected_as_a_pdf() {
    // The conflict case the module exists for: the name says Word, the bytes say PDF, and the bytes
    // decide. A reader told "docx" would unzip a non-archive and fail on the wrong thing.
    let bytes = b"%PDF-1.7\n1 0 obj\n".to_vec();

    assert_eq!(from_extension("report.docx"), Some(Format::Docx));
    assert_eq!(detect(&bytes), Some(Format::Pdf));
    assert!(is_document("report.docx", &bytes));
}

#[test]
fn a_workbook_named_docx_is_detected_as_a_workbook() {
    let bytes = zip_head(&["[Content_Types].xml", "xl/workbook.xml"]);

    assert_eq!(detect(&bytes), Some(Format::Xlsx));
    assert_eq!(
        from_extension("report.docx"),
        Some(Format::Docx),
        "the name still claims Word; detection is what overrides it"
    );
}

#[test]
fn a_presentation_named_pdf_is_detected_as_a_presentation() {
    let bytes = zip_head(&["ppt/presentation.xml"]);

    assert_eq!(detect(&bytes), Some(Format::Pptx));
}

#[test]
fn a_zip_named_docx_that_is_not_an_office_package_is_still_not_text() {
    // The name claims a document and the bytes are a binary archive. Detection declines to name a
    // format — so no advice can be given — but the bytes still cannot be read as lines, which is
    // what `is_document` reports. The caller tells the two cases apart by asking `detect` first.
    let bytes = zip_head(&["META-INF/MANIFEST.MF"]);

    assert_eq!(detect(&bytes), None, "no office package is identified");
    assert!(is_document("app.jar.docx", &bytes), "but it is not text");
    assert!(
        !is_document("app.jar", &bytes),
        "without a document name there is nothing to refuse"
    );
}

#[test]
fn a_docx_named_file_saved_as_text_is_not_a_document() {
    // The opposite conflict, and the reason `is_document` does not trust the name alone: the bytes
    // are readable text, so handing them to the read tool is correct whatever the name says.
    assert!(!is_document(
        "notes.docx",
        b"these are the notes, in plain text\n"
    ));
}

#[test]
fn a_document_is_still_recognized_when_the_head_lacks_part_names() {
    // A head slice that ends inside the first header carries the zip signature but no part name.
    // The extension then decides, which is the only remaining evidence and the right answer here.
    assert!(is_document("report.pdf", b"%PDF-1.7\n"));
    assert!(!is_document("main.rs", b"fn main() {}\n"));
}

// ---- is_document -----------------------------------------------------------------------------

#[test]
fn a_detected_document_is_a_document_whatever_its_name() {
    let docx = zip_head(&["word/document.xml"]);

    assert!(is_document("report.docx", &docx));
    assert!(is_document("report", &docx), "the bytes alone are enough");
    assert!(
        is_document("main.rs", &docx),
        "a wrong name cannot hide a document"
    );
}

#[test]
fn an_untextlike_file_named_like_a_document_is_a_document() {
    // No signature, but a NUL byte and no text: the name is believed because the bytes do not deny it.
    let binary = b"BM\x00\x00\x00\x00\x00\x00".to_vec();

    assert!(is_document("scan.docx", &binary));
}

#[test]
fn source_and_plain_text_files_are_not_documents() {
    assert!(!is_document("src/main.rs", &source_text()));
    assert!(!is_document("README.md", b"# Notes\n\nProse.\n"));
    assert!(!is_document("data.json", b"{\"key\": \"value\"}\n"));
    assert!(!is_document("Makefile", b"all:\n\tcargo build\n"));
}

#[test]
fn an_empty_head_reports_no_document() {
    // Nothing was shown, so nothing is claimed: a caller that did not read the file has not
    // established that it is a document.
    assert!(!is_document("report.docx", b""));
}

#[test]
fn a_utf8_head_cut_mid_character_is_not_called_binary() {
    // A head slice routinely ends inside a multi-byte character. Decoding it as UTF-8 would fail and
    // mark a good text file binary, so the heuristic counts control bytes instead.
    let text = "文档内容，多字节字符。".as_bytes();
    let cut = &text[..text.len() - 1];

    assert!(!is_document("notes.md", cut));
}

// ---- is_readable -----------------------------------------------------------------------------

#[test]
fn no_format_is_readable_in_this_build() {
    // Pinned deliberately: the module recognizes formats and refuses to extract them. If an
    // extractor lands, this test fails and the author must also revisit `explain`.
    for format in ALL {
        assert!(
            !is_readable(*format),
            "{format:?} must not claim readability"
        );
    }
}

// ---- explain ---------------------------------------------------------------------------------

#[test]
fn every_format_has_a_nonempty_explanation() {
    for format in ALL {
        let text = explain(*format);
        assert!(!text.is_empty(), "{format:?} must explain itself");
        assert!(
            text.contains("does not extract document text"),
            "{format:?} must say this build cannot extract text: {text}"
        );
        assert!(
            text.len() > 120,
            "{format:?} must give usable advice, not a stub: {text}"
        );
    }
}

#[test]
fn the_explanation_names_the_format_and_its_workaround() {
    let docx = explain(Format::Docx);

    assert!(docx.contains(".docx"));
    assert!(docx.contains("word/document.xml"), "{docx}");
    assert!(explain(Format::Xlsx).contains("xl/sharedStrings.xml"));
    assert!(explain(Format::Pdf).contains("pdftotext"));
    assert!(explain(Format::Rtf).contains("\\par"));
    assert!(explain(Format::Odt).contains("content.xml"));
}

#[test]
fn explanations_differ_per_format() {
    for (index, format) in ALL.iter().enumerate() {
        for other in &ALL[index + 1..] {
            assert_ne!(
                explain(*format),
                explain(*other),
                "{format:?} and {other:?} must not share advice"
            );
        }
    }
}
