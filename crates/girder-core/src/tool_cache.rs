//! A bounded, mtime-invalidated cache for tool results.
//!
//! Nothing in the harness caches a tool result today. [`crate::response_cache`] remembers what the
//! *model* answered, but `agent_loop::dispatch` runs the tool body on every call, so two turns that
//! both read the same file cost two disk reads and two identical copies in the transcript. This
//! module is the missing memory: a key that identifies one call by tool name plus raw arguments, and
//! a store of the text it produced.
//!
//! # The failure this module is built around
//!
//! A stale cache does not fail loudly. It hands the model a file's *old* contents with the shape of
//! a fresh read, and every decision the model makes afterwards is based on text that is no longer
//! on disk. A cache that is merely slow is a wasted feature; a cache that is wrong is worse than no
//! cache at all. Two independent gates therefore stand between a tool result and the store:
//!
//! 1. [`is_cacheable`] admits only tools whose result is a pure function of the arguments and one
//!    file's bytes. A *writer* must never be admitted: caching `write` would make the second call
//!    return the first call's receipt without touching the file at all, which is silent data loss.
//!    `put` re-checks this gate on the key itself, so a wiring mistake cannot smuggle a writer in.
//! 2. Every entry carries the source file's `mtime` at store time, and [`Cache::get`] compares it
//!    with the `mtime` the caller observes now. An edit moves the mtime, the comparison fails, and
//!    the entry is discarded instead of served.
//!
//! The second gate is the reason entries are not simply time-boxed: a TTL would serve a stale file
//! for the length of the timeout and *also* miss a cache hit on an untouched file that was read a
//! moment too long ago. The file's own mtime answers both questions exactly.
//!
//! # Why `bash` is never cacheable
//!
//! `bash` output is not a function of its command line: it reports the clock, the process table, the
//! network, and whatever a previous command left behind. There is no source file to stamp, so no
//! mtime can invalidate it, and the only correct policy is to never store it. The same reasoning
//! excludes `web_fetch` and `web_search`, whose remote content changes without any local mtime
//! moving.
//!
//! # Why the key is not a hash
//!
//! A digest would shrink the key and, on a birthday schedule, let one call read another call's
//! result. The encoding is length-prefixed instead, so two different calls cannot collide at all; a
//! bounded map of small strings is not worth trading that for.

// The crate root is wired up separately, so until then nothing in the library references these
// items. The allow is scoped to this module and disappears once it is exported.

use std::collections::HashMap;
use std::fmt::Write as _;

/// The tools whose results may be stored.
///
/// Deliberately an allowlist rather than a denylist of writers: a new tool is unknown, and the safe
/// default for an unknown tool is "do not cache", because the wrong choice is a silently stale
/// result while the right choice only costs a disk read.
///
/// `read` is the only entry today. It is read-only, and its entire observable output is a function
/// of its arguments plus the bytes of exactly one file, which is what makes a single `mtime` an
/// adequate invalidation token. `glob` and `grep` are also read-only but are *not* listed: their
/// output depends on a whole directory tree, and one file's mtime cannot prove that no file was
/// added, removed, or renamed. Admitting them needs a directory snapshot, not this gate.
const CACHEABLE_TOOLS: &[&str] = &["read"];

/// Which tools' results may be cached.
///
/// A caller must consult this before [`Cache::get`] and [`Cache::put`]; `put` repeats the check for
/// its own key, but a lookup of an entry that was never stored only costs a counter.
///
/// Rejects, by name, every tool that changes state or whose output changes on its own:
///
/// - `write`, `edit`, `apply_patch`, `bash`: they mutate the workspace or the machine. Storing their
///   result would let a second identical call skip execution entirely while still reporting success.
/// - `terminal_*`, `job_*`: their output is a live process, not a snapshot.
/// - `web_fetch`, `web_search`: remote content with no local mtime to invalidate against.
/// - `glob`, `grep`, `lsp`, `session_search`: read-only, but their source is a tree or a service
///   rather than the one file whose mtime a caller can pass to [`Cache::get`].
/// - `ask_user`, `present`, `skill`, `subagent*`, `todo_write`, `goal`, `workflow_*`: side effects on
///   the session or on a human, and a repeated call is expected to happen again.
#[must_use]
pub fn is_cacheable(tool: &str) -> bool {
    CACHEABLE_TOOLS.contains(&tool)
}

