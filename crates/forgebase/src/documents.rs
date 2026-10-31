//! [`Documents`]: what a job or a method may do with documents, [`DocError`], and the small
//! types around them.
//!
//! A job runs as the trusted **system context** (`Caller::System`, no permission rules) with the
//! actor `job:<app>.<job>`, which is recorded as `owner` / `modified_by` and given to hooks. A
//! method (ADR 0019) runs as the **caller**: a `Caller::User` built from the authenticated
//! request, so the permission evaluator of ADR 0011 decides every call. Either way every call
//! uses the same document functions as the CLI and the HTTP API, so validation, naming and
//! the lifecycle hooks run as usual; each write (and each enqueue) is one **savepoint** inside
//! the surrounding transaction, so a refused write leaves it usable and a refused or failed job
//! or method rolls all of them back. Reads (`get`, `list`) run directly on the transaction: a
//! failed read aborts it, so the attempt fails (a job is retried). Jobs enqueued by the hooks
//! of these calls are written in the same transaction.

use std::fmt;
use std::future::Future;
use std::pin::Pin;

use forgebase_core::document::{Changes, DocStatus, Document};
use forgebase_core::meta::Registry;
use forgebase_core::permission::Caller;
use forgebase_core::value::Value;
use forgebase_db::{DbError, Transaction};

use crate::jobs::JobError;

/// Why a [`Documents`] call failed: the database function's own error, wrapped.
///
/// - In a **job**, `?` turns it into a [`JobError`] exactly as before: a database or connection
///   error is retried, every refusal (validation, permission, hook veto, not found, version
///   conflict, unique violation, an invalid list query) is permanent.
/// - In a **method**, `?` turns it into a [`MethodError`](crate::MethodError) with the status
///   and code the HTTP API gives the same error (`permission_denied` 403, `not_found` 404,
///   `conflict` 409, `validation_failed` 422, ...); an internal or database error is a generic
///   500 and nothing of it is sent to the client.
///
/// The [`Display`](fmt::Display) text is meant for the server's log and a job's `last_error`:
/// it can name a constraint or a server message, so do not copy it into a message for users.
#[derive(Debug)]
pub struct DocError {
    error: DbError,
}

impl DocError {
    pub(crate) fn new(error: DbError) -> Self {
        Self { error }
    }

    /// The database error, for the dispatcher of methods.
    pub(crate) fn into_db(self) -> DbError {
        self.error
    }

    /// Whether the same call may work if it is tried again later: a database or connection
    /// error. A refusal (validation, permission, hook veto, not found, conflict, ...) is not
    /// retryable.
    pub fn is_retryable(&self) -> bool {
        !JobError::from_db(&self.error).is_permanent()
    }
}

impl fmt::Display for DocError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&crate::error::describe_db(&self.error))
    }
}

impl std::error::Error for DocError {}

impl From<DocError> for JobError {
    /// Retryable errors become [`JobError::new`], all others [`JobError::permanent`].
    fn from(error: DocError) -> Self {
        JobError::from_db(&error.error)
    }
}

type Fut<'f, T> = Pin<Box<dyn Future<Output = Result<T, DbError>> + Send + 'f>>;

/// A document as a job reads it. Read the fields with the getters; the type is
/// `#[non_exhaustive]` so that more can be added.
#[derive(Debug, Clone, PartialEq)]
#[non_exhaustive]
pub struct StoredDocument {
    name: String,
    version: i64,
    docstatus: Option<DocStatus>,
    document: Document,
}

impl StoredDocument {
    fn new(stored: forgebase_db::StoredDocument) -> Self {
        Self {
            name: stored.name.clone(),
            version: stored.version,
            docstatus: stored.docstatus,
            document: Document {
                name: Some(stored.name),
                values: stored.values,
                tables: stored.tables,
            },
        }
    }

    /// The document's name.
    pub fn name(&self) -> &str {
        &self.name
    }

    /// The version: pass it to `update`, `delete`, `submit`, `cancel` or `amend`.
    pub fn version(&self) -> i64 {
        self.version
    }

    /// The docstatus of a document of a submittable DocType; `None` for any other.
    pub fn docstatus(&self) -> Option<DocStatus> {
        self.docstatus
    }

    /// The values and the table rows. A Link value is the target document's name. (A list does
    /// not return table rows.)
    pub fn document(&self) -> &Document {
        &self.document
    }
}

