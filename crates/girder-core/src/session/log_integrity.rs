//! Telling an empty session log apart from a damaged one.
//!
//! # The failure this module exists to prevent
//!
//! `DirectoryCatalog::read_record` reads a log with
//! `read_session_file(path).unwrap_or_default()`. A log whose last write a crash cut off therefore
//! does not surface as an error: the read yields a default record, so the report carries
//! `events: []` and `blank: true`, and `session.state` answers `events: 0` *successfully*. A
//! session that was created and never used and a session whose log is torn are then
//! indistinguishable, and the answer given about the second one is not merely incomplete — it is
//! wrong, and delivered with the same confidence as a correct one. A caller cannot fix that
//! afterwards, because the difference was destroyed at read time.
//!
//! # The rule
//!
//! Only zero bytes means empty. Not `events: 0`, not `blank: true`, and not a failed read: a failed
//! read with bytes on disk is *damage*, and damage and emptiness are different facts with different
//! remedies. Everything below follows from that one rule, which is why the emptiness question lives
//! in [`Integrity::is_empty`] and is answered from the byte length alone.
//!
//! | verdict      | `is_empty` | `is_readable` | `needs_repair` | meaning                                  |
//! |--------------|------------|---------------|----------------|------------------------------------------|
//! | `Empty`      | `true`     | `false`       | `false`        | zero bytes; legal, nothing was written    |
//! | `Readable`   | `false`    | `true`        | `false`        | content present and decodable             |
//! | `TornTail`   | `false`    | `true`        | `true`         | ends mid-record; truncation fixes it      |
//! | `Broken`     | `false`    | `false`       | `false`        | damage inside the log; truncation is unsafe |
//!
//! # Why a torn tail still counts as readable
//!
//! The tear cut a write that never completed, so the record it was writing never existed as a
//! whole: everything the session ever committed is in the bytes before the tear. A reader that
//! stops at the tear has seen the entire conversation, which is a different situation from one that
//! is missing a record which does exist and cannot be reached. [`Integrity::is_readable`] answers
//! the first question — is the content there — not the second, and the readers in this crate
//! currently fail on the tear rather than stopping at it, which is why [`Integrity::needs_repair`]
//! exists beside it.
//!
//! # Why `Broken` is never repaired
//!
//! The only repair this crate has is truncation, and the only safe truncation point is a tear at the
//! end of the file. Damage that sits inside the log has readable events *after* it; truncating there
//! would delete bytes the user still has, silently and permanently. So `Broken` reports
//! `needs_repair == false`: it is not a request to run the repair step, it is a request for a human
//! decision, and [`Integrity::describe`] says so in words a user can act on.
//!
//! # How the damage is located
//!
//! [`inspect`] judges the log as a record stream — the bytes a decoder produces, one record per
//! line, terminated by `\n`. Frame-level surgery on compressed logs is a separate concern (see
//! `crate::session::recovery`); this module answers the policy question that follows it: given the
//! bytes and what the reader made of them, is the log empty, whole, torn or broken?
//!
//! The distinction between the last two is a property of the *end* of the stream, and it is
//! decidable without parsing any record: an interrupted append always leaves the file ending
//! *inside* a record, because the write that would have emitted the terminator never finished. A
//! stream that ends exactly on a record boundary and is still refused is therefore not a torn
//! append — something corrupted records that were completed, which is the middle-damage shape.

/// The byte that ends one record and begins the next.
///
/// The log format separates records by line rather than by length, which is what makes "does this
/// stream end inside a record?" answerable from the bytes alone, without a parser.
const RECORD_TERMINATOR: u8 = b'\n';

/// What an inspection concluded about one log.
///
/// The four variants are ordered by how much a caller may trust them; they are values rather than a
/// score because each one has a different *remedy*, and a number would let a caller average two of
/// them into a lossy third.
#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(tag = "kind", rename_all = "camelCase")]
pub enum Integrity {
    /// No verdict was reached, which happens when a record was built without reading the log.
    ///
    /// The default is deliberately *not* `Empty`: a missing verdict is not a claim that nothing was written.
    Unread {
        /// How many bytes the file holds, which is all a caller that did not decode it can say.
        bytes: u64,
    },

    /// Empty log (zero bytes) — legal.
    ///
    /// A session created and never used looks exactly like this, and that is not a defect. It is
    /// also the *only* state that earns this verdict: a log with bytes in it is never empty, however
    /// little a reader could make of them.
    Empty,

    /// Content present and readable.
    Readable {
        /// How many records the reader recovered.
        events: usize,
    },

    /// The tail is torn (repairable).
    ///
    /// An interrupted append left a prefix of the final record. Every record before it is intact,
    /// so truncating at `at_byte` repairs the log without dropping a single readable event.
    TornTail {
        /// How many records are readable before the tear.
        readable: usize,

        /// The byte offset at which the unfinished record begins — the exact cut point.
        at_byte: u64,
    },

    /// Damage inside the log (not safely repairable).
    ///
    /// Readable events may sit *after* the damage, so the truncation that fixes a
    /// [`Integrity::TornTail`] would delete them here. Refusing to act is the only choice that
    /// cannot lose data.
    Broken {
        /// How many records are readable before the damage.
        readable: usize,

        /// The byte offset from which the module can no longer vouch for the records.
        ///
        /// A stream that ends cleanly proves only that its *final* complete record is unusable, so
        /// that is the offset named here; corruption that began earlier would be visible as fewer
        /// readable records, and `detail` carries the reader's own account of it.
        at_byte: u64,

        /// What the reader reported, in the form an operator needs to decide what to do next.
        detail: String,
    },
}