/// One tool call's identity.
///
/// Tool name *and* raw arguments, because two reads of the same path with different `offset`/`limit`
/// are different calls and must not answer for each other. The arguments are the JSON text exactly
/// as the model produced it: re-serializing would risk normalizing two calls into one key, and a
/// key collision here means returning another call's output. The cost is that two textually
/// different spellings of the same arguments both miss, which is the harmless direction.
#[derive(Clone, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct Key(String);

impl Key {
    /// Builds a key from a tool name and its raw argument text.
    #[must_use]
    pub fn of(tool: &str, arguments: &str) -> Self {
        // Length prefixes instead of a separator: every separator can occur inside a tool name or an
        // argument string, so `read` + `{"path":"a"}` and `read{"path"}` + `"a"` would otherwise
        // encode identically. The lengths make the field boundaries unambiguous by construction.
        let mut key = String::with_capacity(tool.len() + arguments.len() + 24);
        // Writing into a `String` cannot fail; the result is dropped rather than unwrapped so this
        // module keeps its promise of having no panic path.
        let _ = write!(key, "{}:{tool}{}:{arguments}", tool.len(), arguments.len());
        Self(key)
    }

    /// The encoded key, for logging and for use as a map key elsewhere.
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }

    /// The tool name this key was built from.
    ///
    /// Recovered from the encoding rather than stored twice, so a key is still one small string. It
    /// is total for every key this module can build; a key that somehow fails to decode reports an
    /// empty name, which [`is_cacheable`] rejects — the entry is dropped rather than trusted.
    fn tool(&self) -> &str {
        let Some((length, rest)) = self.0.split_once(':') else {
            return "";
        };
        let Ok(length) = length.parse::<usize>() else {
            return "";
        };
        // `get` rather than slicing: a byte count that lands mid-character or past the end must not
        // panic on text that arrived from a model.
        rest.get(..length).unwrap_or("")
    }
}

/// One cached tool result.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Entry {
    /// The text the tool produced, stored verbatim so a hit is indistinguishable from a miss.
    pub text: String,
    /// Whether the tool reported a failure.
    ///
    /// Preserved rather than dropped: a cached "cannot read that file" is still the answer to the
    /// same call, and turning it into a success on the second attempt would hide the failure.
    pub is_error: bool,
    /// The source file's modification time when this entry was stored, in milliseconds since the
    /// Unix epoch.
    ///
    /// [`Cache::get`] refuses the entry once this no longer matches what the caller observes. `None`
    /// means the call had no single file to watch — which, given [`is_cacheable`], no cacheable call
    /// does today; it is accepted so that a future tool with a genuinely immutable source can opt out
    /// of invalidation, and it only ever matches a caller that also passes `None`.
    pub source_mtime_ms: Option<u64>,
}

/// One entry plus the evidence of when it was last read.
struct Held {
    entry: Entry,
    /// A logical clock rather than a wall clock: monotonic, unaffected by the system clock moving,
    /// and unique per access, which is what makes "least recently used" unambiguous.
    last_used: u64,
}

/// Hit, miss, and refusal counts.
///
/// Raw counters with the rate derived, because a rate alone cannot distinguish "one hit out of two"
/// from "a thousand out of two thousand", and only the second is evidence the cache is working.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Stats {
    /// Lookups served from memory.
    pub hits: u64,
    /// Lookups that found nothing, including lookups that found an invalidated entry.
    pub misses: u64,
    /// Entries dropped because their source file's mtime no longer matched.
    ///
    /// Split out from `misses` so an operator can tell "the cache is cold" from "the file keeps
    /// changing", which are different problems with different fixes.
    pub stale: u64,
    /// Entries dropped to make room for another, or refused because the cache holds nothing.
    pub evictions: u64,
    /// Stores refused because the key named a tool that is not cacheable.
    ///
    /// Non-zero here is a wiring bug, not normal operation: it means a caller tried to cache a
    /// writer. Counting it makes that mistake visible instead of silent.
    pub refused: u64,
}

