//! A character budget for the content blocks of one tool result.
//!
//! Two mechanisms already bound tool output, and both price the wrong thing.
//! [`tool_result_pruning`](crate::tool_result_pruning) bounds one *text* against a character limit,
//! and [`spill`](crate::spill) replaces one oversized result with a locator. A tool result is
//! neither one text nor one thing: it is a list of content blocks that may mix prose, an image and
//! a file handle, and the model pays for every one of them. This module is the accounting for that
//! list. Blocks are offered one at a time under a single budget, and every offer produces an
//! [`Outcome`] so the caller can tell a block that arrived whole from one that arrived in part and
//! one that did not arrive at all.
//!
//! # An image is not free
//!
//! A budget that measures only text is not a budget. An image block carries an opaque reference of
//! a few dozen characters, but the model receives the picture, and a provider charges visual tokens
//! for it — an image is worth several hundred of them, not the length of its name. Charging the
//! reference would let a result holding a dozen screenshots look like it fits in two kilobytes, and
//! the next request would be refused for exceeding the window. A non-text block is therefore
//! charged [`ATTACHMENT_COST_CHARS`].
//!
//! That number is an **estimate**, and it is the only honest one available at this layer: the
//! bytes live in the attachment service and never reach here, so neither an image's resolution nor
//! a route's per-image pricing is knowable. A fixed charge is still a large improvement over the
//! reference-length charge, because it scales with the *number* of attachments — the part that
//! fills a window — rather than with the length of a name. A caller that knows its route's image
//! pricing should budget for images explicitly instead of trusting this figure; see
//! [`crate::token_meter`], which documents the same blind spot from the token side.
//!
//! # A cut lands between characters, never inside one
//!
//! The budget is counted in characters, so cuts are made in characters. A byte-based cut would
//! split a multi-byte code point and leave the model a replacement character in the middle of a
//! word — corruption that reads as tool failure rather than as truncation. The retained prefix is
//! taken at a `char` boundary, so a view of Chinese prose or emoji is shortened, never mangled.
//!
//! # A shortened view says it is shortened
//!
//! Silence is what makes truncation dangerous: a model that receives a shortened result without a
//! marker cannot tell "the tool printed these lines" from "the tool printed these lines and a
//! thousand more", and answers confidently about output it never saw. Every cut part is followed by
//! a marker naming how much survived, and a view that dropped whole blocks ends with a notice
//! naming how many. The notices are added *outside* the budget on purpose: a notice that had to
//! compete for room would be the first thing dropped, which is precisely how a shortened result
//! comes to look complete. The overshoot is bounded by one line per shortened block.
//!
//! # What this does not own
//!
//! It does not choose which block matters — a function that sees blocks cannot know whether the
//! screenshot or the paragraph explains the failure. It does not spill, and it does not split a
//! round across results; that is [`output_budget`](crate::output_budget). It only enforces a
//! budget, in order, and reports what it did.

use girder_core::ContentBlock;

/// Estimated character-equivalent cost of one attachment block.
///
/// An image's model-visible cost is visual tokens, not the length of its reference: a moderate
/// image is roughly several hundred to sixteen hundred tokens, which at the four-characters-per-
/// token density this workspace uses elsewhere is upward of a thousand character-equivalents. The
/// charge sits at that floor because the true figure is unknowable here — the bytes never reach
/// this layer — and a floor that is never zero is what stops a pile of images from passing a text
/// budget unnoticed.
///
/// A file block is charged the same amount. Its handle text is short, but whether a provider
/// inlines the file's contents is a route's decision, and assuming it does not would underprice
/// exactly the case this budget exists to catch.
///
/// This is an estimate, not a measurement. A caller pricing a route's images exactly should pass
/// a smaller budget for the same request rather than rely on this constant.
pub const ATTACHMENT_COST_CHARS: usize = 1_000;

/// Fewest characters a truncated block may keep.
///
/// A cut to twenty characters buys nothing: the prefix does not identify the block, so the model
/// learns neither the content nor that there was content — the budget was spent on a fragment that
/// reads as corruption. Below this floor the block is omitted whole, and the trailing notice tells
/// the model it existed. The floor also keeps the truncation marker, which is added outside the
/// budget, proportional to what it reports.
pub const MIN_KEPT_CHARS: usize = 64;

