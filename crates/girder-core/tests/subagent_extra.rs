//! Tests for subagent naming and catalog discovery.
//!
//! The properties that matter are the ones a model depends on when it addresses a child: a name must
//! round-trip through [`resolve_agent_path`], a reservation must not hand the same name to two
//! concurrent siblings, and a recursive listing must describe every reachable branch — including the
//! damaged ones, which are reported in place rather than dropped.
//!
//! The module is included by path because the crate root does not wire it in yet, so these tests
//! compile against it as written.
#[path = "../src/subagent/extra.rs"]
pub mod subagent_extra_gap;

use std::collections::{BTreeMap, BTreeSet};

use subagent_extra_gap::{
    Activity, CatalogEntryError, CatalogError, CatalogMode, DiagnosticReason, MAX_AGENT_NAME_LEN,
    NameError, ROOT_AGENT_NAME, SubagentCatalogEntry, SubagentListEntry, SubagentNameReservations,
    agent_address, catalog_names, is_agent_name, is_session_id, join_agent_path, list_children,
    list_descendants, named_child_count, parent_agent_path, resolve_agent_path,
};

/// A named catalog entry.
fn named(id: &str, name: &str, mode: CatalogMode) -> SubagentCatalogEntry {
    SubagentCatalogEntry::new(
        id,
        0,
        mode,
        Some(format!("label-{name}")).as_deref(),
        Some(name),
    )
    .expect("valid test entry")
}

/// An unnamed catalog entry, as a pre-v3 catalog records it.
fn unnamed(id: &str, mode: CatalogMode) -> SubagentCatalogEntry {
    SubagentCatalogEntry::new(id, 0, mode, None::<String>, None).expect("valid test entry")
}

/// A catalog map from parent id to its direct children.
fn catalogs(
    rows: &[(&str, Vec<SubagentCatalogEntry>)],
) -> BTreeMap<String, Vec<SubagentCatalogEntry>> {
    rows.iter()
        .map(|(parent, entries)| ((*parent).to_owned(), entries.clone()))
        .collect()
}

/// Lists a tree, treating every child as inactive.
fn walk(
    root: &str,
    catalogs: &BTreeMap<String, Vec<SubagentCatalogEntry>>,
) -> Result<Vec<SubagentListEntry>, CatalogError> {
    list_descendants(root, |id| list_children(catalogs, id), |_| false)
}

/// The ids of a listing, in order.
fn ids(entries: &[SubagentListEntry]) -> Vec<&str> {
    entries.iter().map(SubagentListEntry::id).collect()
}

// ---------------------------------------------------------------------------------------------
// names
// ---------------------------------------------------------------------------------------------

#[test]
fn a_root_name_is_never_a_child_name() {
    assert_eq!(ROOT_AGENT_NAME, "lead");
    assert!(!is_agent_name(ROOT_AGENT_NAME));
}

#[test]
fn names_are_lower_case_letters_digits_and_hyphens_starting_with_a_letter() {
    for valid in [
        "a",
        "agent-1",
        "lead1",
        "a-b-c",
        "z9",
        "agent-000000000000000000000",
    ] {
        assert!(is_agent_name(valid), "{valid:?} should be valid");
    }
    for invalid in [
        "",          // empty
        "1agent",    // starts with a digit
        "-agent",    // starts with a hyphen
        "Agent",     // upper case
        "AGENT",     // upper case
        "agent_1",   // underscore
        "agent.1",   // dot
        "agent 1",   // space
        "agent/1",   // path separator
        "агент",     // non-ASCII
        "agent-1\n", // control character
    ] {
        assert!(!is_agent_name(invalid), "{invalid:?} should be invalid");
    }
}

#[test]
fn a_name_is_capped_at_thirty_two_characters() {
    assert_eq!(MAX_AGENT_NAME_LEN, 32);
    let longest = format!("a{}", "b".repeat(MAX_AGENT_NAME_LEN - 1));
    assert_eq!(longest.len(), MAX_AGENT_NAME_LEN);
    assert!(is_agent_name(&longest));
    let too_long = format!("{longest}b");
    assert_eq!(too_long.len(), MAX_AGENT_NAME_LEN + 1);
    assert!(!is_agent_name(&too_long));
}

