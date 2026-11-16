//! Provider-side vision-token pricing: what one request image costs a model route.
//!
//! Upstream path: `packages/llm/llm-deepseek/src/image-tokens.ts`, together with the image arms of
//! `packages/llm/llm-deepseek/src/request-pricing.ts`.
//!
//! # Why this module exists
//!
//! [`crate::token_meter`] prices content by a fixed character density and documents, twice, that its
//! image and file arms are a *lower bound* because an attachment is an opaque reference at that
//! layer: the bytes never reach the request, so the visible cost of a real image is unknowable
//! there. Upstream does not leave that gap open. It carries the provider's own published image-token
//! calculator, which turns a pixel geometry into an exact token count, and it prices every retained
//! image occurrence with it before the request is dispatched. Nothing in this build did that, so a
//! request carrying an image was priced at the length of an opaque id.
//!
//! That is not a rounding difference. The heuristic charges roughly `len(attachment) / 4` tokens for
//! an image; the provider charges up to [`MAX_IMAGE_TOKENS`] — a factor of a hundred or more. On the
//! path that matters most, a pre-flight context gate, that error is one-directional and lands in the
//! dangerous direction: the gate reports room that does not exist, the request is sent, and the
//! provider rejects it after a round trip.
//!
//! # What the provider actually does
//!
//! The published accounting, in its current `v41` configuration, has four steps:
//!
//! 1. An image under `544 * 544` total pixels is scaled **up** to that floor, so a 100x100 icon
//!    costs exactly what a 544x544 one costs.
//! 2. Both axes are padded to a whole number of [`PATCH_SIZE`]-pixel patches.
//! 3. Patch counts are downsampled 3:1 per axis into token *cells*, and a grid of `h` rows and `w`
//!    columns costs `h * (w + 1) + 2` tokens: every row carries a separator, and the grid carries
//!    two framing tokens.
//! 4. A grid over [`MAX_IMAGE_TOKENS`] is replaced by the largest aspect-preserving grid that fits,
//!    solved in closed form.
//!
//! This configuration has no alignment pad and no aspect-ratio clamp, so the count is exact rather
//! than an estimate. Actual provider usage remains authoritative for a completed request; this is
//! the *predicted* price used before dispatch.
//!
//! # Why the request dimensions are priced, not the source dimensions
//!
//! Steps 1-4 price a geometry, and the geometry the provider sees is the one the harness encodes
//! and sends, which is not always the source's. A source whose padded grid already fits the cap is
//! sent unchanged, but a larger one is downscaled at its *solved long edge*, and rounding the
//! aspect-preserving short edge can move the result across a token-cell boundary. [`price_request_image`]
//! therefore prices [`Dimensions`] — the sent geometry — exactly as upstream's request pricing does.
//! Pricing the source would be wrong in the ordinary case, not only at the boundary: for a
//! 1224x1429 source the source prices at 959 tokens while the 1187x1386 request prices at 992.
//!
//! # Why floating point is used deliberately, and only here
//!
//! Step 4 is a closed-form solve over a square root and a per-axis scale, and reproducing the
//! provider's published numbers requires performing the *same* arithmetic. An integer re-derivation
//! would be a different algorithm that happens to agree on most inputs and disagree exactly at the
//! cell boundaries this module exists to get right, so the float operations are kept and annotated
//! rather than "cleaned up". Everything outside the solver — padding, cells, grid tokens — is exact
//! integer arithmetic.
//!
//! # Why nothing here panics
//!
//! Upstream throws on two conditions it marks as unreachable: a solved grid that overruns the budget,
//! and a resize pass that fails to reach a fixpoint. Both are guarded here by returning the last
//! computed price instead. The reason is that neither guard is a condition a caller can act on: the
//! value is already computed and is a valid, in-budget price, so turning it into a panic would replace
//! a usable number with a crashed request. A zero dimension is the one input that has no price at
//! all, and it is answered with `0` rather than with the two framing tokens upstream's arithmetic
//! would produce for an image that does not exist.

/// Vision patch edge in pixels.
///
/// The provider grids an image into patches of this size before counting cells. Named rather than
/// inlined because it appears in the padding step, the cell step, and the aspect-preserving resize.
pub const PATCH_SIZE: u64 = 14;

/// Per-axis patch-to-token downsampling ratio.
///
/// Three patches along either axis become one token cell, so one cell covers 42 pixels per side.
pub const DOWNSAMPLE_RATIO: u64 = 3;

/// Provider cap on tokens charged for one request image.
pub const MAX_IMAGE_TOKENS: u64 = 1024;

/// Total-pixel floor; smaller images are scaled up before the grid projection.
///
/// `544 * 544` is the published threshold, and it is why a small icon is not cheap.
pub const MIN_PIXELS: u64 = 544 * 544;

