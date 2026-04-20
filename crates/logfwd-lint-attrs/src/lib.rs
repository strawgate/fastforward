//! Attribute macros that tag functions for semantic lints.
//!
//! Each attribute is a compile-time marker for the dylint lint library
//! (`crates/logfwd-lints/`). The macros may attach hidden metadata so
//! the lint can find annotated items after macro expansion, but they
//! have no runtime or codegen effect and do not change function
//! behavior.
//!
//! # Available attributes
//!
//! - `#[hot_path]` — the tagged function is on the pipeline's hot path.
//!   The `hot_path_no_alloc` lint flags any heap allocation inside the
//!   function body (direct calls to `Box::new`, `Vec::new`,
//!   `Vec::with_capacity`, `String::from`, `.to_string()`, `.to_vec()`,
//!   `.to_owned()`, `format!`, `vec![]`, `.collect()` into an owned
//!   container, `Arc::new`, `Rc::new`).
//!
//! # Runtime behavior
//!
//! These attributes generate no code. They exist purely so the lint can
//! find them. Functions behave identically with or without the attribute
//! at runtime.

#![warn(missing_docs)]

use proc_macro::TokenStream;

/// Mark a function as lying on the pipeline hot path.
///
/// The `hot_path_no_alloc` dylint lint flags any heap allocation inside
/// a function carrying this attribute. See crate-level docs.
///
/// The macro prepends a `#[doc = "__logfwd_hot_path__"]` marker so the
/// lint can find the function after macro expansion. At runtime the
/// function behaves identically to the un-annotated version; the doc
/// attribute has no codegen effect. Note: the marker string may appear
/// in rustdoc output for public items.
#[proc_macro_attribute]
pub fn hot_path(_attr: TokenStream, item: TokenStream) -> TokenStream {
    let marker: TokenStream = "#[doc = \"__logfwd_hot_path__\"]"
        .parse()
        .expect("valid marker attribute");
    let mut out = marker;
    out.extend(item);
    out
}