#[test]
fn the_root_path_is_the_empty_string_and_a_child_appends_its_name() {
    assert_eq!(join_agent_path(Some(""), Some("a")), Some("a".to_owned()));
    assert_eq!(
        join_agent_path(Some("a"), Some("b")),
        Some("a/b".to_owned())
    );
    assert_eq!(
        join_agent_path(Some("a/b"), Some("c")),
        Some("a/b/c".to_owned())
    );
}

#[test]
fn an_unnamed_link_makes_the_whole_path_unknown() {
    // A historical entry records no name, so neither it nor its descendants have an address.
    assert_eq!(join_agent_path(Some("a"), None), None);
    assert_eq!(join_agent_path(None, Some("b")), None);
    assert_eq!(join_agent_path(None, None), None);
}

#[test]
fn an_address_is_the_root_name_a_path_or_the_session_id() {
    assert_eq!(agent_address(Some(""), "s1"), ROOT_AGENT_NAME);
    assert_eq!(agent_address(Some("a/b"), "s1"), "a/b");
    // No complete path: the id is unambiguous, which is what an address needs.
    assert_eq!(agent_address(None, "s1"), "s1");
}

#[test]
fn a_parent_path_drops_the_last_segment() {
    assert_eq!(parent_agent_path("a/b/c"), "a/b");
    assert_eq!(parent_agent_path("a"), "");
    assert_eq!(
        parent_agent_path(""),
        "",
        "the root has no parent, and an empty path stays empty"
    );
}

// ---------------------------------------------------------------------------------------------
// catalog rows
// ---------------------------------------------------------------------------------------------

#[test]
fn a_catalog_row_carries_its_identity_label_and_name() {
    let entry =
        SubagentCatalogEntry::new("s1", 7, CatalogMode::Continuable, Some("task"), Some("a"))
            .expect("valid entry");
    assert_eq!(entry.id(), "s1");
    assert_eq!(entry.created_at(), 7);
    assert_eq!(entry.mode(), CatalogMode::Continuable);
    assert_eq!(entry.label(), Some("task"));
    assert_eq!(entry.name(), Some("a"));
}

#[test]
fn a_historical_row_may_have_no_name() {
    let entry = SubagentCatalogEntry::new("s1", 0, CatalogMode::OneShot, None::<String>, None)
        .expect("valid entry");
    assert_eq!(entry.name(), None);
    assert_eq!(entry.label(), None);
}

#[test]
fn an_invalid_name_is_refused_at_the_row() {
    let error =
        SubagentCatalogEntry::new("s1", 0, CatalogMode::OneShot, None::<String>, Some("Bad"))
            .expect_err("a name must be valid");
    assert_eq!(
        error,
        CatalogEntryError::InvalidName {
            name: "Bad".to_owned()
        }
    );
}

#[test]
fn an_unknown_mode_cannot_carry_a_name() {
    // Upstream's v1 `unknown` schema has no `name` field, so a row claiming one is inconsistent.
    let error = SubagentCatalogEntry::new("s1", 0, CatalogMode::Unknown, None::<String>, Some("a"))
        .expect_err("unknown mode has no name");
    assert_eq!(
        error,
        CatalogEntryError::NameOnUnknownMode {
            name: "a".to_owned()
        }
    );
}

#[test]
fn every_catalog_mode_has_its_upstream_wire_spelling() {
    assert_eq!(CatalogMode::OneShot.as_str(), "one-shot");
    assert_eq!(CatalogMode::Continuable.as_str(), "continuable");
    assert_eq!(CatalogMode::External.as_str(), "external");
    assert_eq!(CatalogMode::Unknown.as_str(), "unknown");
}

