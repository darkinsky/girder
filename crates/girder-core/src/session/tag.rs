//! Tags on a session: a short name a person — or a one-shot rule — attaches to a conversation.
//!
//! `session_title` answers "what is this conversation called", and `session_filter` answers "which
//! sessions are recent, long, or tool-using". Neither answers "show me the bug fixes". A title is
//! free text, so the best a filter can do with it is a substring match; a session has no field a
//! reader can *choose* from a fixed vocabulary. That is the gap this module fills.
//!
//! # What is in scope, and what is not
//!
//! This module owns the vocabulary and nothing else: what a tag is, how its name is spelled, and
//! which combinations of tags are contradictory. It does not read or write a `.tags` sidecar, does
//! not know about `Layout`, and does not know about the catalog. Persistence belongs with the rest of
//! a session's on-disk states (see `session_layout`), and a value type that reached for a path would
//! be untestable without a directory.
//!
//! # Why one spelling per tag, enforced here
//!
//! The caller is a text box. A person types "Bug Fix" on Monday and "bug-fix" on Tuesday, and a set
//! that stored both would show two identical-looking rows, count the same session twice in a tag
//! cloud, and silently split a filter's results in half. Case and spacing are not information here —
//! nobody means a different thing by "BUG FIX" — so [`normalize`] is the single spelling, and both
//! [`TagSet::add`] and the lookups run through it. The rule lives in the type rather than in each
//! caller because the alternative is every call site remembering to lowercase, which is exactly the
//! kind of invariant that holds until the third caller.
//!
//! # Why a duplicate is an error rather than a no-op
//!
//! `add` returns [`TagError::Duplicate`] because the caller needs to know its write did not happen.
//! A silently ignored add looks identical to a successful one until someone notices the tag is
//! missing; worse, the two plausible "fixes" for a silent ignore — re-adding, or renaming slightly —
//! both make the problem harder to see. An error lets a UI say "already tagged" and lets a rule
//! engine count how often inference proposes something a human already said.
//!
//! # Manual and inferred tags may not share a name
//!
//! A set holds at most one tag per normalized name, whatever the source. The reasoning is that the
//! two sources answer the same question — "is this session about X" — and a set that held a manual
//! `bug-fix` *and* an inferred `bug-fix` would have to decide, at every read, which one to believe.
//! Instead:
//!
//! - An inferred tag never displaces a manual one: adding it is rejected as a duplicate, so a rule
//!   that fires on every session cannot overwrite what the person typed.
//! - The reverse is deliberately *also* a duplicate, but with a way forward —
//!   [`TagSet::promote`] turns an existing inferred tag into a manual one. That is the honest
//!   operation for "the user confirmed what the rule guessed": the name is unchanged, and the
//!   confidence that was only ever a guess is dropped.
//! - `add` itself never mutates an existing tag. If it did, a caller retrying an add would silently
//!   rewrite a source or a confidence, which is a write nobody asked for.
//!
//! # Why the length cap
//!
//! A tag is a label, not a sentence. Without a cap, "failed to reproduce on the second attempt with
//! the staging token" is a valid tag, and a vocabulary that accepts that is just titles again. The
//! cap is on characters rather than bytes so that the number means what a person would count, and so
//! a Chinese tag gets the same budget as a Latin one.

use std::collections::BTreeMap;

/// The longest permitted tag name, in characters.
///
/// Exposed because a UI has to enforce the same bound while the person types; a private limit would
/// have the text box accept input the model rejects.
pub const MAX_NAME_CHARS: usize = 64;

/// The highest confidence [`Source::Inferred`] carries.
///
/// A percentage, because a rule's confidence is understood as one. The set clamps to this rather
/// than trusting the caller: a stored 300 would compare oddly against a 0..=100 threshold that a
/// later reader assumed.
pub const MAX_CONFIDENCE: u8 = 100;

/// One tag.
///
/// `name` is always the normalized spelling once the tag is inside a [`TagSet`] — see
/// [`TagSet::add`]. It is a plain field rather than a method so a caller can render it without a
/// borrow dance, and a caller that constructs one by hand may use any spelling because `add`
/// normalizes on the way in.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Tag {
    /// The name, lowercase and hyphen-separated.
    pub name: String,
    /// Where it came from.
    pub source: Source,
}

