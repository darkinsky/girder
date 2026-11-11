//! A reusable compaction policy: when to compact, how much to keep, and when to refuse.
//!
//! `compaction` owns the arithmetic of *where* a surface may be cut. This module is the layer above
//! it: a value a caller holds, applies to a message list, and gets a decision from. The split is
//! deliberate — "is this cut legal" is a property of the history and belongs with the history,
//! while "should we compact at all, and how much should survive" is a product decision that differs
//! between a CLI, an editor, and a server.
//!
//! # Why the policy works on messages, and not on surface entries
//!
//! The mapping between the two is not one-to-one, in either direction:
//!
//! - One assistant `Message` carries every tool call its response emitted, while the session log
//!   also records each call as its own `tool/call` event. So one surface entry can hold many calls,
//!   and the same call appears twice.
//! - A `tool/result` event has no `Message` of its own. The message list holds the result as a
//!   `role: Tool` message whose `source` names the call it answers, and that is the shape a provider
//!   validates.
//!
//! `balances()` therefore cannot be applied to a message list as it stands, and this module derives
//! the pairing from the messages themselves. The rule is the same one, deliberately: calls open,
//! results close, and a cut is legal only where nothing is left open.
//!
//! # The invariant, and the two ways to honour it
//!
//! A history where a `ToolCall` survives without its result — or a result without its call — is
//! rejected by every provider, so neither mode of [`CompactionPolicy`] can produce one. The modes
//! differ only in which side gives way when a tool pair straddles the keep floor: with
//! `require_pairing` the floor is a minimum and the pair is dragged into the kept suffix, without it
//! the floor is a maximum and the pair is dropped whole. Refusing instead of choosing is never wrong,
//! but always losing the most recent exchange is, which is why the default keeps the pair.

use std::collections::BTreeSet;

use girder_core::{ChatMessage, ChatRole, ContentBlock, ToolCallId};

/// The message count at which the default policy compacts.
///
/// A count rather than a token estimate because this layer sees no tokenizer: forty messages is a
/// long conversation by any measure, and a caller that knows its window should set a byte trigger.
pub const DEFAULT_TRIGGER_MESSAGES: usize = 40;

/// How many recent messages the default policy keeps.
///
/// Matches `CompactionRequest::default`, so a caller that switches layers does not silently change
/// how much recent context survives.
pub const DEFAULT_KEEP_RECENT: usize = 8;

/// What a policy decided to do with a history.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Plan {
    /// Compaction is not warranted, so every message stays.
    KeepAll,
    /// The messages to keep, and the contiguous span a summary stands in for.
    ///
    /// `keep` and `drop` partition the input: together they cover every index once, each is in
    /// ascending order, and `keep` holds the pinned prefix followed by the recent suffix. A caller
    /// that renders the surviving history reads it in `keep` order.
    Compact {
        /// Indices of the messages that survive.
        keep: Vec<usize>,
        /// Indices of the messages the summary replaces.
        drop: Vec<usize>,
        /// Why this span, in words a person can read in a log line.
        reason: String,
    },
    /// Compaction is warranted but no safe plan exists, so nothing should be attempted.
    Refuse {
        /// Why refusing is the only safe answer.
        reason: String,
    },
}

impl Plan {
    /// Whether this plan actually compacts anything.
    #[must_use]
    pub fn is_compaction(&self) -> bool {
        matches!(self, Self::Compact { .. })
    }

    /// The explanation attached to a [`Plan::Compact`] or [`Plan::Refuse`].
    ///
    /// `None` for [`Plan::KeepAll`]: there is no decision to explain, and an empty string would be
    /// indistinguishable from a reason that was forgotten.
    #[must_use]
    pub fn reason(&self) -> Option<&str> {
        match self {
            Self::KeepAll => None,
            Self::Compact { reason, .. } | Self::Refuse { reason } => Some(reason),
        }
    }
}

/// The rules a caller applies to a history.
///
/// Fields are public because a policy is configuration, not behaviour: a caller builds one from its
/// own settings, and a wrapper type with a setter per field would only add noise.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CompactionPolicy {
    /// Compact once the history holds at least this many messages.
    ///
    /// Zero compacts on every call, which is a legal but pointless configuration: with a non-zero
    /// `keep_recent` nothing can be dropped from a short history, and the plan says so.
    pub trigger_messages: usize,
    /// Compact once the history's measured cost reaches this many bytes.
    ///
    /// `None` disables the size trigger. Byte cost is an approximation of the window a history
    /// occupies — this layer has no tokenizer — and it exists because a few enormous tool results
    /// can fill a window while the message count stays low. Either trigger firing starts a
    /// compaction.
    pub trigger_bytes: Option<usize>,
    /// How many of the most recently added messages must survive.
    ///
    /// A floor rather than a target when `require_pairing` is set, and a ceiling when it is not; see
    /// the module documentation. A tool pair that has to move to honour it moves the cut by as many
    /// messages as the pair spans, never by dropping half of it.
    pub keep_recent: usize,
    /// Whether tool pairing, rather than the keep floor, decides a contested cut.
    ///
    /// With `true`, a pair straddling the floor is kept whole and the kept suffix grows past
    /// `keep_recent`. With `false`, the pair is dropped whole and the suffix shrinks below it. The
    /// pairing invariant itself holds in both modes: a call never survives without its result.
    pub require_pairing: bool,
}

