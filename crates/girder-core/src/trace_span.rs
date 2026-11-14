//! A tree of timed spans for one request: what ran, how long it took, and what it was nested in.
//!
//! One request to the harness is a turn; a turn is several steps; a step is a model call plus the tool
//! calls it asked for. A flat list of numbers cannot say which tool call belonged to which step, and a
//! reader trying to answer "where did the four seconds go" needs the nesting as much as the numbers.
//! [`Span`] is a node in that tree and [`Tracer`] builds one as the work runs.
//!
//! # The clock is injected, not read
//!
//! Nothing here calls [`std::time::Instant::now`]. Every constructor takes `now_ms`, so the caller
//! owns the clock and tests can assert an exact number instead of a range. A tracer that read the
//! clock itself would be untestable without sleeping, and a test that sleeps is a test that fails on a
//! loaded machine. The same reason [`crate::llm_stats`] takes `(time, chunk)` pairs: measurement and
//! observation are separate concerns, and only the second one belongs in a pure function.
//!
//! The values are expected to come from a monotonic clock in milliseconds. They are deliberately not
//! `Instant`s: a span tree is serializable data, and dropping it into a log must not require a
//! conversion the reader cannot invert.
//!
//! # An unfinished span has no duration, and that is not zero
//!
//! [`Span::duration_ms`] returns `None` until the span ends. Returning `0` for "still running" would
//! be indistinguishable from a span that really took no measurable time, and the wrong reading is the
//! dangerous one: a tool that has been hung for a minute would appear in the log as instant. `None`
//! forces every consumer to decide what to do with a span that has not finished, and [`Span::render`]
//! prints it as `open` rather than as a number.
//!
//! # Children can outlast or exceed their parent
//!
//! Durations are measured, not attributed. If two tools run in parallel, each child reports its own
//! wall time and the two add up to more than the parent that contains them; a child that outlives its
//! parent widens [`Span::total_duration_ms`] past [`Span::duration_ms`]. Summing child durations to
//! check a parent total is therefore wrong for any tree that is not strictly sequential, and this
//! module never does it. The parent records when it started and ended; the children record the same
//! about themselves; reconciling the two is the reader's job, and [`Span::render`] shows the tree so
//! that reconciliation is possible.
//!
//! # Why this is not `session_stats`
//!
//! [`crate::session_stats`] counts turns, steps and tool calls from a session log, and has no
//! durations at all because [`crate::SessionEvent`] carries no timestamps. That is not a conflict with
//! this module, because the two read different sources: stats fold a *finished, persisted* log, while
//! a trace is collected *live*, in the process doing the work, from the clock the caller passes in.
//! Adding a span tree to the log would be an event-format change with a migration attached, and
//! `session_stats` says as much in its own documentation. Until then a trace is a runtime artifact —
//! logged, rendered, or dropped — and the counts stay derivable from the log alone.
//!
//! # Attributes are sorted
//!
//! [`Span::attributes`] is a [`BTreeMap`] rather than a `HashMap`. Two runs of the same request should
//! render byte-identical trees, and hash iteration order would make a log line differ between runs for
//! a reason no reader can see.

use std::collections::BTreeMap;
use std::fmt::Write as _;

/// One timed span in a request's trace tree.
///
/// A span is a name, a start, an end, the spans nested inside it, and labelled facts about it. The
/// fields are public because a span is data: a test builds one by hand to pin an exact shape, and a
/// consumer walks the tree without an accessor for every field.
///
/// See the module documentation for why timestamps are injected and why an unfinished span reports
/// `None` rather than a zero duration.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Span {
    /// The span's name, such as `"turn"`, `"step"`, or `"tool:bash"`.
    pub name: String,
    /// Start time in milliseconds on the caller's monotonic clock.
    pub started_ms: u64,
    /// End time on the same clock, or `None` while the span is still open.
    pub ended_ms: Option<u64>,
    /// Spans nested directly inside this one, in the order they were attached.
    pub children: Vec<Span>,
    /// Labelled facts about the span, kept sorted by key so rendering is deterministic.
    pub attributes: BTreeMap<String, String>,
}

