//! Who wrote the conversation, measured in characters.
//!
//! # What this adds
//!
//! [`MessageSource`] already answers "who produced *this* message" for every message in a log, and
//! that per-message answer is the ground truth here. What the model cannot ask of a per-message
//! field is the aggregate question: *how much of the context is mine?* This module is only that
//! aggregation — it reads the existing sources and sums their characters. It deliberately does not
//! introduce a second provenance vocabulary.
//!
//! # Why characters rather than messages
//!
//! Counting messages makes a conversation look balanced when it is not: one prompt of five
//! characters and one reply of five thousand are two messages, so a message count reports 1:1 and a
//! share of 0.5. The quantity that matters for context budget, cost, and "who is dominating this
//! session" is volume, so every count here is `chars().count()`.
//!
//! Characters, not bytes: a byte count would report a CJK conversation as three to four times
//! larger than the same text in ASCII, which says something about the encoding rather than about
//! the author.
//!
//! # Why the source, not the role
//!
//! [`Role`] is not a producer. A `user`-role message may be the person, another agent relaying, or
//! a plugin injecting context, and `message.rs` documents that this ambiguity is the reason source
//! exists as a second axis. Attributing by role would therefore credit the model's own text to the
//! person whenever a producer reuses the user turn. So the bucket comes from [`MessageSource`] and
//! the role is ignored.
//!
//! # Why unknowns are counted, not defaulted
//!
//! A relayed message is written by another agent — neither the person nor this model. Folding it
//! into `user` would overstate human authorship; folding it into `model` would overstate the
//! model's. [`Attribution::unknown`] keeps it visible instead, so the four named buckets stay
//! honest and a surprising share is traceable to its cause. A producer added later must pick a
//! bucket at compile time, because the match has no wildcard.

use crate::llm::ContentBlock;
use crate::message::{Message, MessageSource};

/// How many characters each producer contributed to a conversation.
///
/// Every field is a character count, never a message count: see the module documentation for why.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Attribution {
    /// Characters the person wrote.
    pub user: usize,
    /// Characters the model wrote, including reasoning and tool-call arguments it authored.
    pub model: usize,
    /// Characters a tool returned.
    pub tool: usize,
    /// Characters the system prompt contributed.
    pub system: usize,
    /// Characters from a producer this type does not classify, such as a relayed agent message.
    ///
    /// Kept separate rather than merged into another bucket: a bucket that silently absorbs
    /// unknown producers cannot be audited.
    pub unknown: usize,
}

impl Attribution {
    /// Counts the characters of every message, bucketed by its source.
    ///
    /// Attachments contribute nothing: an image or file block carries a handle to bytes held
    /// elsewhere, so counting the handle would measure the harness's bookkeeping instead of what
    /// the producer wrote.
    #[must_use]
    pub fn of(messages: &[Message]) -> Self {
        let mut attribution = Self::default();
        for message in messages {
            let chars: usize = message.content.iter().map(block_chars).sum();
            match &message.source {
                MessageSource::User { .. } => attribution.user += chars,
                MessageSource::Model { .. } => attribution.model += chars,
                MessageSource::Tool { .. } => attribution.tool += chars,
                MessageSource::SystemPrompt => attribution.system += chars,
                // No wildcard arm: a new producer has to decide its bucket here rather than be
                // quietly attributed to whoever writes the catch-all.
                MessageSource::Relay { .. } => attribution.unknown += chars,
            }
        }
        attribution
    }

    /// The total number of characters, including [`Self::unknown`].
    ///
    /// The unknown bucket is included so that the shares are fractions of everything that was
    /// counted, and therefore sum to one.
    #[must_use]
    pub fn total(&self) -> usize {
        self.user + self.model + self.tool + self.system + self.unknown
    }

    /// The model's share of the total, in `0.0..=1.0`.
    ///
    /// An empty conversation yields `0.0` rather than `NaN`: the model wrote nothing, and `NaN`
    /// would propagate through comparisons, sorting, and JSON encoding as a non-number that every
    /// caller then has to guard against.
    #[must_use]
    #[allow(clippy::cast_precision_loss)] // A share is a display ratio; character counts above 2^53 cannot be held in memory.
    pub fn model_share(&self) -> f64 {
        let total = self.total();
        if total == 0 {
            return 0.0;
        }
        self.model as f64 / total as f64
    }
}

/// The characters one block contributes to its message's producer.
///
/// Reasoning counts because the model wrote it, even though the user does not see it. Tool-call
/// arguments count for the same reason: they are the model's own generated text, recorded raw.
fn block_chars(block: &ContentBlock) -> usize {
    match block {
        ContentBlock::Text { text } | ContentBlock::Reasoning { text } => text.chars().count(),
        ContentBlock::ToolCall { arguments, .. } => arguments.chars().count(),
        // The bytes live in the attachment service, not in the log, so the reference is not prose.
        ContentBlock::Image { .. } | ContentBlock::File { .. } => 0,
    }
}