/// Pixels covered by one token cell along either axis.
const CELL_SIZE: u64 = PATCH_SIZE * DOWNSAMPLE_RATIO;

/// Pixel geometry of an image, in the dimensions the provider will actually receive.
///
/// A distinct type rather than a `(u32, u32)` tuple because the two fields are easy to transpose at
/// a call site and the provider's answer is asymmetric in them: a 9000x1 image and a 1x9000 image
/// cost different numbers of tokens, so swapping them is a real bug that a named field catches.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Dimensions {
    /// Width in pixels.
    pub width: u32,
    /// Height in pixels.
    pub height: u32,
}

impl Dimensions {
    /// Builds a geometry from a width and a height.
    #[must_use]
    pub const fn new(width: u32, height: u32) -> Self {
        Self { width, height }
    }

    /// Whether this geometry describes an image at all.
    ///
    /// A zero on either axis is not a degenerate image but the absence of one, which is why it has
    /// no price rather than the cheapest possible price. Taken by value because the type is two
    /// machine words and copying it is cheaper than the indirection.
    #[must_use]
    pub const fn is_empty(self) -> bool {
        self.width == 0 || self.height == 0
    }
}

/// What one request image costs the route that sends it.
///
/// The geometry and the token count travel together because they are only meaningful together: the
/// price is a function of the *sent* dimensions, so a caller that keeps the number without the
/// dimensions cannot check the arithmetic or explain the number to a reader.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ImageRequestPrice {
    /// The dimensions the harness encodes and sends.
    pub dimensions: Dimensions,
    /// Vision tokens the provider charges for those dimensions, at most [`MAX_IMAGE_TOKENS`].
    pub tokens: u64,
}

/// One resized grid, as the closed-form solver reports it.
///
/// `best_*` are pixel dimensions and `grid_*` are token-cell counts, mirroring upstream's
/// `GridResize`. Both are kept because the fixpoint loop feeds the pixels back in while equality is
/// decided on all five fields.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct GridResize {
    grid_height: u64,
    grid_width: u64,
    best_height: u64,
    best_width: u64,
    num_tokens: u64,
}

/// Token count of one grid: every row carries a separator, plus two framing tokens.
///
/// Saturating rather than wrapping: a caller that reaches this with a pathological geometry should
/// see a huge price, not a small one that looks affordable.
fn grid_tokens(grid_height: u64, grid_width: u64) -> u64 {
    grid_height
        .saturating_mul(grid_width.saturating_add(1))
        .saturating_add(2)
}

/// Token-cell count along one patch-padded pixel axis.
///
/// Divides into whole patches first, then downsamples — the same order the provider documents, and
/// the order matters: downsampling before padding would charge a cell for a partial patch.
fn grid_cells(padded_length: u64) -> u64 {
    (padded_length / PATCH_SIZE).div_ceil(DOWNSAMPLE_RATIO)
}

/// Pads a pixel length up to a whole number of patches.
fn pad_to_patch(length: u64) -> u64 {
    length.div_ceil(PATCH_SIZE).saturating_mul(PATCH_SIZE)
}

/// Solves the largest aspect-preserving grid that fits `budget` tokens.
///
/// This is the provider's closed form. The ideal grid width comes from inverting
/// `h * (w + 1) + 2 <= budget` for a fixed aspect ratio, which yields the quadratic root
/// `sqrt((budget - 2) / aspect + 0.25) - 0.5`; truncating it to whole cells and then to whole patches
/// gives the grid the provider uses.
///
/// The two `ideal < 1` arms are not defensive padding: an extremely wide image has an aspect ratio
/// near zero, so the width root is below one cell and the solver must fall back to a single-column
/// grid, which is exactly how a 9000x1 strip reaches the 1024-token cap.
#[allow(
    clippy::cast_precision_loss,
    clippy::cast_possible_truncation,
    clippy::cast_sign_loss,
    reason = "mirrors the provider's published float solve; every cast is a pixel or cell count \
              below 2^53 with a non-negative value, and truncation is the published rounding"
)]
fn solve_resize_ratio(height: u64, width: u64, budget: u64) -> GridResize {
    // Both dimensions are non-zero here: `image_tokens` answers an empty geometry with 0 before any
    // solver runs, so the aspect ratio is a real quotient.
    let aspect = height as f64 / width as f64;
    let ideal_grid_width = (((budget.saturating_sub(2)) as f64) / aspect + 0.25).sqrt() - 0.5;
    let ideal_grid_height = ideal_grid_width * aspect;

    let (best_width, best_height) = if ideal_grid_width < 1.0 {
        // Too narrow for even one cell wide: one column, as many rows as the budget allows.
        let solved_grid_width = 1;
        let solved_grid_height = (budget.saturating_sub(2)) / (solved_grid_width + 1);
        (
            solved_grid_width * CELL_SIZE,
            solved_grid_height.saturating_mul(CELL_SIZE),
        )
    } else if ideal_grid_height < 1.0 {
        // Too short for one cell tall: one row, as many columns as the budget allows.
        let solved_grid_height = 1;
        let solved_grid_width = (budget.saturating_sub(2)) / solved_grid_height - 1;
        (
            solved_grid_width.saturating_mul(CELL_SIZE),
            solved_grid_height * CELL_SIZE,
        )
    } else {
        let solved_grid_width = ideal_grid_width.trunc() as u64;
        let solved_grid_height = ideal_grid_height.trunc() as u64;
        let scale = (solved_grid_width.saturating_mul(CELL_SIZE) as f64 / width as f64)
            .min(solved_grid_height.saturating_mul(CELL_SIZE) as f64 / height as f64);
        let snapped_width = (width as f64 * scale / PATCH_SIZE as f64).trunc() as u64 * PATCH_SIZE;
        let snapped_height =
            (height as f64 * scale / PATCH_SIZE as f64).trunc() as u64 * PATCH_SIZE;
        (snapped_width, snapped_height)
    };

    let grid_height = grid_cells(best_height);
    let grid_width = grid_cells(best_width);
    GridResize {
        grid_height,
        grid_width,
        best_height,
        best_width,
        num_tokens: grid_tokens(grid_height, grid_width),
    }
}

