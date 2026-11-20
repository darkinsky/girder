//! The `terminal.readScreen` Remote: the only way a client sees a rendered screen.
//!
//! [`crate::terminal::controller`] holds the screen model — a grid, a cursor, scrolling regions,
//! wide-character widths — and `Screen::read_screen` turns it into a page of text. Nothing registered
//! that as a Remote, so the whole model was reachable only from Rust. A client asking for a screen got
//! `NOT_IMPLEMENTED` while the answer sat in memory.
//!
//! # Why the arguments are parsed by hand
//!
//! All three fields are counts, and a count has a lower bound a `serde` default cannot express: a
//! missing `count` must keep everything, while a `count` of zero must do the same rather than return
//! an empty page. Reading them with `and_then(as_u64)` would turn a negative or a string into "absent",
//! which is the same "keep everything" answer a caller who sent a nonsense value should not receive.

use std::sync::Arc;

use serde_json::Value;

use crate::api::remote::{RemoteCall, RemoteError, RemoteRegistry};
use crate::terminal::controller::{Screen, ScreenPage, ScreenRequest, page_lines};

/// The namespace `terminal.readScreen` lives in.
pub const NAMESPACE: &str = "terminal";

/// The method name.
pub const METHOD: &str = "readScreen";

/// The default page size, matching the model's own.
pub const DEFAULT_COUNT: usize = 40;

/// The largest page a client may ask for.
///
/// A page is rendered text, so an unbounded `count` lets one call materialise a scrollback the user
/// never asked to see. The cap is the model's own line ceiling.
pub const MAX_COUNT: usize = 10_000;

/// The largest byte budget a page may carry.
pub const MAX_BYTES: usize = 4 * 1024 * 1024;

/// One parsed `terminal.readScreen` argument set.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ScreenArgs {
    /// How many lines back from the newest to start.
    pub offset: usize,
    /// How many lines to include.
    pub count: usize,
    /// The byte ceiling the page must not exceed.
    pub max_bytes: usize,
}

impl Default for ScreenArgs {
    fn default() -> Self {
        Self {
            offset: 0,
            count: DEFAULT_COUNT,
            max_bytes: MAX_BYTES,
        }
    }
}

/// Reads one non-negative count, refusing a value that is present but wrong.
///
/// A `null` is absence; anything else must be a non-negative integer within `max`.
fn count_arg(args: &Value, key: &str, max: usize) -> Result<Option<usize>, String> {
    let Some(raw) = args.get(key) else {
        return Ok(None);
    };
    if raw.is_null() {
        return Ok(None);
    }
    let Some(number) = raw.as_u64() else {
        return Err(format!("`{key}` must be a non-negative integer, got {raw}"));
    };
    let number = usize::try_from(number)
        .map_err(|_| format!("`{key}` is larger than this platform can address"))?;
    if number > max {
        return Err(format!("`{key}` must be at most {max}, got {number}"));
    }
    Ok(Some(number))
}

/// Parses `terminal.readScreen` arguments.
///
/// # Errors
///
/// A message naming the offending field, which the caller turns into `INVALID_ARGUMENTS`.
pub fn parse_args(args: &Value) -> Result<ScreenArgs, String> {
    let offset = count_arg(args, "offset", usize::MAX)?.unwrap_or(0);
    // A `count` of zero would be a page with nothing in it, which no client means. Treating it as
    // absent keeps "I sent zero by accident" from looking like "the screen is empty".
    let count = count_arg(args, "count", MAX_COUNT)?
        .filter(|count| *count > 0)
        .unwrap_or(DEFAULT_COUNT);
    let max_bytes = count_arg(args, "maxBytes", MAX_BYTES)?.unwrap_or(MAX_BYTES);
    Ok(ScreenArgs {
        offset,
        count,
        max_bytes,
    })
}

/// The wire shape of one rendered page.
///
/// `lines` is an array rather than one newline-joined string: a line may itself contain a newline
/// after a wide character was wrapped, so joining would make the count disagree with `total`.
fn page_json(page: &ScreenPage) -> Value {
    serde_json::json!({
        "text": page.text,
        "totalLines": page.total_lines,
        "lineBegin": page.line_begin,
        "lineEnd": page.line_end,
    })
}

/// Registers `terminal.readScreen` against a screen lookup.
///
/// The lookup is a closure rather than a store type because the screens live with whoever owns the
/// live terminals, and this module has no business naming that owner.
pub fn register_screen_method<F>(registry: &mut RemoteRegistry, screen_of: F)
where
    F: Fn(&Value) -> Option<Screen> + Send + Sync + 'static,
{
    let screen_of = Arc::new(screen_of);
    registry.register(NAMESPACE, METHOD, move |call: RemoteCall| {
        let screen_of = Arc::clone(&screen_of);
        Box::pin(async move {
            let args = parse_args(&call.args).map_err(|message| RemoteError::InvalidArguments {
                namespace: NAMESPACE.to_owned(),
                method: METHOD.to_owned(),
                message,
            })?;
            let Some(screen) = screen_of(&call.args) else {
                return Err(RemoteError::Failed(
                    "no live terminal with that id".to_owned(),
                ));
            };
            let request = ScreenRequest {
                offset: args.offset,
                count: args.count,
                max_bytes: args.max_bytes,
            };
            // Through the model rather than a copy of it: `page_lines` is where the byte budget and
            // the truncation flag are decided, and a second implementation would disagree with it.
            let page = page_lines(&screen.render_all(), &request);
            Ok(page_json(&page))
        })
    });
    registry.declare(
        NAMESPACE,
        METHOD,
        crate::guard::access::Declaration::read("terminal"),
    );
}
