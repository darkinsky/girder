//! Service isolation: a child handle that resolves one service name against its own label.
//!
//! `Registry::isolate` publishes one service name into a child label. These tests are what would
//! notice if two subtrees stopped being isolated from each other.
//!
//! The property under test is the one upstream's `isolate()` provides: two plugin subtrees can each
//! publish a service under the same *name* without either seeing the other, which is what makes it
//! possible for a group of agents to each own a `session` while sharing one `llm`.

use std::sync::Arc;

use girder_core::{Label, Registry, RegistryError, ServiceKey};

// ---- one service name, several implementations --------------------------------------------------

/// A service name whose implementations are distinguishable by what they say.
trait Greeter: Send + Sync {
    fn greet(&self) -> String;
}

struct GreeterKey;

impl ServiceKey for GreeterKey {
    type Service = dyn Greeter;
    const ID: &'static str = "greeter";
}

/// A greeter with a fixed word.
struct Says(&'static str);

impl Greeter for Says {
    fn greet(&self) -> String {
        self.0.to_owned()
    }
}

/// Registers a greeter that says `word` through `context`.
fn provide(context: &girder_core::RegistryContext, word: &'static str) {
    context
        .register::<GreeterKey>(Arc::new(Says(word)))
        .expect("the handle is fresh");
}

/// The word a handle resolves, or `None` when it resolves nothing.
fn word_of(context: &girder_core::RegistryContext) -> Option<String> {
    context.get::<GreeterKey>().ok().map(|s| s.greet())
}

// ---- isolation ----------------------------------------------------------------------------------

#[test]
fn a_child_can_publish_a_service_under_a_name_the_parent_already_provides() {
    let registry = Registry::new();
    let root = registry.context("root");
    provide(&root, "root");

    let child = registry.isolate(&root, "greeter", None);
    // The same *name*, a different implementation, and no duplicate-registration refusal -- because
    // the two live at different labels.
    provide(&child, "child");

    assert_eq!(word_of(&root).as_deref(), Some("root"));
    assert_eq!(word_of(&child).as_deref(), Some("child"));
}

#[test]
fn a_child_never_sees_the_parents_implementation_through_its_own_label() {
    // This is the half that upstream's doc comment states explicitly: "a lookup through it never
    // sees the parent's provider". A child that fell back to the parent would make isolation look
    // like it worked while a plugin silently used the wrong implementation.
    let registry = Registry::new();
    let root = registry.context("root");
    provide(&root, "root");

    let child = registry.isolate(&root, "greeter", None);
    assert_eq!(
        word_of(&child),
        None,
        "an isolated child has no provider until one publishes through it"
    );
}

#[test]
fn the_parent_never_sees_the_childs_implementation() {
    let registry = Registry::new();
    let root = registry.context("root");
    provide(&root, "root");

    let child = registry.isolate(&root, "greeter", None);
    provide(&child, "child");

    assert_eq!(
        word_of(&root).as_deref(),
        Some("root"),
        "publishing in a child leaves the parent's provider alone"
    );
}

#[test]
fn two_children_with_distinct_labels_are_independent() {
    let registry = Registry::new();
    let root = registry.context("root");
    let first = registry.isolate(&root, "greeter", None);
    let second = registry.isolate(&root, "greeter", None);
    // Both isolates were requested with no label, so each got a fresh one.
    assert_ne!(
        first.label_for("greeter"),
        second.label_for("greeter"),
        "a fresh label is what keeps two children apart"
    );

    provide(&first, "first");
    assert_eq!(word_of(&first).as_deref(), Some("first"));
    assert_eq!(word_of(&second), None, "the second child is untouched");
}

#[test]
fn two_children_sharing_a_label_share_the_implementation() {
    // The documented use: a group of agents each own a `session` while sharing one `llm`. Sharing
    // is requested by passing the same label, not by inheriting it.
    let registry = Registry::new();
    let root = registry.context("root");
    let shared: Label = 7;
    let first = registry.isolate(&root, "greeter", Some(shared));
    let second = registry.isolate(&root, "greeter", Some(shared));

    provide(&first, "shared");
    assert_eq!(
        word_of(&second).as_deref(),
        Some("shared"),
        "one label means one implementation"
    );
}

#[test]
fn isolating_one_name_leaves_every_other_name_alone() {
    // Isolation is per name: a child that isolates `greeter` still resolves `counter` through the
    // parent's label, which is what lets a plugin own one service while sharing the rest.
    let registry = Registry::new();
    let root = registry.context("root");
    provide(&root, "root");

    let child = registry.isolate(&root, "unrelated", None);
    assert_eq!(
        child.label_for("greeter"),
        0,
        "`greeter` was not isolated, so it still resolves at the root label"
    );
    assert_eq!(word_of(&child).as_deref(), Some("root"));
}

#[test]
fn isolating_twice_compounds_rather_than_replaces() {
    // A nested isolate keeps the outer one's entry and adds its own. Losing the outer entry would
    // make a grandchild resolve against the root for the name its parent isolated.
    let registry = Registry::new();
    let root = registry.context("root");
    let child = registry.isolate(&root, "greeter", None);
    let grandchild = registry.isolate(&child, "counter", None);

    assert_eq!(
        grandchild.label_for("greeter"),
        child.label_for("greeter"),
        "the outer isolation survives"
    );
    assert_ne!(
        grandchild.label_for("counter"),
        0,
        "the grandchild isolated `counter`, so it got a fresh label"
    );
    assert_eq!(
        child.label_for("counter"),
        0,
        "the parent never isolated `counter`, so it still resolves at the root label -- isolation          does not propagate downward"
    );
}

#[test]
fn a_duplicate_registration_at_one_label_is_refused() {
    // Isolation is what makes two implementations of one name legal. At a *single* label, a second
    // registration is still a mistake the registry reports.
    let registry = Registry::new();
    let root = registry.context("root");
    provide(&root, "first");
    let error = root
        .register::<GreeterKey>(Arc::new(Says("second")))
        .expect_err("one label holds one provider per name");
    assert!(
        matches!(error, RegistryError::Duplicate { .. }),
        "{error:?} must name the collision"
    );
    assert_eq!(
        word_of(&root).as_deref(),
        Some("first"),
        "a refused registration leaves the incumbent in place"
    );
}