impl Span {
    /// Starts a span that has not ended yet.
    ///
    /// `now_ms` comes from the caller so the span can be asserted exactly in a test; see the module
    /// documentation.
    #[must_use]
    pub fn start(name: impl Into<String>, now_ms: u64) -> Self {
        Self {
            name: name.into(),
            started_ms: now_ms,
            ended_ms: None,
            children: Vec::new(),
            attributes: BTreeMap::new(),
        }
    }

    /// Attaches an attribute, for building a span before any work runs.
    ///
    /// Consuming `self` keeps the common `Span::start(..).with_attribute(..)` shape a single
    /// expression. A span that is already running is labelled through [`Tracer::attribute`] instead,
    /// because there is no span value at hand to consume.
    #[must_use]
    pub fn with_attribute(mut self, key: impl Into<String>, value: impl Into<String>) -> Self {
        self.attributes.insert(key.into(), value.into());
        self
    }

    /// Ends the span at `now_ms`.
    ///
    /// The first call wins: a second `end` is ignored, so a caller that ends a span on both a success
    /// and an error path cannot overwrite the measurement that actually happened. Ending a span does
    /// not end its children — a child that is still open is a fact about the work, not about the API.
    pub fn end(&mut self, now_ms: u64) {
        if self.ended_ms.is_none() {
            self.ended_ms = Some(now_ms);
        }
    }

    /// Whether the span has ended.
    #[must_use]
    pub const fn is_ended(&self) -> bool {
        self.ended_ms.is_some()
    }

    /// How long this span took, or `None` while it is still open.
    ///
    /// `None` rather than `0` on purpose: zero would read as "finished instantly" for work that has
    /// not finished at all. A clock that appears to run backwards between the start and the end
    /// yields `0` rather than wrapping to an enormous number; the arithmetic is saturating so a
    /// mis-supplied clock cannot produce a duration that looks like years.
    #[must_use]
    pub fn duration_ms(&self) -> Option<u64> {
        self.ended_ms.map(|end| end.saturating_sub(self.started_ms))
    }

    /// The wall time the whole tree covers: from this span's start to the latest end anywhere in it.
    ///
    /// `None` until every span in the tree has ended, because a tree with a running span has no total
    /// yet. Unlike a sum of child durations, this is measured from instants, so parallel children do
    /// not inflate it — but a child that ends *after* its parent does widen it, and that is exactly
    /// the case a reader needs to see rather than have averaged away.
    ///
    /// The result is never smaller than [`Span::duration_ms`] when both are `Some`, and the two differ
    /// precisely when some descendant outlived this span.
    #[must_use]
    pub fn total_duration_ms(&self) -> Option<u64> {
        let (latest_end, all_ended) = self.coverage();
        if !all_ended {
            return None;
        }
        // `all_ended` guarantees a latest end exists; `saturating_sub` covers a clock that went
        // backwards without turning the result into an overflowed instant.
        latest_end.map(|end| end.saturating_sub(self.started_ms))
    }

    /// The latest end in this subtree, and whether every span in it has ended.
    ///
    /// Written as one traversal rather than two so a caller cannot pair the two facts from different
    /// walks of a tree that a concurrent writer might have changed in between.
    fn coverage(&self) -> (Option<u64>, bool) {
        let mut latest = self.ended_ms;
        let mut all_ended = self.ended_ms.is_some();

        for child in &self.children {
            let (child_end, child_ended) = child.coverage();
            all_ended &= child_ended;
            latest = match (latest, child_end) {
                (Some(a), Some(b)) => Some(a.max(b)),
                (left, right) => left.or(right),
            };
        }

        (latest, all_ended)
    }

