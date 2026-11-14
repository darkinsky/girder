//! Tests for scanning and defusing characters that can make text lie about itself.
//!
//! Three behaviours are load-bearing and are what most of this file checks. An ANSI escape
//! sequence must be removed **whole** — a filter that only dropped `\u{1b}` would leave `[31m`
//! behind as though the tool had printed it. A bidirectional override must be **replaced**, not
//! deleted — deleting it turns a disguised name into an ordinary-looking one and destroys the
//! evidence. And an offset must point at the offending byte, because a caller that logs where the
//! hazard was is the caller that can explain it afterwards.
//!
//! The module is loaded from source through `#[path]` rather than imported from the crate's root,
//! which keeps this suite runnable against `src/text_safety.rs` alone and independent of whatever a
//! concurrent `lib.rs` change exports. Once the re-export settles, this can become
//! `use girder_core::{HazardKind as Kind, is_text_safe as is_safe, sanitize_text as sanitize, scan_text as scan};`.

#[path = "../src/text_safety.rs"]
mod text_safety;

use text_safety::{Kind, is_safe, sanitize, scan};

/// The introduce byte of every ANSI sequence, named so the tests read as prose.
const ESC: char = '\u{1b}';

// ---- detecting controls -------------------------------------------------------------------------

#[test]
fn a_nul_is_found_with_its_offset() {
    let text = "pass\u{0}word";
    let hazards = scan(text);
    assert_eq!(hazards.len(), 1, "{hazards:?}");
    assert_eq!(hazards[0].at, 4);
    assert_eq!(hazards[0].character, '\u{0}');
    assert_eq!(hazards[0].kind, Kind::Control);
}

#[test]
fn other_c0_controls_are_found() {
    for c in ['\u{1}', '\u{7}', '\u{8}', '\u{b}', '\u{c}', '\u{1f}'] {
        let text = format!("a{c}b");
        let hazards = scan(&text);
        assert_eq!(hazards.len(), 1, "U+{:04X}: {hazards:?}", c as u32);
        assert_eq!(hazards[0].kind, Kind::Control, "U+{:04X}", c as u32);
        assert!(sanitize(&text).is_char_boundary(1));
        assert_eq!(sanitize(&text), "ab", "U+{:04X}", c as u32);
    }
}

#[test]
fn a_delete_character_is_a_control() {
    let hazards = scan("abc\u{7f}");
    assert_eq!(hazards.len(), 1, "{hazards:?}");
    assert_eq!(hazards[0].at, 3);
    assert_eq!(hazards[0].kind, Kind::Control);
}

#[test]
fn controls_are_removed_rather_than_marked() {
    // A control character is a terminal instruction, not text. Keeping a rendered form of it would
    // preserve the instruction inside something that looks innocuous.
    assert_eq!(sanitize("a\u{0}b\u{1}"), "ab");
}

// ---- ANSI escape sequences are removed as a unit -----------------------------------------------

#[test]
fn an_sgr_sequence_is_removed_whole() {
    // The core case: `\x1b[31m` is five bytes, and *all five* must go. Removing only the ESC would
    // leave "[31m" in the text, which reads as though the tool had printed it.
    let text = "\u{1b}[31mred\u{1b}[0m";
    assert_eq!(sanitize(text), "red");
    assert!(!sanitize(text).contains('['), "{}", sanitize(text));
    assert!(!sanitize(text).contains("31m"), "{}", sanitize(text));
}

#[test]
fn an_escape_is_reported_once_at_its_introduce_byte() {
    let hazards = scan("\u{1b}[31mred");
    assert_eq!(
        hazards.len(),
        1,
        "one sequence, not one per byte: {hazards:?}"
    );
    assert_eq!(hazards[0].at, 0);
    assert_eq!(hazards[0].character, ESC);
    assert_eq!(hazards[0].kind, Kind::Escape);
}