/// How a [`ListQuery`] filter compares a field with a value.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum FilterOp {
    /// `=`.
    Eq,
    /// `<>`.
    Ne,
    /// `<`.
    Lt,
    /// `<=`.
    Le,
    /// `>`.
    Gt,
    /// `>=`.
    Ge,
    /// A SQL `LIKE` pattern (`%` and `_`); the value is text, and so must be the field.
    Like,
}

/// A query for [`Documents::list`]: filters (all must match), sort keys and a page size. Built
/// with the methods below. The field and value rules are those of `forgebase doc list`: the value
/// has the kind of the field and is never `Null`; an unknown field is an error.
///
/// `name` is always the last sort key. The default page is 20 documents and at most 500 are
/// returned; to go through more, sort by a field and filter on the last value seen.
#[derive(Debug, Clone, PartialEq, Default)]
#[non_exhaustive]
pub struct ListQuery {
    filters: Vec<(String, FilterOp, Value)>,
    order: Vec<(String, bool)>,
    limit: Option<u32>,
}

impl ListQuery {
    /// A query without filters, in name order.
    pub fn new() -> Self {
        Self::default()
    }

    /// Adds a condition on a field (a field of the DocType, or `name`, `owner`, `created_at`,
    /// `modified_at`, `modified_by`, `version`).
    pub fn filter(mut self, field: &str, op: FilterOp, value: Value) -> Self {
        self.filters.push((field.to_owned(), op, value));
        self
    }

    /// Adds a sort key; the first one added is the most significant.
    pub fn order_by(mut self, field: &str, descending: bool) -> Self {
        self.order.push((field.to_owned(), descending));
        self
    }

    /// Sets the page size.
    pub fn limit(mut self, limit: u32) -> Self {
        self.limit = Some(limit);
        self
    }

    fn to_db(&self) -> Result<forgebase_db::ListQuery, DocError> {
        use forgebase_db::{Filter, FilterOp as Db, Sort};
        let mut filters = Vec::new();
        for (field, op, value) in &self.filters {
            let op = match (op, value) {
                (FilterOp::Eq, v) => Db::Eq(v.clone()),
                (FilterOp::Ne, v) => Db::Ne(v.clone()),
                (FilterOp::Lt, v) => Db::Lt(v.clone()),
                (FilterOp::Le, v) => Db::Le(v.clone()),
                (FilterOp::Gt, v) => Db::Gt(v.clone()),
                (FilterOp::Ge, v) => Db::Ge(v.clone()),
                (FilterOp::Like, Value::Text(pattern)) => Db::Like(pattern.clone()),
                (FilterOp::Like, _) => {
                    return Err(DocError::new(DbError::InvalidQuery(format!(
                        "the `like` filter on `{field}` needs a text value"
                    ))));
                }
            };
            filters.push(Filter::new(field.clone(), op));
        }
        Ok(forgebase_db::ListQuery {
            filters,
            sort: self
                .order
                .iter()
                .map(|(field, descending)| Sort {
                    field: field.clone(),
                    descending: *descending,
                })
                .collect(),
            limit: self.limit,
            offset: None,
        })
    }
}

/// A transaction as `Documents` uses it, with the lifetime of the transaction erased, so that
/// the public type has one lifetime parameter.
trait DocTx: Send {
    fn get<'f>(
        &'f mut self,
        r: &'f Registry,
        doctype: &'f str,
        name: &'f str,
        c: &'f Caller,
    ) -> Fut<'f, Option<forgebase_db::StoredDocument>>;
    fn list<'f>(
        &'f mut self,
        r: &'f Registry,
        doctype: &'f str,
        c: &'f Caller,
        q: &'f forgebase_db::ListQuery,
    ) -> Fut<'f, Vec<forgebase_db::StoredDocument>>;
    fn insert<'f>(
        &'f mut self,
        r: &'f Registry,
        doctype: &'f str,
        doc: Document,
        c: &'f Caller,
    ) -> Fut<'f, forgebase_db::StoredDocument>;
    fn update<'f>(
        &'f mut self,
        r: &'f Registry,
        at: At<'f>,
        changes: &'f Changes,
        c: &'f Caller,
    ) -> Fut<'f, forgebase_db::StoredDocument>;
    fn delete<'f>(&'f mut self, r: &'f Registry, at: At<'f>, c: &'f Caller) -> Fut<'f, ()>;
    fn step<'f>(
        &'f mut self,
        step: Step,
        r: &'f Registry,
        at: At<'f>,
        c: &'f Caller,
    ) -> Fut<'f, forgebase_db::StoredDocument>;
    fn workflow<'f>(
        &'f mut self,
        r: &'f Registry,
        at: At<'f>,
        action: &'f str,
        c: &'f Caller,
    ) -> Fut<'f, forgebase_db::StoredDocument>;
}

