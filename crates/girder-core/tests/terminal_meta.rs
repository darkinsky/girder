//! Tests for terminal metadata (`terminal_meta`).
//!
//! The properties pinned here are the ones the RPC surface will depend on: that a terminal can be
//! found by the id every other layer uses, that a name is validated before it is stored, and that
//! an automated cleanup can ask which terminals are past their retention window without ever being
//! handed one that was asked to live forever.
//!
//! The name rules are tested from both sides of the boundary and with a multi-byte label, because
//! the limit is a character count: a store that measured bytes would reject a 30-character Chinese
//! label that a person would call short, and the bug would only appear in one language.
//!
//! The module is included by path rather than through the crate's re-exports, so these tests compile
//! against it as written even before the crate root wires it in.

#[path = "../src/terminal_meta.rs"]
mod terminal_meta;

use terminal_meta::{MAX_LABEL_CHARS, Meta, MetaError, Retention, Store};

/// Metadata for an id created at a fixed time.
fn meta(id: &str) -> Meta {
    Meta::new(id, 1_000)
}

/// A store holding one terminal, closed `closed_ms`, with `retention`.
fn closed_store(retention: Retention, closed_ms: u64) -> Store {
    let mut store = Store::new();
    let mut entry = meta("t1");
    entry.retention = retention;
    store.insert(entry);
    store
        .close("t1", closed_ms)
        .expect("the terminal was inserted");
    store
}

#[test]
fn a_new_meta_has_no_label_and_discards() {
    let entry = meta("t1");
    assert_eq!(entry.id, "t1");
    assert_eq!(entry.created_ms, 1_000);
    assert_eq!(entry.label, None, "a terminal starts unnamed");
    assert_eq!(
        entry.retention,
        Retention::Discard,
        "the only default that costs nothing"
    );
}

#[test]
fn an_inserted_terminal_can_be_read_back() {
    let mut store = Store::new();
    store.insert(meta("t1"));
    store.insert(meta("t2"));
    assert_eq!(store.len(), 2);
    assert!(!store.is_empty());

    let found = store.get("t1").expect("t1 was inserted");
    assert_eq!(found.id, "t1");
    assert_eq!(found.created_ms, 1_000);
}

#[test]
fn an_absent_id_is_not_found() {
    let store = Store::new();
    assert!(store.is_empty());
    assert!(store.get("nope").is_none());
}

#[test]
fn rename_stores_the_name() {
    let mut store = Store::new();
    store.insert(meta("t1"));
    store.rename("t1", "build").expect("the terminal exists");
    assert_eq!(
        store.get("t1").and_then(|entry| entry.label.as_deref()),
        Some("build")
    );
}

#[test]
fn rename_replaces_the_previous_name() {
    let mut store = Store::new();
    store.insert(meta("t1"));
    store.rename("t1", "first").expect("the terminal exists");
    store.rename("t1", "second").expect("the terminal exists");
    assert_eq!(
        store.get("t1").and_then(|entry| entry.label.as_deref()),
        Some("second"),
        "the latest name wins; renaming is not append-only"
    );
}

#[test]
fn rename_trims_surrounding_whitespace() {
    let mut store = Store::new();
    store.insert(meta("t1"));
    store
        .rename("t1", "  build  ")
        .expect("the terminal exists");
    assert_eq!(
        store.get("t1").and_then(|entry| entry.label.as_deref()),
        Some("build"),
        "a trailing space from a text box is not information"
    );
}

#[test]
fn rename_rejects_an_empty_name() {
    let mut store = Store::new();
    store.insert(meta("t1"));
    assert_eq!(store.rename("t1", ""), Err(MetaError::EmptyLabel));
    assert_eq!(
        store.get("t1").and_then(|entry| entry.label.clone()),
        None,
        "a rejected rename must not touch the stored metadata"
    );
}

#[test]
fn rename_rejects_a_whitespace_only_name() {
    let mut store = Store::new();
    store.insert(meta("t1"));
    assert_eq!(store.rename("t1", "   "), Err(MetaError::EmptyLabel));
    assert_eq!(store.rename("t1", "\t\n"), Err(MetaError::EmptyLabel));
}

#[test]
fn rename_accepts_exactly_the_maximum_length() {
    let mut store = Store::new();
    store.insert(meta("t1"));
    let label = "a".repeat(MAX_LABEL_CHARS);
    store
        .rename("t1", &label)
        .expect("the limit itself is allowed");
    assert_eq!(
        store
            .get("t1")
            .and_then(|entry| entry.label.as_deref())
            .map(str::len),
        Some(MAX_LABEL_CHARS)
    );
}

