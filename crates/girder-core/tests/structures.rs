//! Tests for the chunked list, the launch environment, and the home layout.
//!
//! The chunked list's property is that a snapshot does not observe a later append; the launch environment's is that a
//! credential never reaches a child; the layout's is that a relative root is refused.

use std::collections::BTreeMap;
use std::path::Path;

use girder_core::{
    CHUNK_CAPACITY, ChunkedList, HomePaths, is_ambient, is_forbidden, resolve_launch_environment,
};

// ---- chunked list -----------------------------------------------------------------------------

#[test]
fn an_empty_list_has_nothing() {
    let list: ChunkedList<u32> = ChunkedList::new();

    assert!(list.is_empty());
    assert_eq!(list.len(), 0);
    assert_eq!(list.get(0), None);
    assert_eq!(list.last(), None);
}

#[test]
fn an_append_is_visible_in_the_new_list() {
    let empty: ChunkedList<u32> = ChunkedList::new();
    let one = empty.push(1);

    assert_eq!(one.len(), 1);
    assert_eq!(one.get(0), Some(&1));
    assert_eq!(one.last(), Some(&1));
}

#[test]
fn a_snapshot_does_not_observe_a_later_append() {
    // A caller holding a snapshot while another writes must not see its own data move under it.
    let list: ChunkedList<u32> = ChunkedList::new().push(1);
    let snapshot = list.clone();

    let longer = list.push(2);

    assert_eq!(snapshot.len(), 1);
    assert_eq!(longer.len(), 2);
    assert_eq!(snapshot.to_vec(), vec![1]);
}

#[test]
fn a_growth_past_one_chunk_keeps_every_value_in_order() {
    let mut list: ChunkedList<usize> = ChunkedList::new();
    for value in 0..(CHUNK_CAPACITY * 2 + 5) {
        list = list.push(value);
    }

    assert_eq!(list.len(), CHUNK_CAPACITY * 2 + 5);
    assert_eq!(
        list.to_vec(),
        (0..CHUNK_CAPACITY * 2 + 5).collect::<Vec<_>>()
    );
}

#[test]
fn an_index_across_chunk_boundaries_reads_the_right_value() {
    let mut list: ChunkedList<usize> = ChunkedList::new();
    for value in 0..(CHUNK_CAPACITY * 3) {
        list = list.push(value);
    }

    for probe in [
        0,
        CHUNK_CAPACITY - 1,
        CHUNK_CAPACITY,
        CHUNK_CAPACITY * 2,
        CHUNK_CAPACITY * 3 - 1,
    ] {
        assert_eq!(list.get(probe), Some(&probe), "index {probe}");
    }
}

#[test]
fn an_index_past_the_end_is_none() {
    let list = ChunkedList::new().push(1);

    assert_eq!(list.get(1), None);
    assert_eq!(list.get(1_000_000), None);
}

#[test]
fn a_range_is_clamped_to_the_list() {
    // A caller reading a page past the end wants the little there is, not an error about arithmetic.
    let list = ChunkedList::new().push(1).push(2).push(3);

    assert_eq!(list.range(0, 10), vec![&1, &2, &3]);
    assert_eq!(list.range(5, 10), Vec::<&i32>::new());
    assert_eq!(list.range(1, 3), vec![&2, &3]);
}

#[test]
fn extending_appends_in_order() {
    let list = ChunkedList::new().extend([1, 2, 3]);

    assert_eq!(list.to_vec(), vec![1, 2, 3]);
}

#[test]
fn iterating_yields_every_value_in_order() {
    let list = ChunkedList::new().extend([1, 2, 3]);

    assert_eq!(list.iter().copied().collect::<Vec<_>>(), vec![1, 2, 3]);
}

// ---- launch environment -----------------------------------------------------------------------

#[test]
fn a_credential_prefix_is_forbidden() {
    // A prefix rather than a name, because naming each one misses the next one added.
    assert!(is_forbidden("DEEPSEEK_API_KEY"));
    assert!(is_forbidden("AWS_SECRET_ACCESS_KEY"));
    assert!(is_forbidden("GIRDER_HOME"));
    assert!(is_forbidden("GITHUB_TOKEN"));
}

#[test]
fn an_ordinary_variable_is_not_forbidden() {
    assert!(!is_forbidden("PATH"));
    assert!(!is_forbidden("MY_VARIABLE"));
}

