//! Pure business-rule core: metadata, validation, lifecycle definitions and the permission evaluator.
//!
//! Performs no I/O and has no async code or SQL, and depends on no other Forgebase crate.
//! See ADR 0002 and `docs/blueprint/architecture.md`.
//!
//! Implemented so far (milestone M1.1):
//!
//! - [`ident`]: identifier rules and table naming for DocTypes and fields.
//! - [`meta`]: the DocType model, [`DocType::from_toml_str`](meta::DocType::from_toml_str) and the
//!   [`Registry`](meta::Registry).
//! - [`value`] and [`document`]: the dynamic value model and document validation against a DocType.

pub mod document;
pub mod ident;
pub mod meta;
pub mod value;

mod load;