    /// Renders this span and its descendants as indented text, one span per line.
    ///
    /// `indent` is the starting depth; it exists so a caller that is already inside a structure can
    /// nest the tree under its own line, and it is counted in levels rather than spaces. Each level is
    /// two spaces, which is the smallest difference that stays legible in a log.
    ///
    /// The line format is `name duration attributes`, where the duration is `12ms` for a finished span
    /// and `open` for one that has not ended, and attributes are `key=value` joined by spaces in key
    /// order. Every line ends with a newline, including the last, so trees concatenate without the
    /// caller inserting separators.
    #[must_use]
    pub fn render(&self, indent: usize) -> String {
        let mut out = String::new();
        self.render_into(&mut out, indent);
        out
    }

    /// Appends this span and its descendants to `out`.
    fn render_into(&self, out: &mut String, indent: usize) {
        let depth = "  ".repeat(indent);
        // Writing into a `String` cannot fail, but the result is still discarded rather than
        // unwrapped: production code here carries no `unwrap`, and a `let _` states the intent.
        let _ = write!(
            out,
            "{depth}{} {}",
            self.name,
            duration_label(self.duration_ms())
        );

        for (key, value) in &self.attributes {
            let _ = write!(out, " {key}={value}");
        }
        out.push('\n');

        for child in &self.children {
            child.render_into(out, indent + 1);
        }
    }
}

/// How a duration reads in [`Span::render`]: a number for a finished span, `open` for a running one.
///
/// Kept separate so the "no duration yet" case is written as a word rather than silently as `0`, which
/// is the mistake the whole module is arranged to prevent.
fn duration_label(duration: Option<u64>) -> String {
    match duration {
        Some(ms) => format!("{ms}ms"),
        None => "open".to_owned(),
    }
}

/// Rebuilds the open stack as one chain of spans, innermost deepest, for rendering.
///
/// The stack is stored innermost-last because that is what `end` needs to pop. Drawing needs the
/// opposite, so this walks outward from the deepest span and wraps each step in the one below it. The
/// result owns its spans — a render must not consume the tracer — and is dropped immediately after.
fn open_chain(open: &[Span]) -> Option<Span> {
    let mut chain: Option<Span> = None;

    for span in open.iter().rev() {
        let mut copy = span.clone();
        // The children recorded so far stay; only the not-yet-closed ones are rebuilt around it.
        if let Some(inner) = chain.take() {
            copy.children.push(inner);
        }
        chain = Some(copy);
    }

    chain
}

/// Builds a [`Span`] tree for one request as its work runs.
///
/// The tracer keeps the spans that are currently open on a stack, because work nests: `begin("turn")`
/// then `begin("step")` then `begin("tool:bash")` makes the tool a child of the step. [`Tracer::end`]
/// closes the innermost open span and attaches it to whatever is still open, which means a caller never
/// has to hold a `Span` value while waiting for a tool to return.
///
/// ```text
/// let mut tracer = Tracer::new();
/// tracer.begin("turn", 0);
/// tracer.begin("tool:bash", 10);
/// tracer.end(90);
/// tracer.end(100);
///
/// assert_eq!(tracer.open_depth(), 0);
/// assert_eq!(tracer.roots()[0].duration_ms(), Some(100));
/// assert_eq!(tracer.roots()[0].children[0].name, "tool:bash");
/// ```
#[derive(Clone, Debug, Default)]
pub struct Tracer {
    /// Finished root spans, in the order they closed.
    roots: Vec<Span>,
    /// Spans that have begun and not yet ended, innermost last.
    open: Vec<Span>,
}

impl Tracer {
    /// Creates an empty tracer.
    #[must_use]
    pub const fn new() -> Self {
        Self {
            roots: Vec::new(),
            open: Vec::new(),
        }
    }

    /// Begins a span nested inside the innermost open one, or as a new root when none is open.
    pub fn begin(&mut self, name: impl Into<String>, now_ms: u64) {
        self.open.push(Span::start(name, now_ms));
    }