/// Where a tag came from.
///
/// The distinction is not cosmetic: it decides how the tag may be treated when the two sources
/// disagree about a name ([`TagSet::add`], [`TagSet::promote`]), and a UI usually shows an inferred
/// tag more tentatively than one a person typed.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Source {
    /// The user added it.
    ///
    /// Never expires and is never re-derived, because a person's assertion does not need
    /// re-confirming.
    Manual,
    /// A one-shot rule inferred it from the title or the content.
    ///
    /// "One-shot" is the important part: the rule runs once, at the moment it has the text, rather
    /// than being re-evaluated against a conversation that keeps growing. That keeps a tag stable
    /// across a session's life, at the cost of not knowing something a later turn would have said.
    Inferred {
        /// How sure the rule was, as a percentage: `0` is a wild guess, [`MAX_CONFIDENCE`] is
        /// certain. Values above the maximum are clamped when the tag is added.
        confidence: u8,
    },
}

/// A rejected tag operation.
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
pub enum TagError {
    /// The name was empty once normalized, so there is nothing to store.
    ///
    /// A tag whose name is only spaces would render as a blank chip: present in the set, invisible
    /// in the list, and impossible to remove by the name a person would type.
    #[error("tag name is empty after normalization")]
    Empty,
    /// The name was longer than [`MAX_NAME_CHARS`].
    #[error("tag name is {length} characters, over the limit of {max}")]
    TooLong {
        /// The normalized name's length, in characters.
        length: usize,
        /// The limit that was exceeded, i.e. [`MAX_NAME_CHARS`].
        max: usize,
    },
    /// The set already holds a tag with that normalized name.
    ///
    /// Carries the *normalized* name rather than the one the caller passed, so the message names the
    /// tag that was actually found — which is the one a caller would pass to `remove` or `promote`.
    #[error("a tag named `{name}` is already in the set")]
    Duplicate {
        /// The normalized name that collided.
        name: String,
    },
}

/// A set of tags, at most one per normalized name.
///
/// Backed by an ordered map keyed on the normalized name, which gives three properties for free that
/// a `Vec<Tag>` would have to re-establish on every call: uniqueness (the key), stable ordering
/// ([`TagSet::all`] is sorted, so two equal sets render identically and a test can assert an order
/// without depending on insertion), and O(log n) membership. Sets are compared by name and source
/// together, so a manual `bug-fix` is not equal to an inferred one.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct TagSet {
    /// Keyed on the normalized name, so the key and `Tag::name` always agree.
    tags: BTreeMap<String, Tag>,
}

impl TagSet {
    /// An empty set.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Adds a tag, normalizing its name.
    ///
    /// The name is normalized and then validated, in that order: whitespace-only input normalizes to
    /// nothing and is [`TagError::Empty`], and the length check applies to the normalized form so a
    /// name padded with spaces is not rejected for its padding. On success the stored tag carries the
    /// normalized name and, for an inferred tag, a confidence clamped to [`MAX_CONFIDENCE`].
    ///
    /// Adding a name that is already present fails, *even when the existing tag has the other
    /// source*: the module documentation explains why a manual tag must not silently replace an
    /// inferred one and why the reverse is nonetheless an error rather than a silent upgrade. Use
    /// [`TagSet::promote`] to record that a person confirmed an inferred tag.
    ///
    /// # Errors
    ///
    /// [`TagError::Empty`], [`TagError::TooLong`], or [`TagError::Duplicate`]. The set is unchanged
    /// in every case.
    ///
    /// The tag is taken by reference and its name is re-derived rather than reused: this keeps one
    /// spelling of the name in the whole module — [`normalize`] — instead of letting a caller hand in
    /// a value whose `name` was normalized by something else and have it stored verbatim.
    pub fn add(&mut self, tag: &Tag) -> Result<(), TagError> {
        let name = normalize(&tag.name);
        if name.is_empty() {
            return Err(TagError::Empty);
        }
        // Characters, not bytes: the limit is a count a reader would make, and a byte limit would
        // silently give a Chinese tag a third of the budget of a Latin one.
        let length = name.chars().count();
        if length > MAX_NAME_CHARS {
            return Err(TagError::TooLong {
                length,
                max: MAX_NAME_CHARS,
            });
        }
        if self.tags.contains_key(&name) {
            return Err(TagError::Duplicate { name });
        }
        let source = match tag.source {
            // Clamped rather than rejected: the confidence is advisory, and refusing an otherwise
            // good tag over it would lose the person's label to a rule's arithmetic.
            Source::Inferred { confidence } => Source::Inferred {
                confidence: confidence.min(MAX_CONFIDENCE),
            },
            Source::Manual => Source::Manual,
        };
        // Cloned because both the key and the value need the name: the key is what lookup and
        // ordering use, and the value's `name` is what a caller renders. Sharing one `String` would
        // need a self-referential map, and a tag handed out by `all` must own its name so it stays
        // valid without borrowing back into the map.
        self.tags.insert(name.clone(), Tag { name, source });
        Ok(())
    }

