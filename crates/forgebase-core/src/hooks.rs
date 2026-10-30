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
//! | delete | lock and load the stored document (not found and version conflict as above), docstatus check, `before_delete`, delete |
//! | submit | lock and load the stored document (as above), docstatus check (draft only), `validate`, `before_submit`, framework validation of the whole document, Link resolution, write with docstatus 1 |
//! | cancel | lock and load the stored document (as above), docstatus check (submitted only), `before_cancel`, write with docstatus 2 |
//! | amend | lock and load the cancelled document (as above), docstatus check (cancelled only, not amended yet), then the **insert** pipeline on a copy of it: `before_insert`, `validate`, framework validation, Link resolution, naming (the amended name), write as a draft |
//!
//! **Permissions** come first (M3.2, ADR 0011): `forgebase-db` checks the caller's rights before
//! the first step, and in the "lock and load" steps a document the caller cannot read is "not
//! found", so a refused operation runs no hook. Hooks therefore see operations that are allowed;
//! they are not a place to check permissions.
//!
//! `submit`, `cancel` and `amend` exist only for DocTypes with `submittable = true`. A hook sees
//! which operation it runs for in [`HookContext::operation`]: `validate` runs for
//! [`Operation::Insert`], [`Operation::Update`], [`Operation::Submit`] and [`Operation::Amend`],
//! and `before_insert` for [`Operation::Insert`] and [`Operation::Amend`] (an amended
//! document is a new draft, so the hooks that every new document goes through run for it; check
//! the operation where the two must differ).
//!
//! # Rules
//!
//! - **No database access.** A hook gets the document and a [`HookContext`], nothing else. Hooks
//!   that read other documents are not supported yet (ADR 0009).
//! - **Effects after commit go through the outbox.** A hook may [`HookContext::enqueue`] a
//!   background job. The job is written in the same transaction as the document change, after
//!   all hooks have run, so it exists if and only if the change is committed (ADR 0013).
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
//! let ctx = HookContext::new("customer", "ann", Operation::Insert);
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
use std::sync::Mutex;

use crate::document::Document;

/// The largest payload of a job: 64 KiB of serialized JSON. It applies to every way of
/// enqueueing a job.
pub const MAX_JOB_PAYLOAD_BYTES: usize = 64 * 1024;

/// Checks a job payload: at most [`MAX_JOB_PAYLOAD_BYTES`] of serialized JSON, and no string
/// with a NUL character (PostgreSQL `jsonb` cannot store one). Returns the reason when refused.
pub fn check_job_payload(payload: &serde_json::Value) -> Result<(), String> {
    fn has_nul(value: &serde_json::Value) -> bool {
        match value {
            serde_json::Value::String(text) => text.contains('\0'),
            serde_json::Value::Array(items) => items.iter().any(has_nul),
            serde_json::Value::Object(map) => map
                .iter()
                .any(|(key, value)| key.contains('\0') || has_nul(value)),
            _ => false,
        }
    }
    let size = serde_json::to_vec(payload).map_or(usize::MAX, |bytes| bytes.len());
    if size > MAX_JOB_PAYLOAD_BYTES {
        return Err(format!(
            "the job payload is larger than {MAX_JOB_PAYLOAD_BYTES} bytes of JSON"
        ));
    }
    if has_nul(payload) {
        return Err("the job payload contains a NUL character".to_owned());
    }
    Ok(())
}

/// A job that a hook asked for, waiting to be written to the job table with the document change.
#[derive(Debug, Clone, PartialEq)]
#[non_exhaustive]
pub struct EnqueuedJob {
    name: String,
    payload: serde_json::Value,
}

impl EnqueuedJob {
    /// The full job name, `<app>.<job>`.
    pub fn name(&self) -> &str {
        &self.name
    }

    /// The payload.
    pub fn payload(&self) -> &serde_json::Value {
        &self.payload
    }
}

/// Collects the jobs that the hooks of one operation enqueue. `forgebase-db` makes one per
/// operation, gives it to every hook call through [`HookContext::with_jobs`] and, after the
/// hooks and before the commit, writes what it holds to the job table in the operation's
/// transaction. An app's unit test can make one to see what a hook enqueued.
///
/// It is `Sync` (a mutex inside), so that the futures of the database code stay `Send`.
#[derive(Debug, Default)]
#[non_exhaustive]
pub struct EnqueuedJobs {
    jobs: Mutex<Vec<EnqueuedJob>>,
}