#[test]
fn the_ambient_set_is_short_and_useful() {
    assert!(is_ambient("PATH"));
    assert!(is_ambient("HOME"));
    assert!(!is_ambient("RANDOM_VARIABLE"));
}

#[test]
fn a_credential_in_the_parent_does_not_reach_the_child() {
    // The whole point: a command a model asked to run must not receive the harness's own credentials.
    let parent = BTreeMap::from([
        ("PATH".to_owned(), "/usr/bin".to_owned()),
        ("DEEPSEEK_API_KEY".to_owned(), "sk-secret".to_owned()),
    ]);

    let resolved = resolve_launch_environment(&parent, &BTreeMap::new());

    assert_eq!(resolved.get("PATH"), Some("/usr/bin"));
    assert!(!resolved.contains("DEEPSEEK_API_KEY"));
}

#[test]
fn a_withheld_variable_is_recorded() {
    // So a reader can connect a command's authentication failure to the variable it was denied.
    let parent = BTreeMap::from([("AWS_SECRET_ACCESS_KEY".to_owned(), "x".to_owned())]);

    let resolved = resolve_launch_environment(&parent, &BTreeMap::new());

    assert_eq!(resolved.withheld, vec!["AWS_SECRET_ACCESS_KEY"]);
}

#[test]
fn an_extra_credential_is_dropped_rather_than_honoured() {
    // A caller asking for a credential to pass through is making the mistake this module prevents.
    let extra = BTreeMap::from([("OPENAI_API_KEY".to_owned(), "sk-x".to_owned())]);

    let resolved = resolve_launch_environment(&BTreeMap::new(), &extra);

    assert!(!resolved.contains("OPENAI_API_KEY"));
    assert_eq!(resolved.withheld, vec!["OPENAI_API_KEY"]);
}

#[test]
fn an_extra_variable_is_added() {
    let extra = BTreeMap::from([("MY_FLAG".to_owned(), "1".to_owned())]);

    let resolved = resolve_launch_environment(&BTreeMap::new(), &extra);

    assert_eq!(resolved.get("MY_FLAG"), Some("1"));
}

#[test]
fn an_ambient_variable_the_parent_lacks_is_absent() {
    let resolved = resolve_launch_environment(&BTreeMap::new(), &BTreeMap::new());

    assert!(!resolved.contains("PATH"));
    assert!(resolved.values.is_empty());
}

// ---- home layout ------------------------------------------------------------------------------

#[test]
fn a_relative_root_is_refused() {
    // It would resolve against the launch directory, and a later run would not find the sessions.
    assert!(HomePaths::under("./data").is_err());
    assert!(HomePaths::under("data").is_err());
    assert!(HomePaths::under("").is_err());
}

#[test]
fn an_absolute_root_is_accepted() {
    assert!(HomePaths::under("/var/lib/girder").is_ok());
}

#[test]
fn the_directories_are_distinct_and_under_the_root() {
    let paths = HomePaths::under("/var/lib/girder").expect("absolute");

    let directories = paths.directories();
    assert_eq!(directories.len(), 6);
    assert!(
        directories
            .iter()
            .all(|path| path.starts_with("/var/lib/girder"))
    );

    let unique: std::collections::BTreeSet<_> = directories.iter().collect();
    assert_eq!(unique.len(), directories.len(), "no directory repeats");
}

#[test]
fn an_override_wins_over_the_home_directory() {
    let paths =
        HomePaths::from_environment(Some("/home/user"), Some("/srv/girder")).expect("valid");

    assert_eq!(paths.root(), Path::new("/srv/girder"));
}

#[test]
fn the_home_directory_get_the_harness_subdirectory() {
    let paths = HomePaths::from_environment(Some("/home/user"), None).expect("valid");

    assert_eq!(paths.root(), Path::new("/home/user/.girder"));
}

#[test]
fn a_relative_override_is_refused_even_with_a_home() {
    let error = HomePaths::from_environment(Some("/home/user"), Some("./x")).expect_err("relative");

    assert!(error.contains("absolute"), "{error}");
}

#[test]
fn credentials_have_their_own_directory() {
    // Separate so its permissions can be tighter: a directory that also holds logs cannot be `0700`.
    let paths = HomePaths::under("/srv/girder").expect("absolute");

    assert_ne!(paths.credentials(), paths.sessions());
    assert!(paths.credentials().ends_with("credentials"));
}
