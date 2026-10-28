//! Lifecycle hooks: app code that runs at fixed points of a document operation.
//!
//! A hook set is a type that implements [`DocTypeHooks`] and is registered for one DocType with
//! [`Registry::add_hooks`](crate::meta::Registry::add_hooks), explicitly at startup (ADR 0006,
//! ADR 0009). Every method has a default that does nothing, so a hook set implements only the
//! points it needs. This module performs no I/O and has no async code: `forgebase-db` calls the
//! hooks from its document operations.
//!
//! # Pipeline
//!
//! Hooks run inside the operation's database transaction, in the order below, and for each
//! point in registration order (all hook sets of the DocType: the first registered runs first).
//! Steps 1 and 3 mean that hooks run **before** the framework's own validation: a hook may see
//! input that is invalid (unknown fields, wrong value kinds, missing required fields) and must
//! be defensive. The framework validates the document again after the hooks, so a hook cannot
//! leave an invalid document behind.
//!
//! | Operation | Order |
//! |---|---|
//! | insert | `before_insert`, `validate`, framework validation, Link resolution, naming, write |
//! | update | lock and load the stored document (not found and version conflict are reported here, before any hook), merge the changes, `validate`, framework validation of the whole document, Link resolution, write |
//! | delete | lock and load the stored document (not found and version conflict as above), `before_delete`, delete |
//!
//! # Rules
//!
//! - **No database access.** A hook gets the document and a [`HookContext`], nothing else. Hooks
//!   that read other documents, and effects that must happen after commit, are not supported
//!   yet (ADR 0009).
//! - **An error aborts the operation.** A hook returns `Err(HookError)` to reject the
//!   operation; the transaction (a savepoint inside a caller-owned transaction) is rolled back
//!   and nothing is persisted. `forgebase-db` reports it as `DbError::Hook`.
//! - **A panic is not caught.** A hook is app code: a panic aborts the operation like any
//!   other panic in the caller (the transaction is rolled back when it unwinds).
//! - **Child DocTypes have no hooks.** The rows of a child table are part of the parent
//!   document that hooks of the parent DocType receive (`doc.tables`).
//! - Hooks are synchronous and must be quick: they run while the document's row is locked.
//!
//! # Example
//!
//! ```
//! use std::sync::Arc;
//!
//! use forgebase_core::document::Document;
//! use forgebase_core::hooks::{DocTypeHooks, HookContext, HookError, Operation};
//! use forgebase_core::meta::{DocType, Registry};
//! use forgebase_core::value::Value;
//!
//! /// Normalizes the email and rejects a customer without a domain.
//! struct CustomerRules;
//!
//! impl DocTypeHooks for CustomerRules {
//!     fn validate(&self, _ctx: &HookContext<'_>, doc: &mut Document) -> Result<(), HookError> {
//!         // Hooks run before framework validation: the value may be missing or of any kind.
//!         if let Some(Value::Text(email)) = doc.values.get_mut("email") {
//!             *email = email.trim().to_lowercase();
//!             if !email.contains('@') {
//!                 return Err(HookError::new("email must contain an @"));
//!             }
//!         }
//!         Ok(())
//!     }
//! }
//!
//! let customer = DocType::from_toml_str(
//!     "name = \"customer\"\nlabel = \"Customer\"\n\
//!      [[fields]]\nname = \"email\"\ntype = \"data\"\n",
//! )?;
//! let mut registry = Registry::new();
//! registry.add(customer).map_err(|errors| format!("{errors:?}"))?;
//! registry.add_hooks("customer", Arc::new(CustomerRules))?;
//! assert_eq!(registry.hooks("customer").len(), 1);
//!
//! // `forgebase-db` calls the hooks; here we call one directly.
//! let ctx = HookContext {
//!     doctype: "customer",
//!     actor: "ann",
//!     operation: Operation::Insert,
//! };
//! let mut doc = Document::default();
//! doc.values.insert("email".to_owned(), Value::Text("  Ann@Example.com ".to_owned()));
//! registry.hooks("customer")[0].validate(&ctx, &mut doc)?;
//! assert_eq!(doc.values["email"], Value::Text("ann@example.com".to_owned()));
//!
//! doc.values.insert("email".to_owned(), Value::Text("nobody".to_owned()));
//! assert!(registry.hooks("customer")[0].validate(&ctx, &mut doc).is_err());
//! # Ok::<(), Box<dyn std::error::Error>>(())
//! ```

use std::fmt;

use crate::document::Document;

/// The operation a hook runs for. New operations are added in later milestones (submit and
/// cancel), so match with a wildcard arm.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum Operation {
    /// A new document is being inserted.
    Insert,
    /// An existing document is being updated.
    Update,
    /// An existing document is being deleted.
    Delete,
}

/// What a hook is told about the operation, besides the document.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct HookContext<'a> {
    /// The name of the DocType.
    pub doctype: &'a str,
    /// The actor of the operation, as given to `forgebase-db` (recorded, not authenticated).
    /// Set for every operation, including [`Operation::Delete`].
    pub actor: &'a str,
    /// The operation.
    pub operation: Operation,
}

/// A hook rejects an operation by returning this. The message is shown to the caller.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HookError {
    message: String,
}

impl HookError {
    /// Creates an error with the given message.
    pub fn new(message: impl Into<String>) -> Self {
        Self {
            message: message.into(),
        }
    }

    /// The message.
    pub fn message(&self) -> &str {
        &self.message
    }
}

impl fmt::Display for HookError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.message)
    }
}

impl std::error::Error for HookError {}

/// Lifecycle hooks of one DocType. Every method does nothing by default; see the
/// [module documentation](self) for the pipeline order and the rules every hook follows.
///
/// A hook that returns an error rejects the whole operation; nothing is persisted.
/// `before_submit` and `before_cancel` will be added as further default methods with
/// docstatus (M2.4), so adding them does not break existing implementations.
pub trait DocTypeHooks: Send + Sync {
    /// Runs first when a document is inserted, on the input document, which may be modified.
    /// The document is not validated yet.
    fn before_insert(&self, ctx: &HookContext<'_>, doc: &mut Document) -> Result<(), HookError> {
        let _ = (ctx, doc);
        Ok(())
    }

    /// Runs on insert (after `before_insert`) and on update. The document may be modified.
    /// It is not validated yet by the framework.
    ///
    /// On update `doc` is the **whole** document: the stored values with the changes applied,
    /// and the rows of every table (the stored ones for a table the changes do not mention).
    /// `doc.name` is the stored name; changing the naming field is rejected afterwards. A value
    /// a hook removes from `doc.values` is cleared (as on insert, an absent value is null); to
    /// remove all rows of a table set an empty list, because a table key that is removed from
    /// `doc.tables` keeps the stored rows.
    fn validate(&self, ctx: &HookContext<'_>, doc: &mut Document) -> Result<(), HookError> {
        let _ = (ctx, doc);
        Ok(())
    }

    /// Runs before a document is deleted. `doc` is a read-only view of the stored document,
    /// including its table rows.
    fn before_delete(&self, ctx: &HookContext<'_>, doc: &Document) -> Result<(), HookError> {
        let _ = (ctx, doc);
        Ok(())
    }
}