#[test]
fn rename_rejects_one_character_over_the_limit() {
    let mut store = Store::new();
    store.insert(meta("t1"));
    let label = "a".repeat(MAX_LABEL_CHARS + 1);
    assert_eq!(
        store.rename("t1", &label),
        Err(MetaError::LabelTooLong {
            length: MAX_LABEL_CHARS + 1,
            max: MAX_LABEL_CHARS,
        })
    );
    assert_eq!(
        store.get("t1").and_then(|entry| entry.label.clone()),
        None,
        "a rejected rename must not touch the stored metadata"
    );
}

#[test]
fn a_long_rename_measures_the_trimmed_length() {
    let mut store = Store::new();
    store.insert(meta("t1"));
    let label = format!("   {}   ", "a".repeat(MAX_LABEL_CHARS + 1));
    assert_eq!(
        store.rename("t1", &label),
        Err(MetaError::LabelTooLong {
            length: MAX_LABEL_CHARS + 1,
            max: MAX_LABEL_CHARS,
        }),
        "the reported length is the one that would have been stored"
    );
}

#[test]
fn a_chinese_name_is_counted_in_characters() {
    let mut store = Store::new();
    store.insert(meta("t1"));

    // Three bytes per character: a byte-based limit would reject this at 64 bytes, i.e. 21
    // characters in, and a Chinese user would have a third of the budget of a Latin one.
    let label = "构".repeat(MAX_LABEL_CHARS);
    assert!(label.len() > MAX_LABEL_CHARS, "the label is multi-byte");
    store
        .rename("t1", &label)
        .expect("64 characters is under the limit whatever the byte count");

    assert_eq!(
        store.rename("t1", &"构".repeat(MAX_LABEL_CHARS + 1)),
        Err(MetaError::LabelTooLong {
            length: MAX_LABEL_CHARS + 1,
            max: MAX_LABEL_CHARS,
        })
    );
    assert_eq!(
        store
            .get("t1")
            .and_then(|entry| entry.label.as_deref())
            .map(|stored| stored.chars().count()),
        Some(MAX_LABEL_CHARS),
        "the trim must not cut a multi-byte character in half"
    );
}

#[test]
fn rename_of_an_unknown_terminal_is_an_error() {
    let mut store = Store::new();
    assert_eq!(
        store.rename("ghost", "name"),
        Err(MetaError::Unknown {
            id: "ghost".to_owned()
        })
    );
}

#[test]
fn set_retention_changes_the_policy() {
    let mut store = Store::new();
    store.insert(meta("t1"));
    store
        .set_retention("t1", Retention::KeepFor(5_000))
        .expect("the terminal exists");
    assert_eq!(
        store.get("t1").map(|entry| entry.retention),
        Some(Retention::KeepFor(5_000))
    );
}

#[test]
fn set_retention_of_an_unknown_terminal_is_an_error() {
    let mut store = Store::new();
    assert_eq!(
        store.set_retention("ghost", Retention::Forever),
        Err(MetaError::Unknown {
            id: "ghost".to_owned()
        })
    );
}

#[test]
fn closing_an_unknown_terminal_is_an_error() {
    let mut store = Store::new();
    assert_eq!(
        store.close("ghost", 2_000),
        Err(MetaError::Unknown {
            id: "ghost".to_owned()
        })
    );
}

#[test]
fn an_open_terminal_never_expires() {
    let mut store = Store::new();
    let mut entry = meta("t1");
    entry.retention = Retention::Discard;
    store.insert(entry);
    assert!(
        store.expiring(u64::MAX).is_empty(),
        "a running terminal's output is not garbage"
    );
}

#[test]
fn discard_expires_as_soon_as_the_terminal_closes() {
    let store = closed_store(Retention::Discard, 10_000);
    assert!(
        store.expiring(9_999).is_empty(),
        "nothing expires before the close"
    );
    assert_eq!(store.expiring(10_000).len(), 1);
    assert_eq!(store.expiring(20_000).len(), 1);
}

#[test]
fn keep_for_expires_at_the_end_of_the_window() {
    let store = closed_store(Retention::KeepFor(1_000), 10_000);
    assert!(
        store.expiring(10_999).is_empty(),
        "the window has not elapsed at closed + 999"
    );
    assert_eq!(
        store.expiring(11_000).len(),
        1,
        "the deadline itself is expired"
    );
    assert_eq!(store.expiring(11_001).len(), 1);
}

#[test]
fn keep_for_zero_is_the_same_as_discard() {
    let store = closed_store(Retention::KeepFor(0), 10_000);
    assert!(store.expiring(9_999).is_empty());
    assert_eq!(store.expiring(10_000).len(), 1);
}

#[test]
fn keep_for_saturates_instead_of_wrapping() {
    // `closed + window` overflows here. Wrapping would put the deadline *before* the close, so the
    // terminal would read as long expired one instant after being given an enormous window.
    let closed_ms = u64::MAX - 5;
    let store = closed_store(Retention::KeepFor(u64::MAX), closed_ms);

    assert!(
        store.expiring(closed_ms).is_empty(),
        "the deadline must not wrap into the past"
    );
    assert!(
        store.expiring(u64::MAX - 1).is_empty(),
        "a saturated deadline is the latest representable instant, not an earlier one"
    );
    assert_eq!(
        store.expiring(u64::MAX).len(),
        1,
        "it does expire once the saturated deadline is actually reached"
    );
}