impl EnqueuedJobs {
    /// An empty collector.
    pub fn new() -> Self {
        Self::default()
    }

    /// Takes the collected jobs, in the order they were enqueued, and leaves the collector
    /// empty.
    pub fn take(&self) -> Vec<EnqueuedJob> {
        std::mem::take(&mut *self.lock())
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, Vec<EnqueuedJob>> {
        // A hook that panicked while holding the lock cannot leave a half-written list behind
        // (a push is the only thing done under it), so a poisoned lock is still usable.
        self.jobs
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }
}

/// The operation a hook runs for. New operations may be added later, so match with a wildcard
/// arm.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum Operation {
    /// A new document is being inserted.
    Insert,
    /// An existing document is being updated.
    Update,
    /// An existing document is being deleted.
    Delete,
    /// A draft of a submittable DocType is being submitted.
    Submit,
    /// A submitted document is being cancelled.
    Cancel,
    /// A cancelled document is being amended: a new draft is created from a copy of it.
    Amend,
}

/// What a hook is told about the operation, besides the document.
///
/// `#[non_exhaustive]`: more information may be added later, so other crates cannot build it with
/// a struct literal; use [`HookContext::new`] (for example in a test that calls a hook directly).
/// The fields are public to read.
///
/// It is `Copy`; it holds a reference to the collector of the jobs the hooks enqueue (see
/// [`HookContext::enqueue`]), which equality compares by identity.
#[derive(Debug, Clone, Copy)]
#[non_exhaustive]
pub struct HookContext<'a> {
    /// The name of the DocType.
    pub doctype: &'a str,
    /// The name of the caller of the operation ([`Caller::name`](crate::permission::Caller::name)):
    /// the username of a user, or the actor of the trusted system context. Set for every
    /// operation, including [`Operation::Delete`]; insert and update also store it, delete only
    /// gives it to the hooks (there is no audit trail yet). By the time a hook runs the
    /// caller's permissions have been checked (ADR 0011); hooks do not see the roles.
    pub actor: &'a str,
    /// The operation.
    pub operation: Operation,
    jobs: Option<&'a EnqueuedJobs>,
}

impl PartialEq for HookContext<'_> {
    fn eq(&self, other: &Self) -> bool {
        self.doctype == other.doctype
            && self.actor == other.actor
            && self.operation == other.operation
            && match (self.jobs, other.jobs) {
                (None, None) => true,
                (Some(a), Some(b)) => std::ptr::eq(a, b),
                _ => false,
            }
    }
}

impl Eq for HookContext<'_> {}

impl<'a> HookContext<'a> {
    /// A context for `operation` on `doctype` by `actor`. The framework builds these for the hooks
    /// it calls; an app needs this only to call a hook directly in a test.
    pub const fn new(doctype: &'a str, actor: &'a str, operation: Operation) -> Self {
        Self {
            doctype,
            actor,
            operation,
            jobs: None,
        }
    }

    /// This context with a collector for the jobs a hook enqueues. The framework gives every
    /// hook call of an operation the same collector; an app's unit test uses it to see what a
    /// hook enqueued.
    pub const fn with_jobs(mut self, jobs: &'a EnqueuedJobs) -> Self {
        self.jobs = Some(jobs);
        self
    }

    /// Asks for a background job `name` (`<app>.<job>`) with `payload`. The job is written to
    /// the job table in the same transaction as the document change, after all hooks have run,
    /// so it exists if and only if the change is committed (the transactional outbox, ADR
    /// 0013). An unknown job name fails the whole operation when the job is written. Never put
    /// secrets in a payload: it is stored in clear text.
    ///
    /// Fails if the payload is larger than [`MAX_JOB_PAYLOAD_BYTES`] bytes of JSON, or when this
    /// context has no collector (a context made by [`HookContext::new`] in a test: add one with
    /// [`HookContext::with_jobs`]).
    pub fn enqueue(&self, name: &str, payload: serde_json::Value) -> Result<(), HookError> {
        let Some(jobs) = self.jobs else {
            return Err(HookError::new(
                "jobs cannot be enqueued here: this HookContext has no job collector (use HookContext::with_jobs in a test)",
            ));
        };
        check_job_payload(&payload).map_err(HookError::new)?;
        jobs.lock().push(EnqueuedJob {
            name: name.to_owned(),
            payload,
        });
        Ok(())
    }
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
pub trait DocTypeHooks: Send + Sync {
    /// Runs first when a document is inserted, on the input document, which may be modified.
    /// The document is not validated yet.
    fn before_insert(&self, ctx: &HookContext<'_>, doc: &mut Document) -> Result<(), HookError> {
        let _ = (ctx, doc);
        Ok(())
    }