#[test]
fn a_cursor_movement_sequence_is_removed_whole() {
    // `\x1b[2K` erases the line that was already printed. If it survived, a tool could paint over
    // the output the model is about to read.
    assert_eq!(sanitize("done\u{1b}[2K"), "done");
}

#[test]
fn a_compound_sgr_sequence_is_removed_whole() {
    assert_eq!(sanitize("\u{1b}[1;38;5;196mbold\u{1b}[m"), "bold");
}

#[test]
fn a_two_byte_escape_is_removed_whole() {
    // The non-CSI form: ESC followed by a single byte, as in ESC ( B or ESC 7.
    assert_eq!(sanitize("a\u{1b}(Bb"), "ab");
    assert_eq!(sanitize("a\u{1b}7b"), "ab");
}

#[test]
fn a_lone_escape_is_removed() {
    // A bare ESC consumes only itself, so the text after it is not swallowed.
    assert_eq!(sanitize("tail\u{1b}"), "tail");
    assert_eq!(scan("\u{1b}").len(), 1);
}

#[test]
fn a_three_byte_escape_is_removed_whole() {
    // `ESC ( B` selects ASCII. Removing only `ESC (` would leave a stray `B`.
    assert_eq!(sanitize("\u{1b}(Btext"), "text");
    assert_eq!(sanitize("a\u{1b}#8b"), "ab");
}

#[test]
fn a_hyperlink_escape_is_removed_whole() {
    // OSC 8 attaches a URL to the following text. If the sequence survived, a tool could make a
    // harmless-looking label point anywhere.
    let text = "\u{1b}]8;;https://example.invalid/x\u{7}click\u{1b}]8;;\u{7}";
    assert_eq!(sanitize(text), "click");
    assert!(!sanitize(text).contains("example.invalid"));
}

#[test]
fn an_unterminated_control_string_does_not_swallow_the_rest_of_the_text() {
    // A stream that opens an OSC and never closes it must not consume an entire build log.
    let text = format!("keep\u{1b}][{}", "x".repeat(20_000));
    let cleaned = sanitize(&text);
    assert!(
        cleaned.starts_with("keep"),
        "{}",
        &cleaned[..cleaned.len().min(40)]
    );
    assert!(cleaned.len() > 15_000, "consumed {} bytes", cleaned.len());
}

#[test]
fn an_unterminated_sequence_does_not_swallow_the_rest_of_the_text() {
    // A hostile or truncated sequence must not delete the whole buffer, and a run of parameters
    // must not be able to make the scan quadratic.
    let text = "keep\u{1b}[";
    assert_eq!(sanitize(text), "keep");

    let long = format!("start\u{1b}[{}", "1;".repeat(500));
    let cleaned = sanitize(&long);
    assert!(cleaned.starts_with("start"), "{cleaned}");
}

#[test]
fn several_sequences_in_one_line_are_all_removed() {
    let text = "\u{1b}[32m+\u{1b}[0m ok \u{1b}[1mbold\u{1b}[22m";
    assert_eq!(sanitize(text), "+ ok bold");
    assert_eq!(scan(text).len(), 4);
}

// ---- bidirectional overrides are replaced, not deleted -----------------------------------------

#[test]
fn a_right_to_left_override_is_replaced_with_a_visible_marker() {
    // The whole point. `\u{202e}` makes "fdp.exe" display as "exe.pdf". Deleting it would leave
    // "invoicefdp.exe", which looks like a perfectly ordinary name — the disguise would succeed by
    // being erased. The marker is what exposes it.
    let name = "invoice\u{202e}fdp.exe";
    let cleaned = sanitize(name);

    assert_ne!(cleaned, "invoicefdp.exe", "deleted instead of replaced");
    assert!(!cleaned.contains('\u{202e}'), "the override survived");
    assert!(cleaned.contains("[BIDI]"), "{cleaned}");
    assert_eq!(cleaned, "invoice[BIDI]fdp.exe");
}