#[test]
fn forever_never_expires() {
    let store = closed_store(Retention::Forever, 10_000);
    assert!(store.expiring(0).is_empty());
    assert!(store.expiring(10_000).is_empty());
    assert!(
        store.expiring(u64::MAX).is_empty(),
        "forever means forever, not until the next cleanup pass"
    );
}

#[test]
fn forever_is_absent_even_beside_expiring_terminals() {
    let mut store = Store::new();
    let mut kept = meta("a-kept");
    kept.retention = Retention::Forever;
    let mut discarded = meta("b-discarded");
    discarded.retention = Retention::Discard;
    store.insert(kept);
    store.insert(discarded);
    store.close("a-kept", 10_000).expect("a-kept exists");
    store
        .close("b-discarded", 10_000)
        .expect("b-discarded exists");

    let expiring = store.expiring(10_000);
    let ids: Vec<&str> = expiring.iter().map(|entry| entry.id.as_str()).collect();
    assert_eq!(ids, vec!["b-discarded"]);
}

#[test]
fn several_terminals_expire_on_their_own_policies() {
    let mut store = Store::new();
    for (id, retention) in [
        ("a-discard", Retention::Discard),
        ("b-window", Retention::KeepFor(500)),
        ("c-forever", Retention::Forever),
        ("d-open", Retention::Discard),
    ] {
        let mut entry = meta(id);
        entry.retention = retention;
        store.insert(entry);
    }
    store.close("a-discard", 1_000).expect("a-discard exists");
    store.close("b-window", 1_000).expect("b-window exists");
    store.close("c-forever", 1_000).expect("c-forever exists");
    // `d-open` is deliberately never closed.

    assert!(store.expiring(999).is_empty());

    let ids = |store: &Store, now: u64| -> Vec<String> {
        store
            .expiring(now)
            .iter()
            .map(|entry| entry.id.clone())
            .collect()
    };
    assert_eq!(ids(&store, 1_000), vec!["a-discard".to_owned()]);
    assert_eq!(
        ids(&store, 1_500),
        vec!["a-discard".to_owned(), "b-window".to_owned()],
        "ids are reported in sorted order so a cleanup log is stable"
    );
    assert_eq!(
        ids(&store, 500_000),
        vec!["a-discard".to_owned(), "b-window".to_owned()],
        "the open terminal and the forever one are still absent"
    );
}

#[test]
fn closing_twice_does_not_extend_the_window() {
    let mut store = Store::new();
    store.insert(meta("t1"));
    store.close("t1", 10_000).expect("t1 exists");
    store
        .close("t1", 20_000)
        .expect("a repeated close succeeds");

    assert_eq!(
        store.expiring(10_000).len(),
        1,
        "the first close is the deadline; a retried close must not keep it alive"
    );
}

#[test]
fn retention_is_measured_from_the_close_not_from_creation() {
    let mut store = Store::new();
    let mut entry = meta("t1");
    entry.created_ms = 1_000;
    entry.retention = Retention::KeepFor(1_000);
    store.insert(entry);
    store.close("t1", 50_000).expect("t1 exists");

    assert_eq!(
        store.expiring(50_500).len(),
        0,
        "the window starts when the work finished, not when it started"
    );
    assert_eq!(store.expiring(51_000).len(), 1);
}

#[test]
fn reinserting_metadata_does_not_reopen_a_closed_terminal() {
    let mut store = Store::new();
    store.insert(meta("t1"));
    store.close("t1", 10_000).expect("t1 exists");
    store.insert(meta("t1"));
    assert_eq!(
        store.expiring(10_000).len(),
        1,
        "a refreshed label is not a resurrection"
    );
}

#[test]
fn a_rejected_rename_can_be_followed_by_a_good_one() {
    let mut store = Store::new();
    store.insert(meta("t1"));
    assert_eq!(store.rename("t1", ""), Err(MetaError::EmptyLabel));
    store.rename("t1", "ok").expect("a valid name still works");
    assert_eq!(
        store.get("t1").and_then(|entry| entry.label.as_deref()),
        Some("ok")
    );
}

#[test]
fn each_meta_is_returned_whole() {
    let mut store = Store::new();
    let mut entry = meta("t1");
    entry.retention = Retention::Forever;
    store.insert(entry);
    store.rename("t1", "deploy").expect("t1 exists");

    let found = store.get("t1").expect("t1 was inserted");
    assert_eq!(
        found,
        &Meta {
            id: "t1".to_owned(),
            label: Some("deploy".to_owned()),
            created_ms: 1_000,
            retention: Retention::Forever,
        }
    );
}