impl CompactionPolicy {
    /// Builds a policy that responds to message count only.
    #[must_use]
    pub const fn new(trigger_messages: usize, keep_recent: usize) -> Self {
        Self {
            trigger_messages,
            trigger_bytes: None,
            keep_recent,
            require_pairing: true,
        }
    }

    /// Decides what to do with `messages`.
    ///
    /// The decision never depends on where the caller happens to be in a turn: it reads the whole
    /// history and answers for it, so two calls with the same input agree.
    #[must_use]
    pub fn plan(&self, messages: &[ChatMessage]) -> Plan {
        if messages.is_empty() {
            return Plan::KeepAll;
        }

        let len = messages.len();
        let Some(trigger) = self.trigger(len, history_cost(messages)) else {
            return Plan::KeepAll;
        };

        if self.keep_recent == 0 {
            return Plan::Refuse {
                reason: "the policy keeps no recent messages, so compaction would summarize the \
                         entire history"
                    .to_owned(),
            };
        }
        if len <= self.keep_recent {
            return Plan::KeepAll;
        }

        // The system prompt is not conversation: summarizing it away replaces the instructions the
        // model is supposed to follow with a description of them, which is strictly worse.
        let pinned = pinned_prefix(messages);
        if pinned >= len {
            return Plan::Refuse {
                reason: format!(
                    "all {len} messages belong to the pinned system prefix, so there is nothing to \
                     summarize"
                ),
            };
        }

        let floor_cut = len - self.keep_recent;
        let earliest_cut = pinned + 1;
        if floor_cut < earliest_cut {
            return Plan::Refuse {
                reason: format!(
                    "the {pinned}-message system prefix and the {}-message keep floor leave no \
                     messages between them to summarize",
                    self.keep_recent
                ),
            };
        }

        let pairing = scan_pairing(messages);
        // A cut is legal exactly where no call is still open: that is the message-list reading of
        // `pairing_balanced_at`, which works on surface entries.
        let balanced = |cut: usize| {
            pairing
                .open_after
                .get(cut - 1)
                .is_none_or(|open| *open == 0)
        };
        let cut = if self.require_pairing {
            // Keeping more than the floor is legal; keeping fewer is not, so search downwards.
            (earliest_cut..=floor_cut)
                .rev()
                .find(|cut| balanced(*cut))
        } else {
            // Keeping fewer than the floor is legal; keeping more is not, so search upwards.
            (floor_cut..len).find(|cut| balanced(*cut))
        };
        let Some(cut) = cut else {
            return Plan::Refuse {
                reason: self.refusal(messages, floor_cut),
            };
        };

        // A result whose call is no longer in the history is rejected by every provider. Dropping
        // such a message repairs the history; keeping it would hand the provider a request it is
        // known to refuse, so the whole plan is refused instead.
        if let Some(index) = pairing
            .orphans
            .iter()
            .copied()
            .find(|index| *index < pinned || *index >= cut)
        {
            return Plan::Refuse {
                reason: format!(
                    "message {index} is a tool result whose call is not in the kept history; \
                     compaction cannot repair a request a provider would already reject"
                ),
            };
        }

        let recent = len - cut;
        let prefix_note = if pinned == 0 {
            String::new()
        } else {
            format!(" plus the {pinned}-message system prefix")
        };
        let note = if cut == floor_cut {
            String::new()
        } else if self.require_pairing {
            let earlier = floor_cut - cut;
            format!(
                "; the cut moved {earlier} message(s) earlier than the floor to keep a straddling \
                 tool pair whole"
            )
        } else {
            let later = cut - floor_cut;
            format!(
                "; the cut moved {later} message(s) later than the floor so the straddling tool \
                 pair is dropped whole"
            )
        };

        Plan::Compact {
            keep: (0..pinned).chain(cut..len).collect(),
            drop: (pinned..cut).collect(),
            reason: format!(
                "{trigger}; dropping messages {pinned}..{cut} keeps the last {recent}\
                 {prefix_note}{note}"
            ),
        }
    }