#[test]
fn every_diagnostic_reason_has_its_upstream_wire_spelling() {
    for (reason, spelling) in [
        (DiagnosticReason::Corrupt, "corrupt"),
        (DiagnosticReason::Unsupported, "unsupported"),
        (DiagnosticReason::Unavailable, "unavailable"),
    ] {
        assert_eq!(reason.as_str(), spelling);
    }
}

#[test]
fn only_external_children_are_leaves() {
    assert!(!CatalogMode::External.is_local());
    assert!(CatalogMode::OneShot.is_local());
    assert!(CatalogMode::Continuable.is_local());
    assert!(
        CatalogMode::Unknown.is_local(),
        "the mode describes the writer, not the child's log"
    );
}

#[test]
fn a_catalog_collects_only_named_children() {
    let entries = [
        named("s1", "a", CatalogMode::OneShot),
        unnamed("s2", CatalogMode::OneShot),
        named("s3", "b", CatalogMode::Continuable),
    ];
    assert_eq!(
        catalog_names(&entries),
        BTreeSet::from(["a".to_owned(), "b".to_owned()])
    );
    assert_eq!(named_child_count(&entries), 2);
}

// ---------------------------------------------------------------------------------------------
// name reservations
// ---------------------------------------------------------------------------------------------

#[test]
fn an_unrequested_name_is_the_first_free_agent_ordinal() {
    let pool = SubagentNameReservations::new();
    let committed = BTreeSet::from(["agent-1".to_owned()]);
    let first = pool
        .reserve("parent", &committed, None)
        .expect("a name is free");
    assert_eq!(first.name(), "agent-2");
    let second = pool
        .reserve("parent", &committed, None)
        .expect("a name is free");
    assert_eq!(second.name(), "agent-3");
}

#[test]
fn an_in_flight_claim_is_taken_even_before_anything_commits() {
    // This is the race the pool exists for: both creations asked before either committed.
    let pool = SubagentNameReservations::new();
    let committed = BTreeSet::new();
    let first = pool
        .reserve("parent", &committed, Some("x"))
        .expect("first claim wins");
    assert_eq!(first.name(), "x");
    let error = pool
        .reserve("parent", &committed, Some("x"))
        .expect_err("the second claim loses");
    assert_eq!(
        error,
        NameError::Duplicate {
            name: "x".to_owned()
        }
    );
    assert_eq!(error.code(), "DUPLICATE_AGENT_NAME");
}

#[test]
fn a_committed_name_is_refused() {
    let pool = SubagentNameReservations::new();
    let committed = BTreeSet::from(["x".to_owned()]);
    assert_eq!(
        pool.reserve("parent", &committed, Some("x"))
            .expect_err("committed"),
        NameError::Duplicate {
            name: "x".to_owned()
        }
    );
}

#[test]
fn an_invalid_requested_name_is_refused_with_its_own_code() {
    let pool = SubagentNameReservations::new();
    let committed = BTreeSet::new();
    let error = pool
        .reserve("parent", &committed, Some("Lead"))
        .expect_err("invalid");
    assert_eq!(
        error,
        NameError::Invalid {
            name: "Lead".to_owned()
        }
    );
    assert_eq!(error.code(), "INVALID_AGENT_NAME");
}

#[test]
fn dropping_a_reservation_returns_the_name() {
    let pool = SubagentNameReservations::new();
    let committed = BTreeSet::new();
    {
        let reservation = pool.reserve("parent", &committed, Some("x")).expect("free");
        assert_eq!(reservation.name(), "x");
        assert_eq!(pool.pending_for("parent"), BTreeSet::from(["x".to_owned()]));
    }
    assert!(
        pool.pending_for("parent").is_empty(),
        "a dropped reservation must not leak its name"
    );
    assert!(pool.reserve("parent", &committed, Some("x")).is_ok());
}