/// A document at a version.
#[derive(Clone, Copy)]
struct At<'f> {
    doctype: &'f str,
    name: &'f str,
    version: i64,
}

#[derive(Clone, Copy)]
enum Step {
    Submit,
    Cancel,
    Amend,
}

impl DocTx for Transaction<'_> {
    fn get<'f>(
        &'f mut self,
        r: &'f Registry,
        doctype: &'f str,
        name: &'f str,
        c: &'f Caller,
    ) -> Fut<'f, Option<forgebase_db::StoredDocument>> {
        Box::pin(forgebase_db::get(self, r, doctype, name, c))
    }
    fn list<'f>(
        &'f mut self,
        r: &'f Registry,
        doctype: &'f str,
        c: &'f Caller,
        q: &'f forgebase_db::ListQuery,
    ) -> Fut<'f, Vec<forgebase_db::StoredDocument>> {
        Box::pin(async move { forgebase_db::list(&*self, r, doctype, c, q).await })
    }
    fn insert<'f>(
        &'f mut self,
        r: &'f Registry,
        doctype: &'f str,
        doc: Document,
        c: &'f Caller,
    ) -> Fut<'f, forgebase_db::StoredDocument> {
        Box::pin(forgebase_db::insert(self, r, doctype, doc, c))
    }
    fn update<'f>(
        &'f mut self,
        r: &'f Registry,
        at: At<'f>,
        changes: &'f Changes,
        c: &'f Caller,
    ) -> Fut<'f, forgebase_db::StoredDocument> {
        Box::pin(forgebase_db::update(
            self, r, at.doctype, at.name, at.version, changes, c,
        ))
    }
    fn delete<'f>(&'f mut self, r: &'f Registry, at: At<'f>, c: &'f Caller) -> Fut<'f, ()> {
        Box::pin(forgebase_db::delete(
            self, r, at.doctype, at.name, at.version, c,
        ))
    }
    fn workflow<'f>(
        &'f mut self,
        r: &'f Registry,
        at: At<'f>,
        action: &'f str,
        c: &'f Caller,
    ) -> Fut<'f, forgebase_db::StoredDocument> {
        Box::pin(forgebase_db::transition(
            self, r, at.doctype, at.name, action, at.version, c,
        ))
    }
    fn step<'f>(
        &'f mut self,
        step: Step,
        r: &'f Registry,
        at: At<'f>,
        c: &'f Caller,
    ) -> Fut<'f, forgebase_db::StoredDocument> {
        match step {
            Step::Submit => Box::pin(forgebase_db::submit(
                self, r, at.doctype, at.name, at.version, c,
            )),
            Step::Cancel => Box::pin(forgebase_db::cancel(
                self, r, at.doctype, at.name, at.version, c,
            )),
            Step::Amend => Box::pin(forgebase_db::amend(
                self, r, at.doctype, at.name, at.version, c,
            )),
        }
    }
}

/// The documents a job or a method can reach. Get it from
/// [`JobContext::documents`](crate::JobContext::documents) (the system context) or
/// [`MethodContext::documents`](crate::MethodContext::documents) (the caller). Every method
/// returns `Result<_, DocError>`; see [`DocError`] for what a job and a method make of it.
///
/// The arguments are those of the matching `forgebase doc` command: the DocType name, the
/// document name, and for a change the `version` that was read (a document changed by someone
/// else in between is a version conflict, which is not retryable).
///
/// Every call acts as the caller the handle was made with, through the same functions as the
/// HTTP API, so the permission rules of ADR 0011 (`if_owner`, read rules on `get` and `list`,
/// read-only fields, workflow roles) are the only decision. A job's handle is the system
/// context, which no rule restricts.
pub struct Documents<'a> {
    tx: &'a mut dyn DocTx,
    registry: &'a Registry,
    caller: &'a Caller,
}

impl<'a> Documents<'a> {
    pub(crate) fn new(
        tx: &'a mut Transaction<'_>,
        registry: &'a Registry,
        caller: &'a Caller,
    ) -> Self {
        Self {
            tx,
            registry,
            caller,
        }
    }

    /// The document, or `None` when there is none with this name (or the caller may not read
    /// it).
    pub async fn get(
        &mut self,
        doctype: &str,
        name: &str,
    ) -> Result<Option<StoredDocument>, DocError> {
        let found = self.tx.get(self.registry, doctype, name, self.caller).await;
        Ok(found.map_err(DocError::new)?.map(StoredDocument::new))
    }

