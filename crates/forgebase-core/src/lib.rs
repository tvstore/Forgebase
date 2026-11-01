//! Pure business-rule core: metadata, validation, lifecycle definitions and the permission evaluator.
//!
//! **Internal crate, not a public API:** applications use the `forgebase` crate, and nothing here
//! has a compatibility promise (ADR 0012). Performs no I/O and has no async code or SQL, and
//! depends on no other Forgebase crate.
//! See ADR 0002 and `docs/blueprint/architecture.md`.
//!
//! Implemented so far (milestones M1.1, M2.1, M2.2, M2.3, M2.4, M3.2, M8.1, M8.4 and M9.1):
//!
//! - [`ident`]: identifier rules and table naming for DocTypes and fields.
//! - [`meta`]: the DocType model, [`DocType::from_toml_str`](meta::DocType::from_toml_str) and the
//!   [`Registry`](meta::Registry), including Link and Table fields, child DocTypes and
//!   [`Registry::check_references`](meta::Registry::check_references).
//! - [`value`] and [`document`]: the dynamic value model and document validation against a DocType,
//!   including the rows of child tables.
//! - [`hooks`]: the [`DocTypeHooks`](hooks::DocTypeHooks) trait for lifecycle hooks, registered
//!   with [`Registry::add_hooks`](meta::Registry::add_hooks); the pipeline order is documented
//!   there; `before_submit` and `before_cancel` arrived with docstatus.
//! - [`series`]: naming series patterns (`INV-.YYYY.-.#####`), parsing and rendering.
//! - [`permission`]: `[[permissions]]` rules, [`Right`](permission::Right), the
//!   [`Caller`](permission::Caller) and [`evaluate`](permission::evaluate), the one function that
//!   decides access (M3.2, ADR 0011).
//! - [`workflow`]: the states and transitions of a DocType's `[workflow]` (M8.1, ADR 0015); who
//!   may take a transition is [`transition_access`](permission::transition_access).
//!
//! - [`method`]: the definition of an app method endpoint (M9.1, ADR 0019); who may call one is
//!   [`can_call_method`](permission::can_call_method).
//!
//! Documents of a `submittable` DocType have a [`DocStatus`](document::DocStatus) (draft,
//! submitted, cancelled).

pub mod document;
pub mod hooks;
pub mod ident;
pub mod meta;
pub mod method;
pub mod permission;
pub mod report;
pub mod series;
pub mod value;
pub mod workflow;

mod load;

/// How many characters of user input an error message repeats.
pub const ECHO_CHARS: usize = 64;

/// `text` shortened for an error message: at most [`ECHO_CHARS`] characters (cut on a character
/// boundary) and `…` if something was cut. Input that an error repeats (a column name, a cell, a
/// rejected option) can be megabytes long, and a message must not grow with it.
pub fn shorten(text: &str) -> String {
    match text.char_indices().nth(ECHO_CHARS) {
        None => text.to_owned(),
        Some((end, _)) => format!("{}…", &text[..end]),
    }
}

#[cfg(test)]
mod shorten_tests {
    use super::{ECHO_CHARS, shorten};

    #[test]
    fn long_input_is_cut_on_a_character_boundary() {
        assert_eq!(shorten("short"), "short");
        let exact = "x".repeat(ECHO_CHARS);
        assert_eq!(shorten(&exact), exact);
        assert_eq!(shorten(&"x".repeat(ECHO_CHARS + 1)), format!("{exact}…"));
        let wide = "é".repeat(1000);
        assert_eq!(shorten(&wide).chars().count(), ECHO_CHARS + 1);
        assert!(shorten(&"\u{1}".repeat(1_000_000)).len() < 100);
    }
}