#[test]
fn dropping_the_last_reservation_drops_the_creator_entry() {
    let pool = SubagentNameReservations::new();
    let committed = BTreeSet::new();
    let first = pool.reserve("p", &committed, Some("a")).expect("free");
    let second = pool.reserve("p", &committed, Some("b")).expect("free");
    drop(first);
    assert_eq!(pool.pending_for("p"), BTreeSet::from(["b".to_owned()]));
    drop(second);
    // An empty entry would make `pending_for` return an empty set either way, so this asserts the
    // observable consequence: the creator is free again from the first ordinal.
    assert_eq!(
        pool.reserve("p", &committed, None).expect("free").name(),
        "agent-1"
    );
}

#[test]
fn two_creators_do_not_share_a_name_space() {
    let pool = SubagentNameReservations::new();
    let committed = BTreeSet::new();
    let a = pool.reserve("one", &committed, Some("x")).expect("free");
    let b = pool.reserve("two", &committed, Some("x")).expect("free");
    assert_eq!(a.name(), "x");
    assert_eq!(b.name(), "x");
    assert_eq!(pool.pending_for("one"), BTreeSet::from(["x".to_owned()]));
    assert_eq!(pool.pending_for("two"), BTreeSet::from(["x".to_owned()]));
}

#[test]
fn a_freed_name_is_free_again() {
    let pool = SubagentNameReservations::new();
    let committed = BTreeSet::from(["a".to_owned()]);
    assert!(!pool.is_free("p", &committed, "a"));
    assert!(pool.is_free("p", &committed, "b"));
    assert!(!pool.is_free("p", &committed, "B"), "invalid is not free");
    let held = pool.reserve("p", &committed, Some("b")).expect("free");
    assert!(!pool.is_free("p", &committed, "b"));
    drop(held);
    assert!(pool.is_free("p", &committed, "b"));
}

#[test]
fn concurrent_reservations_never_hand_out_one_name_twice() {
    // The property the pool exists for. If the claim were not taken synchronously, two threads that
    // both read the committed set would both be told `agent-1` is free, and a `send` would reach the
    // wrong child. Eight threads race; every winner must hold a distinct name.
    let pool = SubagentNameReservations::new();
    let committed = BTreeSet::new();
    let winners = std::thread::scope(|scope| {
        let handles: Vec<_> = (0..8)
            .map(|_| {
                scope.spawn(|| {
                    let reservation = pool.reserve("p", &committed, None).expect("room remains");
                    let name = reservation.name().to_owned();
                    // Held rather than released: the next thread must see this claim as taken.
                    std::mem::forget(reservation);
                    name
                })
            })
            .collect();
        handles
            .into_iter()
            .map(|handle| handle.join().expect("no thread panicked"))
            .collect::<Vec<String>>()
    });
    let unique: BTreeSet<&String> = winners.iter().collect();
    assert_eq!(
        unique.len(),
        winners.len(),
        "two concurrent claims won {winners:?}"
    );
    assert_eq!(pool.pending_for("p").len(), 8);
}

// ---------------------------------------------------------------------------------------------
// traversal
// ---------------------------------------------------------------------------------------------

#[test]
fn a_listing_is_pre_order_with_siblings_in_catalog_order() {
    let tree = catalogs(&[
        (
            "root",
            vec![
                named("a", "alpha", CatalogMode::Continuable),
                named("b", "beta", CatalogMode::OneShot),
            ],
        ),
        (
            "a",
            vec![
                named("a1", "a-one", CatalogMode::OneShot),
                named("a2", "a-two", CatalogMode::OneShot),
            ],
        ),
        ("b", Vec::new()),
        ("a1", Vec::new()),
        ("a2", Vec::new()),
    ]);
    let rows = walk("root", &tree).expect("root catalog is readable");
    assert_eq!(ids(&rows), vec!["a", "a1", "a2", "b"]);
    let rows: Vec<_> = rows.iter().filter_map(SubagentListEntry::row).collect();
    assert_eq!(rows[0].depth(), 1);
    assert_eq!(rows[0].path(), Some("alpha"));
    assert_eq!(rows[0].parent_id(), "root");
    assert!(rows[0].has_children());
    assert_eq!(rows[1].depth(), 2);
    assert_eq!(rows[1].path(), Some("alpha/a-one"));
    assert_eq!(rows[1].parent_id(), "a");
    assert!(!rows[1].has_children());
    assert_eq!(rows[3].depth(), 1);
    assert_eq!(rows[3].path(), Some("beta"));
    assert_eq!(rows[0].label(), Some("label-alpha"));
    assert_eq!(rows[0].name(), Some("alpha"));
    assert_eq!(
        rows[3].label(),
        Some("label-beta"),
        "the creation label survives into the row"
    );
    assert_eq!(rows[3].name(), Some("beta"));
}