/// What happened to one block offered to a [`View`].
///
/// Returned rather than inferred from the rendered text, because a caller often has to decide
/// something else afterwards — retry a call, ask the tool for a narrower output, or spill the
/// original — and "the block is not in the view" is not a reason the caller can reconstruct from
/// the text alone.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Outcome {
    /// The whole block was kept.
    Kept,
    /// The block was shortened, in characters.
    ///
    /// Only text-bearing blocks are ever in this state: an image cannot be half-shown, so an
    /// attachment that does not fit is omitted instead.
    Truncated {
        /// Characters that survived.
        kept: usize,
        /// Characters the block had to begin with.
        total: usize,
    },
    /// Nothing of the block was kept.
    Omitted,
}

/// A budgeted view of one tool result's content blocks.
///
/// Queries are answered from the state built by [`View::add_block`]; nothing is computed from a
/// source text afterwards, so a caller may drop the original blocks as soon as they are offered.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct View {
    /// The ceiling on the sum of retained block costs.
    budget_chars: usize,
    /// Costs charged so far. Never exceeds `budget_chars`.
    spent: usize,
    /// Retained blocks, in the order they were offered.
    parts: Vec<Part>,
    /// Blocks dropped whole.
    omitted: usize,
    /// Blocks shortened. A shortened block is also one of `parts`, not an omission.
    truncated: usize,
}

/// One retained piece of a view.
#[derive(Clone, Debug, PartialEq, Eq)]
enum Part {
    /// Content retained exactly as offered.
    Whole(String),
    /// Content retained as a prefix, with the counts the marker needs.
    Cut {
        /// The retained prefix.
        text: String,
        /// Characters retained.
        kept: usize,
        /// Characters the block had.
        total: usize,
    },
}

impl View {
    /// Creates a view with `budget_chars` characters to spend.
    ///
    /// The budget is a decision threshold, not a hard cap on [`View::render`]: the markers that
    /// report a cut are added on top. See the module documentation for why they are not charged.
    #[must_use]
    pub fn new(budget_chars: usize) -> Self {
        Self {
            budget_chars,
            spent: 0,
            parts: Vec::new(),
            omitted: 0,
            truncated: 0,
        }
    }

    /// Offers one block to the view and reports what became of it.
    ///
    /// Text is charged its character count. An image or file is charged
    /// [`ATTACHMENT_COST_CHARS`] however short its reference is, because the model pays for the
    /// attachment rather than for the name; that charge is what keeps a result of many images from
    /// fitting a text-only budget it does not actually fit.
    ///
    /// A block that does not fit in full is shortened when it carries text and at least
    /// [`MIN_KEPT_CHARS`] of room remains; otherwise it is omitted whole, and the omission is
    /// reported by [`View::render`]. Attachments are never shortened: half an image is not a
    /// smaller image.
    pub fn add_block(&mut self, block: &ContentBlock) -> Outcome {
        let cost = block_cost(block);
        match block {
            ContentBlock::Text { text } | ContentBlock::Reasoning { text } => {
                self.add_text(text, cost)
            }
            other => {
                if cost <= self.remaining() {
                    self.spent = self.spent.saturating_add(cost);
                    self.parts.push(Part::Whole(marker(other)));
                    Outcome::Kept
                } else {
                    self.omitted = self.omitted.saturating_add(1);
                    Outcome::Omitted
                }
            }
        }
    }

    /// Renders the retained blocks as the text a model receives.
    ///
    /// Blocks are separated by newlines and a shortened block carries a marker on the line after
    /// its retained prefix. When anything was shortened or dropped, a trailing notice says how
    /// many blocks were affected and what the budget was.
    #[must_use]
    pub fn render(&self) -> String {
        let mut lines: Vec<String> = Vec::with_capacity(self.parts.len().saturating_add(1));
        for part in &self.parts {
            lines.push(match part {
                Part::Whole(text) => text.clone(),
                Part::Cut { text, kept, total } => {
                    format!("{text}\n[truncated: kept {kept} of {total} characters]")
                }
            });
        }
        if let Some(notice) = self.notice() {
            lines.push(notice);
        }
        lines.join("\n")
    }