    /// Begins a span and labels it in one call.
    pub fn begin_with(
        &mut self,
        name: impl Into<String>,
        now_ms: u64,
        attributes: impl IntoIterator<Item = (String, String)>,
    ) {
        let mut span = Span::start(name, now_ms);
        span.attributes.extend(attributes);
        self.open.push(span);
    }

    /// Ends the innermost open span at `now_ms`, attaching it to its parent.
    ///
    /// Returns whether a span was closed. `false` means the stack was empty, which is a caller bug
    /// rather than a reason to panic: a tracer used across an `await` that is cancelled can
    /// legitimately see one unmatched `end`, and losing that one span is better than losing the
    /// request. Nothing is recorded in that case, because there is no name to record it under.
    pub fn end(&mut self, now_ms: u64) -> bool {
        let Some(mut span) = self.open.pop() else {
            return false;
        };
        span.end(now_ms);

        match self.open.last_mut() {
            Some(parent) => parent.children.push(span),
            None => self.roots.push(span),
        }
        true
    }

    /// Labels the innermost open span, returning whether one was open to label.
    ///
    /// A later call with the same key replaces the earlier value: an attribute is the latest fact
    /// known about a span, and keeping both would render two lines that contradict each other.
    pub fn attribute(&mut self, key: impl Into<String>, value: impl Into<String>) -> bool {
        let Some(span) = self.open.last_mut() else {
            return false;
        };
        span.attributes.insert(key.into(), value.into());
        true
    }

    /// How many spans are open right now.
    ///
    /// Exposed so a caller can assert that every `begin` found its `end`; a trace with a non-zero depth
    /// at the end of a request is incomplete, and the number says how much.
    #[must_use]
    pub fn open_depth(&self) -> usize {
        self.open.len()
    }

    /// The root spans recorded so far, in the order they closed.
    #[must_use]
    pub fn roots(&self) -> &[Span] {
        &self.roots
    }

    /// Consumes the tracer and returns its root spans.
    ///
    /// Spans still open are not discarded: each becomes a root with `ended_ms` left `None`, so an
    /// abandoned trace still shows what was running when it was abandoned. Discarding them would turn
    /// the most interesting case — the request that never finished — into an empty trace.
    ///
    /// The stack is unwound from the inside out so nesting survives: each open span is attached to the
    /// one opened before it, which rebuilds the same tree the matching `end` calls would have built.
    /// Only the outermost one becomes a root, and it stays open because no end time is known.
    #[must_use]
    pub fn into_roots(mut self) -> Vec<Span> {
        while let Some(span) = self.open.pop() {
            match self.open.last_mut() {
                Some(parent) => parent.children.push(span),
                None => self.roots.push(span),
            }
        }
        self.roots
    }

    /// Renders the trace: finished roots first, then whatever is still running.
    ///
    /// Open spans are included rather than omitted. A trace is most useful *while* the request is
    /// running — that is when someone asks "what is it stuck on" — and a render that showed only
    /// finished work would print an empty string at exactly that moment. A running span reads `open`
    /// instead of a duration, so the reader can tell the two apart without consulting the code.
    ///
    /// A finished root never has an open child: `end` closes the innermost span, so a span can only
    /// become a root when nothing is above it. The open stack is therefore rendered as its own chain
    /// after the roots, and the two groups never interleave.
    #[must_use]
    pub fn render(&self, indent: usize) -> String {
        let mut out = String::new();
        for root in &self.roots {
            root.render_into(&mut out, indent);
        }
        if let Some(chain) = open_chain(&self.open) {
            chain.render_into(&mut out, indent);
        }
        out
    }

    /// The number of spans in the trace, counting the ones still open.
    #[must_use]
    pub fn span_count(&self) -> usize {
        fn count(span: &Span) -> usize {
            1 + span.children.iter().map(count).sum::<usize>()
        }
        self.roots.iter().map(count).sum::<usize>() + self.open.len()
    }
}