#[test]
fn an_address_of_a_listed_child_is_its_path() {
    let tree = catalogs(&[
        ("root", vec![named("a", "alpha", CatalogMode::Continuable)]),
        ("a", vec![named("a1", "a-one", CatalogMode::OneShot)]),
        ("a1", Vec::new()),
    ]);
    let rows = walk("root", &tree).expect("readable");
    let rows: Vec<_> = rows.iter().filter_map(SubagentListEntry::row).collect();
    assert_eq!(rows[0].address(), "alpha");
    assert_eq!(rows[1].address(), "alpha/a-one");
}

#[test]
fn an_unnamed_child_is_addressed_by_its_id_and_its_subtree_has_no_path() {
    let tree = catalogs(&[
        ("root", vec![unnamed("a", CatalogMode::OneShot)]),
        ("a", vec![named("a1", "a-one", CatalogMode::OneShot)]),
        ("a1", Vec::new()),
    ]);
    let rows = walk("root", &tree).expect("readable");
    let children: Vec<_> = rows.iter().filter_map(SubagentListEntry::row).collect();
    assert_eq!(children[0].path(), None);
    assert_eq!(children[0].address(), "a");
    assert_eq!(
        children[1].path(),
        None,
        "a named child of an unnamed parent still has no complete path"
    );
    assert_eq!(children[1].address(), "a1");
}

#[test]
fn an_external_child_is_a_leaf_and_is_never_queried() {
    let tree = catalogs(&[(
        "root",
        vec![named("ext", "outsider", CatalogMode::External)],
    )]);
    let mut queried = Vec::new();
    let rows = list_descendants(
        "root",
        |id| {
            queried.push(id.to_owned());
            list_children(&tree, id)
        },
        |_| false,
    )
    .expect("readable");
    assert_eq!(ids(&rows), vec!["ext"]);
    assert_eq!(
        queried,
        vec!["root".to_owned()],
        "an external child owns no local Session, so its catalog is never read"
    );
    let row = rows[0].row().expect("a child row");
    assert!(!row.has_children());
    assert_eq!(row.mode(), CatalogMode::External);
}

#[test]
fn an_unknown_mode_is_unsupported_but_its_children_are_still_listed() {
    let tree = catalogs(&[
        ("root", vec![unnamed("u", CatalogMode::Unknown)]),
        ("u", vec![named("u1", "gamma", CatalogMode::OneShot)]),
        ("u1", Vec::new()),
    ]);
    let rows = walk("root", &tree).expect("readable");
    assert_eq!(ids(&rows), vec!["u", "u1"]);
    let diagnostic = rows[0].diagnostic().expect("a diagnostic, not a child");
    assert_eq!(diagnostic.reason(), DiagnosticReason::Unsupported);
    assert_eq!(diagnostic.depth(), 1);
    assert_eq!(diagnostic.id(), "u");
    assert_eq!(diagnostic.parent_id(), "root");
    assert_eq!(
        rows[1]
            .row()
            .expect("the grandchild is still described")
            .depth(),
        2
    );
}

