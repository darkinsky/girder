//! Where generated code finds the schema types.
//!
//! A derive inside `girder-core` and a derive in a downstream crate resolve `::girder_core`
//! differently, so the path is a variable rather than a literal.

/// The path to the crate holding `Schema` and friends, as parseable tokens.
///
/// `::girder_core` in a downstream crate. Inside `girder-core` itself that path does not exist, so a
/// derive there would fail to compile — which is a limitation, not a bug: this crate has no
/// self-referential derive today, and adding one would need a `crate` path variant.
///
/// A `TokenStream` rather than a string because `quote!` inserts a string as a *literal*, which
/// produces `expected type, found "::girder_core"` rather than a path.
#[must_use]
pub fn schema_crate() -> proc_macro2::TokenStream {
    let path: syn::Path = syn::parse_quote!(::girder_core);
    quote::quote!(#path)
}