    /// A page of documents (only those the caller may read); their table rows are not listed.
    pub async fn list(
        &mut self,
        doctype: &str,
        query: &ListQuery,
    ) -> Result<Vec<StoredDocument>, DocError> {
        let query = query.to_db()?;
        let found = self
            .tx
            .list(self.registry, doctype, self.caller, &query)
            .await;
        Ok(found
            .map_err(DocError::new)?
            .into_iter()
            .map(StoredDocument::new)
            .collect())
    }

    /// Inserts a document and returns it as stored. Its hooks run.
    pub async fn insert(
        &mut self,
        doctype: &str,
        document: Document,
    ) -> Result<StoredDocument, DocError> {
        let stored = self
            .tx
            .insert(self.registry, doctype, document, self.caller)
            .await;
        stored.map(StoredDocument::new).map_err(DocError::new)
    }

    /// Changes a document that is at `version`, and returns it as stored.
    pub async fn update(
        &mut self,
        doctype: &str,
        name: &str,
        version: i64,
        changes: Changes,
    ) -> Result<StoredDocument, DocError> {
        let at = At {
            doctype,
            name,
            version,
        };
        let stored = self
            .tx
            .update(self.registry, at, &changes, self.caller)
            .await;
        stored.map(StoredDocument::new).map_err(DocError::new)
    }

    /// Deletes a document that is at `version`.
    pub async fn delete(
        &mut self,
        doctype: &str,
        name: &str,
        version: i64,
    ) -> Result<(), DocError> {
        let at = At {
            doctype,
            name,
            version,
        };
        let deleted = self.tx.delete(self.registry, at, self.caller).await;
        deleted.map_err(DocError::new)
    }

    /// Submits a draft of a submittable DocType and returns it.
    pub async fn submit(
        &mut self,
        doctype: &str,
        name: &str,
        version: i64,
    ) -> Result<StoredDocument, DocError> {
        self.step(Step::Submit, doctype, name, version).await
    }

    /// Cancels a submitted document and returns it.
    pub async fn cancel(
        &mut self,
        doctype: &str,
        name: &str,
        version: i64,
    ) -> Result<StoredDocument, DocError> {
        self.step(Step::Cancel, doctype, name, version).await
    }

    /// Amends a cancelled document: a new draft with a copy of its values, which is returned.
    pub async fn amend(
        &mut self,
        doctype: &str,
        name: &str,
        version: i64,
    ) -> Result<StoredDocument, DocError> {
        self.step(Step::Amend, doctype, name, version).await
    }

    /// Takes the workflow transition `action` of a document of a DocType with a workflow and
    /// returns the document as stored (`forgebase doc transition`). The caller needs one of the
    /// transition's roles and the right its docstatus move needs (ADR 0015).
    pub async fn transition(
        &mut self,
        doctype: &str,
        name: &str,
        action: &str,
        version: i64,
    ) -> Result<StoredDocument, DocError> {
        let at = At {
            doctype,
            name,
            version,
        };
        let stored = self
            .tx
            .workflow(self.registry, at, action, self.caller)
            .await;
        stored.map(StoredDocument::new).map_err(DocError::new)
    }

    async fn step(
        &mut self,
        step: Step,
        doctype: &str,
        name: &str,
        version: i64,
    ) -> Result<StoredDocument, DocError> {
        let at = At {
            doctype,
            name,
            version,
        };
        let stored = self.tx.step(step, self.registry, at, self.caller).await;
        stored.map(StoredDocument::new).map_err(DocError::new)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_doc_error_maps_to_a_job_error_as_the_database_error_did() {
        let retryable = DocError::new(DbError::LockTimeout);
        assert!(retryable.is_retryable());
        let job = JobError::from(retryable);
        assert!(!job.is_permanent());

        for error in [
            DbError::NoChanges,
            DbError::Validation(vec![]),
            DbError::NotFound {
                doctype: "a".into(),
                name: "b".into(),
            },
        ] {
            let error = DocError::new(error);
            assert!(!error.is_retryable(), "{error}");
            let text = error.to_string();
            let job = JobError::from(error);
            assert!(job.is_permanent());
            assert_eq!(job.message(), text);
        }
    }

    #[test]
    fn a_bad_like_filter_is_an_invalid_query_with_the_old_message() {
        let query = ListQuery::new().filter("title", FilterOp::Like, Value::Int(1));
        let error = query.to_db().expect_err("text only");
        assert!(!error.is_retryable());
        assert_eq!(
            error.to_string(),
            "invalid list query: the `like` filter on `title` needs a text value"
        );
        assert!(matches!(error.into_db(), DbError::InvalidQuery(_)));
    }
}