#[test]
fn a_corrupt_child_catalog_is_reported_in_place_and_its_siblings_survive() {
    let catalogs = catalogs(&[
        (
            "root",
            vec![
                named("bad", "broken", CatalogMode::Continuable),
                named("good", "fine", CatalogMode::OneShot),
            ],
        ),
        ("good", Vec::new()),
    ]);
    let rows = list_descendants(
        "root",
        |id| match id {
            "bad" => Err(CatalogError::Corrupt { id: id.to_owned() }),
            other => list_children(&catalogs, other),
        },
        |_| false,
    )
    .expect("only a deeper failure is a row");
    assert_eq!(ids(&rows), vec!["bad", "good"]);
    let diagnostic = rows[0].diagnostic().expect("a diagnostic");
    assert_eq!(diagnostic.reason(), DiagnosticReason::Corrupt);
    assert_eq!(
        diagnostic.path(),
        Some("broken"),
        "the diagnostic keeps the position the child would have had"
    );
    assert!(rows[1].row().is_some(), "a sibling is still described");
}

#[test]
fn an_unavailable_child_catalog_is_reported_as_unavailable() {
    let tree = catalogs(&[(
        "root",
        vec![named("gone", "missing", CatalogMode::Continuable)],
    )]);
    let rows = walk("root", &tree).expect("only the child failed");
    assert_eq!(
        rows[0].diagnostic().expect("a diagnostic").reason(),
        DiagnosticReason::Unavailable
    );
}

#[test]
fn an_unreadable_root_is_an_error_because_there_is_no_row_to_hold_it() {
    let error = list_descendants(
        "root",
        |id| Err(CatalogError::Unavailable { id: id.to_owned() }),
        |_| false,
    )
    .expect_err("the root failure is returned");
    assert_eq!(
        error,
        CatalogError::Unavailable {
            id: "root".to_owned()
        }
    );
}

#[test]
fn a_catalog_that_names_an_ancestor_terminates() {
    // A malformed catalog whose child points back at the root must not walk forever.
    let tree = catalogs(&[
        ("root", vec![named("a", "alpha", CatalogMode::Continuable)]),
        ("a", vec![named("root", "loop", CatalogMode::Continuable)]),
    ]);
    let rows = walk("root", &tree).expect("readable");
    assert_eq!(ids(&rows), vec!["a"], "the root is visited once");
}

#[test]
fn a_child_listed_by_two_parents_is_reported_once() {
    let tree = catalogs(&[
        (
            "root",
            vec![
                named("a", "alpha", CatalogMode::Continuable),
                named("b", "beta", CatalogMode::Continuable),
            ],
        ),
        ("a", vec![named("shared", "shared", CatalogMode::OneShot)]),
        ("b", vec![named("shared", "shared", CatalogMode::OneShot)]),
        ("shared", Vec::new()),
    ]);
    let rows = walk("root", &tree).expect("readable");
    let mut listed = ids(&rows);
    listed.sort_unstable();
    assert_eq!(listed, vec!["a", "b", "shared"]);
}

#[test]
fn activity_reports_residency_and_nothing_durable() {
    let tree = catalogs(&[
        (
            "root",
            vec![named("live", "live", CatalogMode::Continuable)],
        ),
        ("live", vec![named("idle", "idle", CatalogMode::OneShot)]),
        ("idle", Vec::new()),
    ]);
    let rows = list_descendants("root", |id| list_children(&tree, id), |id| id == "live")
        .expect("readable");
    let rows: Vec<_> = rows.iter().filter_map(SubagentListEntry::row).collect();
    assert_eq!(rows[0].activity(), Activity::Running);
    assert_eq!(rows[1].activity(), Activity::Inactive);
    assert_eq!(Activity::Running.as_str(), "running");
    assert_eq!(Activity::Inactive.as_str(), "inactive");
}

#[test]
fn an_empty_root_catalog_lists_nothing() {
    let tree = catalogs(&[("root", Vec::new())]);
    assert!(walk("root", &tree).expect("readable").is_empty());
}

// ---------------------------------------------------------------------------------------------
// resolution
// ---------------------------------------------------------------------------------------------