#[test]
fn every_directional_override_is_found() {
    for c in [
        '\u{202a}', '\u{202b}', '\u{202c}', '\u{202d}', '\u{202e}', '\u{2066}', '\u{2067}',
        '\u{2068}', '\u{2069}',
    ] {
        let text = format!("a{c}b");
        let hazards = scan(&text);
        assert_eq!(
            hazards.len(),
            1,
            "U+{:04X} not found: {hazards:?}",
            c as u32
        );
        assert_eq!(hazards[0].kind, Kind::BidiOverride, "U+{:04X}", c as u32);
        assert!(
            sanitize(&text).contains("[BIDI]"),
            "U+{:04X} was not marked: {}",
            c as u32,
            sanitize(&text)
        );
    }
}

#[test]
fn two_names_that_differ_only_by_an_override_stay_distinct_after_sanitizing() {
    // The property that matters more than any single string: sanitizing must never collapse two
    // different inputs into one, because that is what makes a disguise indistinguishable.
    let disguised = "settings\u{202e}exe.txt";
    let honest = "settingsexe.txt";
    assert_ne!(sanitize(disguised), sanitize(honest));
    assert_ne!(sanitize("admin"), sanitize("admin\u{202e}"));
}

#[test]
fn directional_marks_used_by_real_scripts_are_not_flagged() {
    // U+200E and U+200F are ordinary in Arabic and Hebrew text. Flagging them would make the check
    // cry wolf on legitimate content, and a check that cries wolf is turned off.
    assert!(is_safe("\u{200e}hello\u{200f}"));
    assert_eq!(sanitize("مرحبا\u{200f}"), "مرحبا\u{200f}");
}

// ---- zero-width characters are replaced --------------------------------------------------------

#[test]
fn a_zero_width_space_inside_a_name_is_marked() {
    let text = "admin\u{200b}";
    let hazards = scan(text);
    assert_eq!(hazards.len(), 1, "{hazards:?}");
    assert_eq!(hazards[0].at, 5);
    assert_eq!(hazards[0].kind, Kind::ZeroWidth);

    let cleaned = sanitize(text);
    assert_ne!(cleaned, "admin", "deleted instead of replaced");
    assert_eq!(cleaned, "admin[ZWSP]");
}

#[test]
fn every_zero_width_character_is_found() {
    for c in ['\u{200b}', '\u{200c}', '\u{200d}', '\u{2060}', '\u{feff}'] {
        let hazards = scan(&format!("x{c}y"));
        assert_eq!(hazards.len(), 1, "U+{:04X}: {hazards:?}", c as u32);
        assert_eq!(hazards[0].kind, Kind::ZeroWidth, "U+{:04X}", c as u32);
    }
}

#[test]
fn two_identifiers_that_differ_only_by_a_zero_width_stay_distinct() {
    assert_ne!(sanitize("admin"), sanitize("adm\u{200d}in"));
}

// ---- layout characters are safe ----------------------------------------------------------------

#[test]
fn newline_and_tab_are_safe() {
    let text = "line one\n\tindented\r\nline two\n";
    assert!(is_safe(text));
    assert!(scan(text).is_empty());
    assert_eq!(sanitize(text), text);
}

#[test]
fn a_multiline_log_with_tabs_loses_nothing() {
    let text = "fn main() {\n\tprintln!(\"hi\");\n}\n";
    assert_eq!(sanitize(text), text);
    assert!(is_safe(text));
}

// ---- ordinary and empty text -------------------------------------------------------------------

#[test]
fn ordinary_text_is_safe_and_unchanged() {
    for text in [
        "the build finished in 3.2s",
        "if x >= 0 { y += 1; }",
        "日本語のテキスト",
        "emoji 🎉 and accents é",
        "a [b] {c} <d> $((e))", // brackets are ordinary text outside an escape
        "",
    ] {
        assert!(is_safe(text), "{text:?}");
        assert!(scan(text).is_empty(), "{text:?}");
        assert_eq!(sanitize(text), text, "{text:?}");
    }
}