    /// Runs on insert (after `before_insert`), on update, on submit (before `before_submit`) and
    /// on amend (after `before_insert`). The document may be modified. It is not validated yet
    /// by the framework.
    ///
    /// On update `doc` is the **whole** document: the stored values with the changes applied,
    /// and the rows of every table (the stored ones for a table the changes do not mention).
    /// `doc.name` is the stored name; changing the naming field is rejected afterwards. A value
    /// a hook removes from `doc.values` is cleared (as on insert, an absent value is null); to
    /// remove all rows of a table set an empty list, because a table key that is removed from
    /// `doc.tables` keeps the stored rows. (The exception: for a required Table field the
    /// framework validation then reports `Required`, since the document has no rows for it.)
    ///
    /// Changing `doc.name` is ignored on update (the stored name is kept; the naming field is
    /// checked as said above). On insert the name is decided by the naming rule afterwards, and
    /// a `doc.name` that does not agree with it is rejected with `Naming`.
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

    /// Runs when a draft is submitted, after `validate`. `doc` is the **whole** stored document
    /// (values and the rows of every table) and may be modified, like in `validate` (it takes
    /// `&mut` because a submit hook typically finalizes values, for example a total); the
    /// framework validates the result afterwards, and the changes are written together with the
    /// new docstatus. Changing `doc.name` is ignored.
    fn before_submit(&self, ctx: &HookContext<'_>, doc: &mut Document) -> Result<(), HookError> {
        let _ = (ctx, doc);
        Ok(())
    }

    /// Runs before a submitted document is cancelled. `doc` is a read-only view of the stored
    /// document, including its table rows (like `before_delete`: a veto, not a modification).
    fn before_cancel(&self, ctx: &HookContext<'_>, doc: &Document) -> Result<(), HookError> {
        let _ = (ctx, doc);
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    #[test]
    fn a_hook_enqueues_into_the_collector_in_order() {
        let jobs = EnqueuedJobs::new();
        let ctx = HookContext::new("customer", "ann", Operation::Insert).with_jobs(&jobs);
        ctx.enqueue("crm.welcome", json!({"n": 1}))
            .expect("enqueued");
        // The context is `Copy`: a copy shares the collector.
        let copy = ctx;
        copy.enqueue("crm.audit", json!(null)).expect("enqueued");
        let taken = jobs.take();
        assert_eq!(
            taken.iter().map(EnqueuedJob::name).collect::<Vec<_>>(),
            ["crm.welcome", "crm.audit"]
        );
        assert_eq!(taken[0].payload(), &json!({"n": 1}));
        assert!(jobs.take().is_empty(), "take leaves the collector empty");
        assert_eq!(ctx, copy);
    }

    #[test]
    fn a_context_without_a_collector_refuses_to_enqueue() {
        let ctx = HookContext::new("customer", "ann", Operation::Insert);
        let error = ctx.enqueue("crm.welcome", json!({})).expect_err("refused");
        assert!(error.message().contains("no job collector"), "{error}");
        assert_ne!(ctx, ctx.with_jobs(&EnqueuedJobs::new()));
    }

    #[test]
    fn the_payload_limit_counts_serialized_json() {
        let fits = json!({"text": "x".repeat(MAX_JOB_PAYLOAD_BYTES - 20)});
        assert!(check_job_payload(&fits).is_ok());
        let too_big = json!({"text": "x".repeat(MAX_JOB_PAYLOAD_BYTES)});
        assert!(
            check_job_payload(&too_big)
                .unwrap_err()
                .contains("larger than")
        );
        assert!(check_job_payload(&json!({"a": ["b\u{0}"]})).is_err());
        assert!(check_job_payload(&json!({"a\u{0}": 1})).is_err());
        assert!(check_job_payload(&json!("a\\u0000")).is_ok());
    }
}
