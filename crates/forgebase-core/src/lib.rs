//! Pure business-rule core: metadata, validation, lifecycle definitions and the permission evaluator.
//!
//! Performs no I/O and has no async code or SQL, and depends on no other Forgebase crate.
//! See ADR 0002 and `docs/blueprint/architecture.md`.
//!
//! Implemented so far (milestones M1.1, M2.1 and M2.2):
//!
//! - [`ident`]: identifier rules and table naming for DocTypes and fields.
//! - [`meta`]: the DocType model, [`DocType::from_toml_str`](meta::DocType::from_toml_str) and the
//!   [`Registry`](meta::Registry), including Link and Table fields, child DocTypes and
//!   [`Registry::check_references`](meta::Registry::check_references).
//! - [`value`] and [`document`]: the dynamic value model and document validation against a DocType,
//!   including the rows of child tables.

pub mod document;
pub mod ident;
pub mod meta;
pub mod value;

mod load;