/// Projects a padded pixel geometry onto an in-budget token grid.
///
/// `height` and `width` are the possibly-upscaled source dimensions the solver needs for its aspect
/// ratio; `padded_height` and `padded_width` are the patched geometry whose cells are counted
/// directly. Keeping them separate is what lets a grid that already fits be returned verbatim —
/// rounding it again would change a price that was already correct.
fn safe_resize(height: u64, width: u64, padded_height: u64, padded_width: u64) -> GridResize {
    let grid_height = grid_cells(padded_height);
    let grid_width = grid_cells(padded_width);
    let direct = GridResize {
        grid_height,
        grid_width,
        best_height: padded_height,
        best_width: padded_width,
        num_tokens: grid_tokens(grid_height, grid_width),
    };
    if direct.num_tokens <= MAX_IMAGE_TOKENS {
        return direct;
    }
    // Over the cap: the solver's grid is in budget by construction. It is not re-asserted here,
    // because the only response available to a violation would be a panic, and the value it would
    // panic in place of is a usable price.
    solve_resize_ratio(height, width, MAX_IMAGE_TOKENS)
}

/// One scale-pad-project pass; the caller iterates it to a fixpoint.
///
/// A single pass can undershoot: the grid it solves is expressed in pixels, and re-projecting those
/// pixels is what proves the price is stable. Small images are scaled up first so the answer matches
/// the provider's documented floor.
#[allow(
    clippy::cast_precision_loss,
    clippy::cast_possible_truncation,
    clippy::cast_sign_loss,
    reason = "the scale factor is a positive root and the products are pixel counts below 2^53; \
              truncation is the published rounding"
)]
fn resize_once(width: u64, height: u64) -> GridResize {
    let mut scaled_width = width;
    let mut scaled_height = height;
    let pixels = scaled_width.saturating_mul(scaled_height);
    if pixels > 0 && pixels < MIN_PIXELS {
        let scale = (MIN_PIXELS as f64 / pixels as f64).sqrt();
        scaled_width = (scaled_width as f64 * scale).trunc() as u64;
        scaled_height = (scaled_height as f64 * scale).trunc() as u64;
    }
    let padded_width = pad_to_patch(scaled_width);
    let padded_height = pad_to_patch(scaled_height);
    safe_resize(scaled_height, scaled_width, padded_height, padded_width)
}

/// Vision tokens the provider charges for one request image of this geometry.
///
/// This is upstream's `deepSeekImageTokens`: the published calculator, exact for the `v41`
/// configuration (no alignment pad, no aspect-ratio clamp), so the result is a price rather than an
/// estimate. It is capped at [`MAX_IMAGE_TOKENS`] for every geometry, and small images price at the
/// [`MIN_PIXELS`] floor, so a 100x100 thumbnail costs exactly what a 544x544 one does.
///
/// A zero dimension answers `0`: no image is present, so there is nothing to charge for.
///
/// A geometry whose projection does not reach a fixpoint within ten passes returns the last
/// computed price. Every pass is a projection onto a grid, so a second identical pass is a fixpoint
/// and the guard is unreachable in practice; returning the value is the alternative to panicking
/// over a number that is already usable.
///
/// # Examples
///
/// ```text
/// // The provider's published reference geometry: a 640x480 image prices at 206 tokens.
/// assert_eq!(image_tokens(640, 480), 206);
/// // Everything is capped, however large the source.
/// assert!(image_tokens(5000, 5000) <= 1024);
/// ```
///
/// These are `text` rather than `rust` blocks because this module is not yet declared in the crate
/// root; wiring `pub mod llm_gap;` belongs to the integrator, and a compiled doctest would fail
/// until then. `tests/llm_gap.rs` asserts the same numbers today.
#[must_use]
pub fn image_tokens(width: u32, height: u32) -> u64 {
    if width == 0 || height == 0 {
        return 0;
    }
    let mut result = resize_once(u64::from(width), u64::from(height));
    for _ in 1..10 {
        let next = resize_once(result.best_width, result.best_height);
        if next == result {
            return result.num_tokens;
        }
        result = next;
    }
    result.num_tokens
}

