//! Derive macros for the type catalogue.
//!
//! The schema shape was decided by hand first, in `girder_core::typert`. These macros generate the
//! same thing; the hand-written version is the standard the macro output is tested against, because a
//! macro that disagreed with a considered design would be the macro's fault, not the design's.
//!
//! # Why a derive rather than analysis
//!
//! A Rust procedural macro sees type *tokens*, not a resolved type graph: `Vec<SessionSummary>` is a
//! name and nothing more. So a type participates by deriving this, and a type used inside another
//! must derive it too. That is more annotation than TypeScript needs, and it is the honest cost of
//! having no type checker available at this point.

use proc_macro::TokenStream;

mod derive;
mod schema_path;

/// Derives the schema description of a struct.
///
/// # Attributes
///
/// - `#[typert(name = "...")]` on the struct: the schema name, defaulting to the Rust name.
/// - `#[typert(added_in = N)]` on the struct or a field: the revision that introduced it.
/// - `#[typert(optional)]` on a field: omittable regardless of `Option`.
/// - `#[typert(rename = "...")]` on a field: the wire name.
///
/// A field whose type is `Option<T>` is nullable and optional. A field documented with `///` carries
/// that documentation into the schema.
#[proc_macro_derive(Typert, attributes(typert))]
pub fn derive_typert(input: TokenStream) -> TokenStream {
    derive::expand(input)
}