#[test]
fn a_path_resolves_to_its_session() {
    let tree = catalogs(&[
        ("root", vec![named("a", "alpha", CatalogMode::Continuable)]),
        ("a", vec![named("a1", "a-one", CatalogMode::Continuable)]),
        ("a1", vec![named("a2", "a-two", CatalogMode::OneShot)]),
        ("a2", Vec::new()),
    ]);
    let resolve = |name: &str| {
        resolve_agent_path("root", name, |id| list_children(&tree, id)).expect("catalogs readable")
    };
    assert_eq!(resolve("alpha"), Some("a".to_owned()));
    assert_eq!(resolve("alpha/a-one"), Some("a1".to_owned()));
    assert_eq!(resolve("alpha/a-one/a-two"), Some("a2".to_owned()));
}

#[test]
fn a_path_that_is_not_in_this_tree_resolves_to_nothing() {
    let tree = catalogs(&[
        ("root", vec![named("a", "alpha", CatalogMode::Continuable)]),
        ("a", Vec::new()),
    ]);
    let resolve = |name: &str| {
        resolve_agent_path("root", name, |id| list_children(&tree, id)).expect("readable")
    };
    assert_eq!(resolve("beta"), None);
    assert_eq!(resolve("alpha/beta"), None);
    // An invalid segment is not a name, so nothing is even read.
    assert_eq!(resolve("Alpha"), None);
    assert_eq!(resolve("lead"), None);
    assert_eq!(resolve("alpha//a-one"), None);
}

#[test]
fn an_external_child_cannot_be_an_intermediate_segment() {
    let tree = catalogs(&[
        (
            "root",
            vec![named("ext", "outsider", CatalogMode::External)],
        ),
        ("ext", vec![named("inner", "inner", CatalogMode::OneShot)]),
    ]);
    let resolve = |name: &str| {
        resolve_agent_path("root", name, |id| list_children(&tree, id)).expect("readable")
    };
    // It can be addressed as a leaf...
    assert_eq!(resolve("outsider"), Some("ext".to_owned()));
    // ...but nothing below it resolves, because it owns no local Session.
    assert_eq!(resolve("outsider/inner"), None);
}

#[test]
fn a_resolution_read_failure_is_an_error_not_a_missing_child() {
    let error = resolve_agent_path("root", "alpha", |_| {
        Err(CatalogError::Corrupt {
            id: "root".to_owned(),
        })
    })
    .expect_err("a read failure is not a missing child");
    assert_eq!(
        error,
        CatalogError::Corrupt {
            id: "root".to_owned()
        }
    );
}

#[test]
fn a_named_child_round_trips_through_its_address() {
    // The property a model relies on: the address it was given resolves back to the same session.
    let tree = catalogs(&[
        ("root", vec![named("a", "alpha", CatalogMode::Continuable)]),
        ("a", vec![named("a1", "a-one", CatalogMode::OneShot)]),
        ("a1", Vec::new()),
    ]);
    let rows = walk("root", &tree).expect("readable");
    for row in rows.iter().filter_map(SubagentListEntry::row) {
        let address = row.address();
        let resolved =
            resolve_agent_path("root", &address, |id| list_children(&tree, id)).expect("readable");
        assert_eq!(
            resolved.as_deref(),
            Some(row.id()),
            "address {address:?} must resolve to its own session"
        );
    }
}

// ---------------------------------------------------------------------------------------------
// catalog access and ids
// ---------------------------------------------------------------------------------------------

#[test]
fn a_present_catalog_is_returned_and_an_absent_one_is_unavailable() {
    let tree = catalogs(&[("root", vec![named("a", "alpha", CatalogMode::OneShot)])]);
    assert_eq!(
        list_children(&tree, "root").expect("present"),
        vec![named("a", "alpha", CatalogMode::OneShot)]
    );
    assert_eq!(
        list_children(&tree, "absent").expect_err("absent"),
        CatalogError::Unavailable {
            id: "absent".to_owned()
        }
    );
}

#[test]
fn session_ids_are_validated_where_they_are_built() {
    assert!(is_session_id("abc"));
    assert!(is_session_id("a/b"));
    assert!(!is_session_id(""));
    assert!(!is_session_id("has space"));
}


// Path aliases so a file compiled with #[path] still resolves crate::<domain>::<module>.
mod subagent {
    pub use super::subagent_extra_gap as extra;
}