#[test]
fn empty_text_is_safe() {
    assert!(is_safe(""));
    assert!(scan("").is_empty());
    assert_eq!(sanitize(""), "");
}

#[test]
fn a_bracket_alone_is_not_an_escape() {
    // `[31m` without its ESC is text a program may legitimately print, so it must survive. Only a
    // sequence that actually begins with ESC is a terminal instruction.
    assert!(is_safe("[31m"));
    assert_eq!(sanitize("[31m"), "[31m");
}

// ---- is_safe agrees with scan ------------------------------------------------------------------

#[test]
fn is_safe_agrees_with_scan_on_every_kind() {
    for text in [
        "",
        "plain",
        "with\u{0}control",
        "\u{1b}[31mred",
        "name\u{202e}exe",
        "zero\u{200b}width",
        "line\nbreak\ttab",
    ] {
        assert_eq!(
            is_safe(text),
            scan(text).is_empty(),
            "disagreement on {text:?}"
        );
    }
}

#[test]
fn is_safe_is_false_when_any_hazard_is_present() {
    assert!(!is_safe("clean\u{0}dirty"));
    assert!(!is_safe("\u{1b}"));
    assert!(!is_safe("\u{202e}"));
    assert!(!is_safe("\u{200b}"));
}

#[test]
fn sanitize_never_leaves_a_hazard_behind() {
    // Whatever the input, the output must be safe by the module's own definition, or the sanitizer
    // has not finished the job it advertises.
    for text in [
        "\u{0}\u{1}\u{1b}[31m\u{202e}\u{200b}",
        "a\u{1b}[1;2;3mb",
        "\u{2069}\u{feff}",
        "\u{1b}",
        "plain\n\ttext",
        "",
    ] {
        let cleaned = sanitize(text);
        assert!(is_safe(&cleaned), "{text:?} -> {cleaned:?}");
        assert!(scan(&cleaned).is_empty(), "{text:?} -> {cleaned:?}");
    }
}

// ---- offsets ------------------------------------------------------------------------------------

#[test]
fn offsets_are_byte_offsets_into_the_original_text() {
    // "é" is two bytes, so the hazard after it is at byte 3, not at character 2. A character index
    // would be wrong the moment the text is not ASCII, and slicing with it would panic.
    let text = "é\u{0}x";
    let hazards = scan(text);
    assert_eq!(hazards.len(), 1, "{hazards:?}");
    assert_eq!(hazards[0].at, 2);
    assert_eq!(&text[hazards[0].at..=hazards[0].at], "\u{0}");
}

#[test]
fn offsets_point_at_each_hazard_in_order() {
    // U+202E is three bytes, so the zero-width space after `c` is at byte 7, not at character 4.
    let text = "a\u{0}b\u{202e}c\u{200b}d";
    let hazards = scan(text);
    assert_eq!(hazards.len(), 3, "{hazards:?}");
    assert_eq!(hazards[0].at, 1);
    assert_eq!(hazards[1].at, 3);
    assert_eq!(hazards[2].at, 7);
    for hazard in &hazards {
        assert!(text.is_char_boundary(hazard.at));
        assert_eq!(text[hazard.at..].chars().next(), Some(hazard.character));
    }
}

#[test]
fn an_escape_offset_points_at_its_escape_byte() {
    let text = "ok \u{1b}[31mred";
    let hazards = scan(text);
    assert_eq!(hazards.len(), 1, "{hazards:?}");
    assert_eq!(hazards[0].at, 3);
    assert_eq!(text[hazards[0].at..].chars().next(), Some(ESC));
}

#[test]
fn scanning_finds_every_hazard_in_a_mixed_text() {
    let text = "\u{1b}[31mwarn\u{0}ing\u{202e}\u{200b}";
    let kinds: Vec<Kind> = scan(text).iter().map(|hazard| hazard.kind).collect();
    assert_eq!(
        kinds,
        vec![
            Kind::Escape,
            Kind::Control,
            Kind::BidiOverride,
            Kind::ZeroWidth
        ]
    );
}