/// The dimensions the harness sends so the provider keeps the whole image.
///
/// This is upstream's `deepSeekRequestImageDimensions`. When the source's patch-padded grid already
/// fits [`MAX_IMAGE_TOKENS`] the source itself is sent, and small images are never enlarged — the
/// provider pads the short edge to whole patches on its side. Otherwise the source aspect ratio is
/// preserved at the solved grid's long edge.
///
/// The result is not a cosmetic resize: it is the geometry the request carries, and therefore the
/// geometry [`price_request_image`] prices. A caller that wants the token cost of a source image must
/// price *these* dimensions, not the source's.
#[must_use]
pub fn request_dimensions(width: u32, height: u32) -> Dimensions {
    if width == 0 || height == 0 {
        return Dimensions::new(0, 0);
    }
    let padded_width = pad_to_patch(u64::from(width));
    let padded_height = pad_to_patch(u64::from(height));
    if grid_tokens(grid_cells(padded_height), grid_cells(padded_width)) <= MAX_IMAGE_TOKENS {
        return Dimensions::new(width, height);
    }
    let solved = solve_resize_ratio(u64::from(height), u64::from(width), MAX_IMAGE_TOKENS);
    let long_edge = if width >= height {
        solved.best_width
    } else {
        solved.best_height
    };
    long_edge_dimensions(Dimensions::new(width, height), long_edge)
}

/// Rescales a geometry to an exact long edge, rounding the short edge to the nearest pixel.
///
/// An encoder resizing by the long edge alone does this, so a price computed from the result matches
/// what the provider decodes. A long edge at or beyond the source's own returns the source, which
/// keeps "never enlarge" true on this path too.
#[allow(
    clippy::cast_precision_loss,
    clippy::cast_possible_truncation,
    clippy::cast_sign_loss,
    reason = "the long edge is a pixel count below 2^53 and the rounded short edge is non-negative"
)]
fn long_edge_dimensions(source: Dimensions, long_edge: u64) -> Dimensions {
    let width = u64::from(source.width);
    let height = u64::from(source.height);
    let long_edge = long_edge.clamp(1, width.max(height));
    if long_edge >= width.max(height) {
        return source;
    }
    if width >= height {
        // `round` is half-away-from-zero, matching the reference's `Math.round` for positive values.
        let short = (long_edge as f64 * height as f64 / width as f64).round() as u64;
        Dimensions::new(
            u32::try_from(long_edge).unwrap_or(u32::MAX),
            u32::try_from(short.max(1)).unwrap_or(u32::MAX),
        )
    } else {
        let short = (long_edge as f64 * width as f64 / height as f64).round() as u64;
        Dimensions::new(
            u32::try_from(short.max(1)).unwrap_or(u32::MAX),
            u32::try_from(long_edge).unwrap_or(u32::MAX),
        )
    }
}

/// Prices one source image the way the route will actually send it.
///
/// This is the composition upstream's `deepSeekImageRequestPricing` performs for a retained image
/// occurrence: resolve the sent geometry with [`request_dimensions`], then price those dimensions
/// with [`image_tokens`]. Doing it in one call is the point — the two steps must use the same
/// geometry, and a caller that pairs [`image_tokens`] with its *source* dimensions silently
/// over- or under-prices every image that the cap resizes.
///
/// A text-only route prices an image occurrence as its placeholder text instead; that is the
/// substitution path, not this one, and this function deliberately reports the vision price a route
/// that can see images would pay.
///
/// # Examples
///
/// ```text
/// // A source that must be downscaled prices at its sent geometry, not its own.
/// let price = price_request_image(1224, 1429);
/// assert_eq!(price.dimensions, Dimensions::new(1187, 1386));
/// assert_eq!(price.tokens, 992);
/// ```
#[must_use]
pub fn price_request_image(width: u32, height: u32) -> ImageRequestPrice {
    let dimensions = request_dimensions(width, height);
    ImageRequestPrice {
        dimensions,
        tokens: image_tokens(dimensions.width, dimensions.height),
    }
}