    /// Removes the tag with this name, returning whether one was there.
    ///
    /// The name is normalized, so `remove("Bug Fix")` removes `bug-fix`. The boolean is the answer to
    /// "did anything change": a UI that showed a confirmation toast needs to know whether the tag
    /// existed, and a caller removing several names in a loop can count what actually happened.
    pub fn remove(&mut self, name: &str) -> bool {
        self.tags.remove(&normalize(name)).is_some()
    }

    /// Every tag, sorted by name.
    ///
    /// Sorted because the map is ordered: a tag list that reshuffled between two reads of an
    /// unchanged set would look like it was moving under the reader, and a diff of two sets would be
    /// noise.
    #[must_use]
    pub fn all(&self) -> Vec<&Tag> {
        self.tags.values().collect()
    }

    /// Whether the set holds this tag, under any spelling that normalizes to it.
    #[must_use]
    pub fn has(&self, name: &str) -> bool {
        self.tags.contains_key(&normalize(name))
    }

    /// How many tags the set holds.
    #[must_use]
    pub fn len(&self) -> usize {
        self.tags.len()
    }

    /// Whether the set holds no tags.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.tags.is_empty()
    }

    /// Turns an inferred tag into a manual one, keeping its name.
    ///
    /// This is the one sanctioned way for a person to take ownership of something a rule guessed.
    /// It is a separate operation rather than an overload of [`TagSet::add`] because the resulting
    /// write is different in kind: `add` inserts, `promote` rewrites a source, and a caller reading
    /// its own code should be able to see which one it asked for.
    ///
    /// Returns whether a tag of that name is in the set — `false` means there was nothing to
    /// promote. Promoting a tag that is already [`Source::Manual`] succeeds and changes nothing, so
    /// a caller need not check the source first.
    pub fn promote(&mut self, name: &str) -> bool {
        let Some(tag) = self.tags.get_mut(&normalize(name)) else {
            return false;
        };
        tag.source = Source::Manual;
        true
    }
}

/// Normalizes a tag name.
///
/// The rule, in order: lower-case every character, treat every run of whitespace, `-`, and `_` as a
/// single separator, spell that separator `-`, and drop separators at either end. So `"Bug Fix"`,
/// `" bug-fix "`, `"BUG__FIX"`, and `"bug \n fix"` all become `bug-fix`.
///
/// Other punctuation is kept, because it can carry meaning: `#rust` and `rust` are different tags to
/// whoever typed the `#`, and folding them would lose information the person supplied. Underscores
/// and runs, by contrast, are spelling — `snake_case` and `kebab-case` are the same word.
///
/// Unicode case folding is used rather than ASCII-only lowercasing, so a tag in a non-Latin script is
/// matched the way its reader would match it. Returns an empty string when nothing survives, which
/// is what makes [`TagError::Empty`] decidable.
#[must_use]
pub fn normalize(name: &str) -> String {
    let mut normalized = String::with_capacity(name.len());
    // Remembered rather than pushed immediately: a run of separators must collapse to one hyphen,
    // and a leading run must produce none at all.
    let mut separator_pending = false;
    for character in name.chars() {
        if is_separator(character) {
            // Only interesting once something precedes it; otherwise the hyphen would land at the
            // front and the tag would no longer match its own normalized spelling.
            separator_pending = !normalized.is_empty();
            continue;
        }
        if separator_pending {
            normalized.push('-');
            separator_pending = false;
        }
        // `to_lowercase` can expand one character into several, so the result is pushed per char
        // rather than assigned; dropping the extra characters would mangle such a name.
        for lower in character.to_lowercase() {
            normalized.push(lower);
        }
    }
    normalized
}

/// Whether a character is spelling rather than content.
///
/// Whitespace covers newlines and tabs as well as spaces: a tag pasted out of a paragraph arrives
/// with all three, and each one is the same separation to a reader.
fn is_separator(character: char) -> bool {
    character.is_whitespace() || character == '-' || character == '_'
}