impl Stats {
    /// Lookups served, hit or miss.
    ///
    /// Saturating because a counter overflow must not wrap a huge total down to a small one, which
    /// would make the hit rate look better than it is.
    #[must_use]
    pub fn requests(&self) -> u64 {
        self.hits.saturating_add(self.misses)
    }

    /// Hits divided by lookups, or `0.0` when nothing was looked up.
    ///
    /// Zero rather than `NaN` for the empty case: a cache that has not been used has no rate, and a
    /// `NaN` propagating into a metric poisons every average it enters.
    #[must_use]
    #[allow(clippy::cast_precision_loss)] // A hit count above 2^53 is unreachable by a counter in a process lifetime; the ratio only needs display precision.
    pub fn hit_rate(&self) -> f64 {
        let requests = self.requests();
        if requests == 0 {
            return 0.0;
        }
        self.hits as f64 / requests as f64
    }
}

/// A bounded least-recently-used cache of tool results.
///
/// Eviction is by *last use*, not by insertion order: a file the model re-reads on every turn has to
/// survive while a file read once ages out. [`Cache::get`] therefore takes `&mut self` — a read is a
/// write to the recency order.
///
/// [`Cache::get`] also takes the source file's current mtime, and that argument is not optional in
/// spirit even though it is optional in type: passing a value that was not read from the file being
/// cached disables the only protection this module has against serving stale content.
pub struct Cache {
    capacity: usize,
    /// Logical clock; see [`Held::last_used`].
    clock: u64,
    hits: u64,
    misses: u64,
    stale: u64,
    evictions: u64,
    refused: u64,
    entries: HashMap<Key, Held>,
}

impl Cache {
    /// Builds a cache holding at most `capacity` results.
    ///
    /// `0` is legal and makes every [`Cache::put`] a no-op, which is how a caller disables caching
    /// without branching at every call site.
    #[must_use]
    pub fn new(capacity: usize) -> Self {
        Self {
            capacity,
            clock: 0,
            hits: 0,
            misses: 0,
            stale: 0,
            evictions: 0,
            refused: 0,
            entries: HashMap::new(),
        }
    }

    /// Looks up a result, provided its source file has not changed since it was stored.
    ///
    /// `now_mtime_ms` is the modification time of the file this call reads, observed by the caller
    /// immediately before the lookup. A mismatch — in either direction, including `Some` becoming
    /// `None` because the file was deleted — drops the entry and reports a miss. The entry is
    /// removed rather than merely skipped: keeping it would leak a result that can never be served.
    ///
    /// Counts a hit or a miss. A refused or invalidated entry is a miss, because from the caller's
    /// point of view the lookup did not produce an answer.
    pub fn get(&mut self, key: &Key, now_mtime_ms: Option<u64>) -> Option<&Entry> {
        self.clock = self.clock.wrapping_add(1);
        let tick = self.clock;

        // Checked before taking a mutable borrow of the entry, so the removal below is not fighting
        // the borrow of the value being removed.
        let fresh = self
            .entries
            .get(key)
            .is_some_and(|held| held.entry.source_mtime_ms == now_mtime_ms);
        if !fresh {
            if self.entries.remove(key).is_some() {
                self.stale = self.stale.saturating_add(1);
            }
            self.misses = self.misses.saturating_add(1);
            return None;
        }

        self.hits = self.hits.saturating_add(1);
        let Some(held) = self.entries.get_mut(key) else {
            // Unreachable: the presence check above and this map are the same map, under the same
            // `&mut self`. Handled by reporting a miss rather than by panicking in production code.
            self.hits = self.hits.saturating_sub(1);
            self.misses = self.misses.saturating_add(1);
            return None;
        };
        held.last_used = tick;
        Some(&held.entry)
    }