    /// How many blocks were dropped whole.
    ///
    /// Shortened blocks are not counted here: they are still in the view, with a marker saying how
    /// much of them survived. A caller that needs both numbers can read the rendered notice.
    #[must_use]
    pub fn omitted(&self) -> usize {
        self.omitted
    }

    /// How many characters of budget remain.
    ///
    /// Never underflows: a block is charged only when its cost is within what remains, so this
    /// reaches zero and stops rather than wrapping.
    #[must_use]
    pub fn remaining(&self) -> usize {
        self.budget_chars.saturating_sub(self.spent)
    }

    /// Adds a text-bearing block, shortening it when it does not fit.
    fn add_text(&mut self, text: &str, cost: usize) -> Outcome {
        let room = self.remaining();
        if cost <= room {
            self.spent = self.spent.saturating_add(cost);
            self.parts.push(Part::Whole(text.to_owned()));
            return Outcome::Kept;
        }
        // A fragment this short identifies nothing, so the whole block is dropped and reported.
        if room < MIN_KEPT_CHARS {
            self.omitted = self.omitted.saturating_add(1);
            return Outcome::Omitted;
        }
        let kept = prefix_of(text, room);
        let kept_chars = kept.chars().count();
        self.spent = self.spent.saturating_add(kept_chars);
        self.truncated = self.truncated.saturating_add(1);
        self.parts.push(Part::Cut {
            text: kept.to_owned(),
            kept: kept_chars,
            total: cost,
        });
        Outcome::Truncated {
            kept: kept_chars,
            total: cost,
        }
    }

    /// Builds the trailing notice, or `None` when the view is complete.
    ///
    /// The line is model-facing prose rather than structured data, because the model is the reader
    /// and a struct field it never sees tells it nothing.
    fn notice(&self) -> Option<String> {
        if self.omitted == 0 && self.truncated == 0 {
            return None;
        }
        let omitted = self.omitted;
        let truncated = self.truncated;
        let total = self.parts.len().saturating_add(omitted);
        let budget = self.budget_chars;
        let cut = if truncated == 0 {
            String::new()
        } else {
            format!(", {truncated} truncated")
        };
        Some(format!(
            "[view: {omitted} of {total} blocks omitted{cut}; budget {budget} characters]"
        ))
    }
}

/// The character cost of one block.
///
/// Text is charged its character count — the unit the budget is denominated in, and the unit a
/// tokenizer roughly sees. An image or a file is charged [`ATTACHMENT_COST_CHARS`], a fixed
/// estimate, because the payload never reaches this layer and its true visual cost depends on a
/// resolution and a route that are not available here. A tool call is charged its name and its
/// raw arguments, both of which are text the model has to read.
///
/// This is an estimate in every arm, but only the attachment arms are guesses: text has a real,
/// countable size, while an attachment has a cost this function can only bound from below.
#[must_use]
pub fn block_cost(block: &ContentBlock) -> usize {
    match block {
        ContentBlock::Text { text } | ContentBlock::Reasoning { text } => text.chars().count(),
        ContentBlock::ToolCall {
            name, arguments, ..
        } => name
            .chars()
            .count()
            .saturating_add(arguments.chars().count()),
        ContentBlock::Image { .. } | ContentBlock::File { .. } => ATTACHMENT_COST_CHARS,
    }
}

/// The longest prefix of `text` holding at most `chars` characters.
///
/// `char_indices` yields the byte offset of each code point, so the offset it returns for the
/// `chars`-th character is itself a `char` boundary and `text[..offset]` cannot split one. Cutting
/// on a byte count instead would turn a multi-byte character into a replacement character, which
/// a model reads as corruption rather than as a cut.
fn prefix_of(text: &str, chars: usize) -> &str {
    match text.char_indices().nth(chars) {
        Some((offset, _)) => &text[..offset],
        None => text,
    }
}

/// The model-facing text for a block that is not prose.
///
/// The wording matches the provider adapters, so a view and a request body describe an attachment
/// the same way and a model that learned one recognises the other.
fn marker(block: &ContentBlock) -> String {
    match block {
        ContentBlock::Image { attachment } => format!("[image {attachment}]"),
        ContentBlock::File { attachment } => format!("[file {attachment}]"),
        ContentBlock::ToolCall { name, .. } => format!("[tool call {name}]"),
        ContentBlock::Text { text } | ContentBlock::Reasoning { text } => text.clone(),
    }
}