impl Integrity {
    /// Whether a client can read the session's content.
    ///
    /// True for [`Integrity::Readable`] and for [`Integrity::TornTail`]: a tear only ever destroys
    /// the write that was in flight, so all committed content is still there.
    ///
    /// False for [`Integrity::Broken`], where events exist that a plain reader cannot reach, and for
    /// [`Integrity::Empty`], which holds no content at all — a zero-byte file is readable *as a
    /// file*, but that is what [`Integrity::is_empty`] reports, and conflating the two is the exact
    /// mistake this module exists to stop.
    pub fn is_readable(&self) -> bool {
        matches!(self, Self::Readable { .. } | Self::TornTail { .. })
    }

    /// Whether the log needs repairing.
    ///
    /// True only for [`Integrity::TornTail`], where truncation at the reported offset restores the
    /// log and loses nothing.
    ///
    /// [`Integrity::Broken`] answers `false` deliberately: the repair step available here truncates,
    /// and truncating a log whose damage is in the middle deletes readable events after it. The log
    /// needs a decision, not a repair, and reporting `true` here would invite the destructive one.
    pub fn needs_repair(&self) -> bool {
        matches!(self, Self::TornTail { .. })
    }

    /// Whether the log is empty — **zero bytes is the only thing that is**.
    ///
    /// This is the question the rest of the crate used to answer with `events == 0 ||
    /// blank`, which is also true of a torn log and of a log no reader could decode. Emptiness is
    /// evidence about what was *written*, so it is decided from the bytes, and the answer cannot be
    /// changed by a reader failing afterwards.
    pub fn is_empty(&self) -> bool {
        matches!(self, Self::Empty)
    }

    /// One sentence for a person, with the numbers they need to act.
    ///
    /// The wording is user-facing rather than diagnostic: every variant says both what was found and
    /// what it means for the conversation, because a refusal a user cannot interpret is
    /// indistinguishable from a bug.
    pub fn describe(&self) -> String {
        match self {
            Self::Empty => {
                "the log is empty (0 bytes): nothing was ever written, so nothing is lost"
                    .to_owned()
            }
            Self::Unread { bytes } => format!(
                "the log has {bytes} byte(s), which this view did not read; whether it is intact is \
                 unknown until something reads it"
            ),
            Self::Readable { events } => {
                format!("the log is intact: {events} event(s) readable, no repair needed")
            }
            Self::TornTail { readable, at_byte } => format!(
                "the log is torn at byte {at_byte}: {readable} event(s) before it are readable and \
                 only an unfinished write is lost, so truncating there loses nothing"
            ),
            Self::Broken {
                readable,
                at_byte,
                detail,
            } => format!(
                "the log is damaged at byte {at_byte}, not only at its end: {readable} event(s) \
                 before it are readable, events after it are unreachable, and truncating would \
                 delete them ({detail})"
            ),
        }
    }
}

/// Judges one already-read byte sequence.
///
/// `bytes` is what was read from disk and `outcome` is what the reader made of it: `Ok(events)`
/// means the sequence decoded into `events` records, `Err(detail)` means it did not, with `detail`
/// carrying the reader's account.
///
/// The order of the checks is the whole point of the function. Emptiness is decided **first**, from
/// the byte length alone, so a zero-byte log cannot be reclassified by a failing reader. Only then
/// does a failure become evidence of damage — and a failure on a non-empty sequence is always
/// damage, never emptiness.
pub fn inspect(bytes: &[u8], outcome: Result<usize, &str>) -> Integrity {
    // Zero bytes is the only evidence of a log nothing was ever written to. A reader that fails on
    // such a file has lost nothing, so the outcome is deliberately not consulted: letting it decide
    // would let a broken reader manufacture damage out of absent content.
    if bytes.is_empty() {
        return Integrity::Empty;
    }
    match outcome {
        Ok(events) => Integrity::Readable { events },
        Err(detail) => locate(bytes, detail),
    }
}

/// Decides which damage shape a refused, non-empty log shows.
///
/// The stream is walked once, at record boundaries only, to answer a single question: does it end
/// inside a record? An interrupted append always does, because the terminator of the record being
/// written was never reached; a stream that ends on a boundary was not cut short by a killed write,
/// so its damage lives in records that were completed.
///
/// Nothing here decodes a record. That is deliberate — a parser would have to guess at a schema this
/// module does not own, and the shape of the damage does not depend on the content.
fn locate(bytes: &[u8], detail: &str) -> Integrity {
    let mut readable = 0usize;
    // Where the record currently being read began. Starts at 0 because the first record does.
    let mut record_start = 0usize;
    // Where the last *complete* record began; only meaningful once `readable > 0`.
    let mut last_complete = 0usize;

    for (index, byte) in bytes.iter().enumerate() {
        if *byte == RECORD_TERMINATOR {
            readable += 1;
            last_complete = record_start;
            record_start = index + 1;
        }
    }

    if record_start < bytes.len() {
        // The stream ends mid-record: this is the shape a killed append leaves, and cutting at the
        // start of the unfinished record drops no completed record.
        return Integrity::TornTail {
            readable,
            at_byte: record_start as u64,
        };
    }

    // The stream ends on a record boundary, so the final record was written in full and the refusal
    // is not a tear. The module can prove no more than that the last complete record is unusable,
    // so that record — and only it — is what it reports as beyond saving.
    Integrity::Broken {
        readable: readable.saturating_sub(1),
        at_byte: last_complete as u64,
        detail: detail.to_owned(),
    }
}

impl Default for Integrity {
    /// A missing verdict, not a claim that the log is empty.
    ///
    /// The two are different: `Empty` says a file was opened and had nothing in it, while this says nobody opened one. A
    /// record built without reading its log must not report the first.
    fn default() -> Self {
        Self::Unread { bytes: 0 }
    }
}