    /// Why no cut was found, in the vocabulary of the mode that failed.
    fn refusal(&self, messages: &[ChatMessage], floor_cut: usize) -> String {
        if !self.require_pairing {
            return "dropping a tool pair that straddles the cut would leave no message to keep"
                .to_owned();
        }
        // Naming the call makes the refusal actionable: the caller can look at that exchange and
        // decide whether to finish it or to compact by hand.
        let open = open_ids_after(messages, floor_cut);
        let named = open.iter().next().map_or_else(String::new, |id| {
            format!("; the call `{}` is still open there", id.as_str())
        });
        format!(
            "keeping the last {} messages would split a tool call from its result, and keeping \
             more would leave nothing to summarize{named}",
            self.keep_recent
        )
    }

    /// The trigger that fired, described for the plan's reason.
    ///
    /// It returns the description rather than a boolean because every caller that asks also wants to
    /// say why, and deriving the explanation twice is how the two drift apart.
    fn trigger(&self, len: usize, cost: usize) -> Option<String> {
        if len >= self.trigger_messages {
            return Some(format!(
                "{len} messages reached the {} message trigger",
                self.trigger_messages
            ));
        }
        self.trigger_bytes
            .filter(|limit| cost >= *limit)
            .map(|limit| format!("{cost} bytes reached the {limit} byte trigger"))
    }
}

impl Default for CompactionPolicy {
    fn default() -> Self {
        Self {
            trigger_messages: DEFAULT_TRIGGER_MESSAGES,
            trigger_bytes: None,
            keep_recent: DEFAULT_KEEP_RECENT,
            require_pairing: true,
        }
    }
}

/// The model-visible size of one message, in bytes.
///
/// Bytes rather than tokens because no tokenizer is available here, and a caller comparing this
/// against a window should apply its own margin. Tool arguments are counted: they are the part of a
/// call the model wrote and the part a window actually pays for.
#[must_use]
pub fn message_cost(message: &ChatMessage) -> usize {
    message
        .content
        .iter()
        .map(|block| match block {
            ContentBlock::Text { text } | ContentBlock::Reasoning { text } => text.len(),
            ContentBlock::ToolCall {
                name, arguments, ..
            } => name.len() + arguments.len(),
            ContentBlock::Image { attachment } | ContentBlock::File { attachment } => {
                attachment.len()
            }
        })
        .sum()
}

/// The model-visible size of a history, in bytes.
#[must_use]
pub fn history_cost(messages: &[ChatMessage]) -> usize {
    messages.iter().map(message_cost).sum()
}

/// The tool-pairing state of a message list, and the defects already in it.
struct Pairing {
    /// How many calls are open after each message, in order.
    open_after: Vec<usize>,
    /// Indices of results that answer a call the history never opened.
    orphans: Vec<usize>,
}

/// Folds the pairing rule over a message list.
///
/// The same rule as `balances()`: a count of open calls is enough, because a legal cut requires
/// zero. A result for an unknown call removes nothing rather than going negative, which is what
/// keeps a broken history from looking like a balanced one.
fn scan_pairing(messages: &[ChatMessage]) -> Pairing {
    let mut open = BTreeSet::new();
    let mut open_after = Vec::with_capacity(messages.len());
    let mut orphans = Vec::new();

    for (index, message) in messages.iter().enumerate() {
        if apply_pairing(message, &mut open) {
            orphans.push(index);
        }
        open_after.push(open.len());
    }

    Pairing { open_after, orphans }
}

/// Applies one message to the open-call set, reporting whether it answered nothing.
///
/// Results are applied before calls: a message is either an answer or a question in practice, and
/// answering first keeps a hypothetical message that does both from closing itself.
fn apply_pairing(message: &ChatMessage, open: &mut BTreeSet<ToolCallId>) -> bool {
    let mut orphan = false;
    if let Some(id) = message.source.call_id() {
        orphan = !open.remove(id);
    }
    for block in &message.content {
        if let ContentBlock::ToolCall { id, .. } = block {
            open.insert(id.clone());
        }
    }
    orphan
}

/// The calls still open after the first `cut` messages.
fn open_ids_after(messages: &[ChatMessage], cut: usize) -> BTreeSet<ToolCallId> {
    let mut open = BTreeSet::new();
    for message in messages.iter().take(cut) {
        apply_pairing(message, &mut open);
    }
    open
}

/// How many leading system messages are exempt from compaction.
///
/// A message that carries a tool call cannot be pinned: pinning it would drag a call into the kept
/// prefix whose result is being summarized, which is exactly the split this module exists to prevent.
fn pinned_prefix(messages: &[ChatMessage]) -> usize {
    messages
        .iter()
        .take_while(|message| message.role == ChatRole::System && !opens_call(message))
        .count()
}

/// Whether a message asks for a tool.
fn opens_call(message: &ChatMessage) -> bool {
    message
        .content
        .iter()
        .any(|block| matches!(block, ContentBlock::ToolCall { .. }))
}
