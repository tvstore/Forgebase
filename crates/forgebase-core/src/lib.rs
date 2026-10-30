//! Pure business-rule core: metadata, validation, lifecycle definitions and the permission evaluator.
//!
//! **Internal crate, not a public API:** applications use the `forgebase` crate, and nothing here
//! has a compatibility promise (ADR 0012). Performs no I/O and has no async code or SQL, and
//! depends on no other Forgebase crate.
//! See ADR 0002 and `docs/blueprint/architecture.md`.
//!
//! Implemented so far (milestones M1.1, M2.1, M2.2, M2.3, M2.4, M3.2, M8.1 and M8.4):
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
//! Documents of a `submittable` DocType have a [`DocStatus`](document::DocStatus) (draft,
//! submitted, cancelled).

pub mod document;
pub mod hooks;
pub mod ident;
pub mod meta;
pub mod permission;
pub mod report;
pub mod series;
pub mod value;
pub mod workflow;

mod load;