    /// Stores a result, replacing any entry for the same key and evicting the least recently used
    /// entry when the cache is full.
    ///
    /// The key's tool is checked against [`is_cacheable`] here, not only at the call site: a writer
    /// that reaches this method must be refused, because serving a cached `write` would skip the
    /// write. A refused key is dropped silently but counted in [`Stats::refused`].
    ///
    /// A `put` is not a lookup, so it changes no hit or miss counter.
    pub fn put(&mut self, key: Key, entry: Entry) {
        if !is_cacheable(key.tool()) {
            self.refused = self.refused.saturating_add(1);
            return;
        }
        if self.capacity == 0 {
            return;
        }
        self.clock = self.clock.wrapping_add(1);
        let tick = self.clock;

        if let Some(held) = self.entries.get_mut(&key) {
            held.entry = entry;
            held.last_used = tick;
            return;
        }
        if self.entries.len() >= self.capacity {
            self.evict_one();
        }
        self.entries.insert(
            key,
            Held {
                entry,
                last_used: tick,
            },
        );
    }

    /// Drops one entry, reporting whether one was present.
    ///
    /// For a caller that knows its source changed without waiting for the next lookup to notice —
    /// after its own `write`, for instance, where the mtime may not have moved on a coarse-grained
    /// filesystem.
    pub fn invalidate(&mut self, key: &Key) -> bool {
        self.entries.remove(key).is_some()
    }

    /// Drops every entry.
    ///
    /// The counters survive on purpose: they are the session's evidence about how well caching
    /// worked, and zeroing them on a flush would erase exactly the numbers an operator is looking
    /// at. Nothing about the counters can be stale.
    pub fn clear(&mut self) {
        self.entries.clear();
    }

    /// Removes the entry whose last use is furthest in the past.
    ///
    /// A linear scan rather than a linked list: the capacity is small enough that the scan costs less
    /// than an intrusive list's bookkeeping, and a single index cannot desynchronize from the map the
    /// way a second one can. The tick is unique per access, so the minimum is unambiguous.
    fn evict_one(&mut self) {
        let victim = self
            .entries
            .iter()
            .min_by_key(|(_, held)| held.last_used)
            .map(|(key, _)| key.clone());
        if let Some(key) = victim {
            self.entries.remove(&key);
            self.evictions = self.evictions.saturating_add(1);
        }
    }

    /// Hit, miss, and refusal counts so far.
    #[must_use]
    pub fn stats(&self) -> Stats {
        Stats {
            hits: self.hits,
            misses: self.misses,
            stale: self.stale,
            evictions: self.evictions,
            refused: self.refused,
        }
    }

    /// How many results are held.
    #[must_use]
    pub fn len(&self) -> usize {
        self.entries.len()
    }

    /// Whether the cache holds nothing.
    ///
    /// Present because a `len` without it forces each caller to write `== 0` and to remember that a
    /// zero-capacity cache is a valid, permanently empty one.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    /// The most results this cache will hold.
    #[must_use]
    pub fn capacity(&self) -> usize {
        self.capacity
    }
}

/// `clock` is omitted rather than printed: it is a bookkeeping counter whose absolute value says
/// nothing a reader can use, and `len` already answers the question it would be consulted for.
#[allow(clippy::missing_fields_in_debug)]
impl std::fmt::Debug for Cache {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        // Deliberately not derived: an entry is a whole file's text, and printing it would put a
        // file's contents in a log line. The counters and the size are what a diagnostic needs.
        f.debug_struct("Cache")
            .field("capacity", &self.capacity)
            .field("len", &self.entries.len())
            .field("hits", &self.hits)
            .field("misses", &self.misses)
            .field("stale", &self.stale)
            .field("evictions", &self.evictions)
            .field("refused", &self.refused)
            .finish()
    }
}
