//! The error type of `forgebase-db`.

use std::error::Error as _;
use std::fmt::Write as _;

use tokio_postgres::error::SqlState;

use forgebase_core::document::{DocStatus, ValidationError};
use forgebase_core::ident::TABLE_PREFIX;
use forgebase_core::meta::{DefinitionError, DocType, FieldType};
use forgebase_core::permission::Right;

use crate::plan::Conflict;

/// Everything that can go wrong in `forgebase-db`.
#[derive(Debug, thiserror::Error)]
pub enum DbError {
    /// A PostgreSQL or connection error. The message includes the server's message and SQLSTATE
    /// (`tokio_postgres::Error` alone only displays "db error").
    #[error("{}", describe(.0))]
    Postgres(tokio_postgres::Error),
    /// A DDL statement waited longer than the sync `lock_timeout` (SQLSTATE 55P03).
    #[error(
        "timed out waiting for a database lock (another transaction holds a conflicting lock); the statement that timed out was rolled back (earlier committed steps, such as framework migrations in `migrate`, stay applied), retry later"
    )]
    LockTimeout,
    /// `create_database` cannot use the URL it was given. Never contains the URL.
    #[error("{0}")]
    CreateDatabase(String),
    /// The TLS settings of the database URL (`sslmode`, `sslrootcert`, ADR 0018) cannot be used,
    /// or the roots cannot be loaded. Never contains the URL or the password.
    #[error("{0}")]
    Tls(String),
    /// `pg_environment` or `inspect_database` cannot use the URL or the server's answer. Never
    /// contains the URL or a value of it.
    #[error("{0}")]
    Backup(String),
    /// The connection pool could not be created (for example an invalid database URL).
    #[error("cannot create connection pool: {0}")]
    CreatePool(#[from] deadpool_postgres::CreatePoolError),
    /// No connection could be obtained from the pool.
    #[error("cannot get a database connection: {0}")]
    Pool(deadpool_postgres::PoolError),
    /// An applied framework migration no longer matches the embedded SQL.
    #[error(
        "framework migration {version} ({name}) was changed after it was applied: expected checksum {expected}, database has {actual}"
    )]
    ChecksumMismatch {
        /// Migration version.
        version: i32,
        /// Migration name.
        name: String,
        /// Checksum of the embedded SQL.
        expected: String,
        /// Checksum recorded in the database.
        actual: String,
    },
    /// The database has a migration this build does not know (the database is newer than the code).
    #[error(
        "database has framework migration {version} ({name}) that this version of Forgebase does not know; upgrade Forgebase"
    )]
    UnknownMigration {
        /// Migration version.
        version: i32,
        /// Migration name as recorded in the database.
        name: String,
    },
    /// An applied app migration no longer matches the SQL the app declares now.
    #[error(
        "migration {version} ({name}) of app `{app}` was changed after it was applied: expected checksum {expected}, database has {actual}"
    )]
    AppChecksumMismatch {
        /// App name.
        app: String,
        /// Migration version.
        version: i32,
        /// Migration name.
        name: String,
        /// Checksum of the SQL the app declares.
        expected: String,
        /// Checksum recorded in the database.
        actual: String,
    },
    /// The database has an app migration that the app does not declare any more.
    #[error(
        "database has migration {version} ({name}) of app `{app}` that the app does not declare; the database is newer than the app, or a migration was removed"
    )]
    UnknownAppMigration {
        /// App name.
        app: String,
        /// Migration version.
        version: i32,
        /// Migration name as recorded in the database.
        name: String,
    },
    /// An app migration is not applied but a migration with a higher version of the same app
    /// is: new migrations must have versions above every applied one.
    #[error(
        "migration {version} ({name}) of app `{app}` is not applied, but migration {applied_version} of the app is; a new migration must have a version above every applied one (renumber it above {applied_version}); nothing was applied"
    )]
    OutOfOrderAppMigration {
        /// App name.
        app: String,
        /// Version of the pending migration.
        version: i32,
        /// Name of the pending migration.
        name: String,
        /// The highest applied version of the app.
        applied_version: i32,
    },
    /// An app migration failed. It was rolled back; the migrations before it stay applied.
    #[error(
        "migration {version} ({name}) of app `{app}` failed and was rolled back (earlier migrations stay applied): {message}"
    )]
    AppMigrationFailed {
        /// App name.
        app: String,
        /// Migration version.
        version: i32,
        /// Migration name.
        name: String,
        /// The database error.
        message: String,
    },
    /// Schema sync needs the framework tables, which are not there yet.
    #[error("framework tables are missing; run the framework migrations before schema sync")]
    FrameworkNotMigrated,
    /// The metadata cannot be applied without a destructive or risky change; nothing was applied.
    #[error("schema sync refused, {} conflict(s): {}", .0.len(), join(.0))]
    SyncConflicts(Vec<Conflict>),
    /// The DocTypes as a whole are inconsistent, for example a Link field names a DocType that
    /// is not in the registry, or a child DocType is used by no (or several) Table fields
    /// ([`forgebase_core::meta::Registry::check_references`]). Nothing was changed.
    #[error("invalid DocType metadata: {}", join(.0))]
    InvalidMetadata(Vec<DefinitionError>),
    /// The DocType name is not in the registry.
    #[error("unknown DocType `{name}`")]
    UnknownDocType {
        /// The name that was asked for.
        name: String,
    },
    /// An operation was called with a child DocType: its rows are read and
    /// written through the Table field of its parent's documents only.
    #[error(
        "DocType `{doctype}` is a child DocType: its rows belong to a parent document and are read and written through the parent"
    )]
    ChildDocType {
        /// The child DocType name.
        doctype: String,
    },
    /// The rows of a document changed several times while it was being read, so no consistent
    /// copy could be read. Retry.
    #[error("{doctype} `{name}` was changed while it was being read; retry")]
    ChangedWhileReading {
        /// DocType name.
        doctype: String,
        /// Document name.
        name: String,
    },
    /// The document or the changes break the DocType's rules; every problem is listed.
    #[error("validation failed: {}", join(.0))]
    Validation(Vec<ValidationError>),
    /// No document of this DocType has this name.
    #[error("{doctype} `{name}` not found")]
    NotFound {
        /// DocType name.
        doctype: String,
        /// Document name.
        name: String,
    },
    /// The document changed since the caller read it: `expected_version` is not the current
    /// `version`. Read the document again and retry.
    #[error(
        "{doctype} `{name}` was changed by someone else: expected version {expected}, current version {current}"
    )]
    Conflict {
        /// DocType name.
        doctype: String,
        /// Document name.
        name: String,
        /// The version the caller expected.
        expected: i64,
        /// The version in the database.
        current: i64,
    },
    /// The document cannot be deleted because other documents link to it (SQLSTATE 23503,
    /// `ON DELETE RESTRICT`).
    #[error("{doctype} `{name}` cannot be deleted: it is linked from {}", referenced_by.as_deref().unwrap_or("another document"))]
    LinkedDocumentExists {
        /// DocType name of the document that was to be deleted.
        doctype: String,
        /// Its name.
        name: String,
        /// DocType of the linking documents. `None` when it cannot be derived from the error, and
        /// also when the caller has no read access to that DocType: it is not named then, so the
        /// error does not tell the caller about documents they cannot see (ADR 0011).
        referenced_by: Option<String>,
    },
    /// The caller lacks a right. For a DocType the caller has no such right on at all, or, for
    /// an `if_owner` right, on a document of somebody else that the caller can read. It never
    /// names a document and is never returned for a document the caller cannot read (that is
    /// [`DbError::NotFound`], so a hidden document is not told from a missing one).
    #[error("permission denied: no `{right}` right on DocType `{doctype}`")]
    PermissionDenied {
        /// DocType name.
        doctype: String,
        /// The right that was needed.
        right: Right,
    },
    /// A list query is not valid: an unknown or unsuitable field, an operator that does not fit
    /// the field, a value of the wrong kind, or a limit or offset out of range. The message names
    /// the problem; nothing from the query was sent to the database.
    #[error("invalid list query: {0}")]
    InvalidQuery(String),
    /// A unique constraint (a `unique` field or the document name) was violated (SQLSTATE 23505).
    /// The message is the server's, including the constraint name and the conflicting key.
    #[error("{message}")]
    UniqueViolation {
        /// The server's message and detail.
        message: String,
    },
    /// The actor (user name recorded in `owner` and `modified_by`) is empty or contains NUL.
    #[error("actor must be a non-empty string without NUL characters")]
    InvalidActor,
    /// The document name conflicts with the DocType's naming rule, or an update would change it.
    #[error("{0}")]
    Naming(String),
    /// A stored value could not be read. Only another writer (for example `psql`) can produce
    /// such a value: dates and datetimes outside the years supported by the `time` crate (beyond
    /// ±9999), `infinity` and numeric `NaN` cannot be decoded.
    #[error("cannot read field `{field}` (column \"{field}\") of DocType `{doctype}`: {message}")]
    Decode {
        /// DocType name.
        doctype: String,
        /// Field name (also the column name).
        field: String,
        /// The decoder's message.
        message: String,
    },
    /// An update was called without any change.
    #[error("update needs at least one changed field")]
    NoChanges,
    /// The operation is not allowed in the document's docstatus. Submit needs a draft, cancel a
    /// submitted document, amend a cancelled one; update works on a draft (and on a submitted one
    /// for `allow_on_submit` fields, see [`DbError::FieldLocked`]) but not on a cancelled one;
    /// delete works on a draft or a cancelled document but not on a submitted one.
    #[error("cannot {operation} {doctype} `{name}`: it is {docstatus}")]
    InvalidDocStatus {
        /// DocType name.
        doctype: String,
        /// Document name.
        name: String,
        /// The document's current docstatus.
        docstatus: DocStatus,
        /// The refused operation: `update`, `delete`, `submit`, `cancel` or `amend`.
        operation: &'static str,
    },
    /// Submit, cancel or amend was called for a DocType that is not `submittable`.
    #[error("cannot {operation}: DocType `{doctype}` is not submittable (`submittable = true`)")]
    NotSubmittable {
        /// DocType name.
        doctype: String,
        /// The refused operation: `submit`, `cancel` or `amend`.
        operation: &'static str,
    },
    /// A change to a submitted document touches a field that is not `allow_on_submit` (or a
    /// table): submitted documents are locked. Raised for the caller's changes and for changes
    /// made by a hook.
    #[error(
        "{doctype} `{name}` is submitted: field `{field}` cannot be changed (only fields marked allow_on_submit can; tables are locked)"
    )]
    FieldLocked {
        /// DocType name.
        doctype: String,
        /// Document name.
        name: String,
        /// The locked field.
        field: String,
    },
    /// The cancelled document was already amended: it has at most one amendment at a time.
    #[error("{doctype} `{name}` was already amended by `{amended_by}`")]
    AlreadyAmended {
        /// DocType name.
        doctype: String,
        /// The cancelled document.
        name: String,
        /// The document that amends it.
        amended_by: String,
    },
    /// Submit or cancel was called for a DocType that has a workflow: its documents change state
    /// only through a transition (ADR 0015).
    #[error("DocType `{doctype}` has a workflow; use a transition (cannot {operation})")]
    WorkflowRequired {
        /// DocType name.
        doctype: String,
        /// The refused operation: `submit` or `cancel`.
        operation: &'static str,
    },
    /// A transition (or its list or history) was asked for on a DocType without a workflow.
    #[error("DocType `{doctype}` has no workflow")]
    NoWorkflow {
        /// DocType name.
        doctype: String,
    },
    /// The document's state has no transition with this action (the action does not exist, or
    /// not from this state). A document without a state has no transitions either.
    #[error("{doctype} `{name}` is in state `{state}`: no transition `{action}` from it")]
    UnknownTransition {
        /// DocType name.
        doctype: String,
        /// Document name.
        name: String,
        /// The state the document is in (`(none)` when it has no state).
        state: String,
        /// The action that was asked for.
        action: String,
    },
    /// The caller may not take the transition: it needs one of the transition's roles and the
    /// right of its docstatus move (write, submit or cancel), and for an `if_owner` right the
    /// document must be the caller's. Like [`DbError::PermissionDenied`] it is only returned
    /// for a document the caller can read.
    #[error(
        "permission denied: transition `{action}` of DocType `{doctype}` needs one of its roles and the `{right}` right"
    )]
    TransitionDenied {
        /// DocType name.
        doctype: String,
        /// The action.
        action: String,
        /// The right the transition's docstatus move needs.
        right: Right,
    },
    /// The document is in a state whose `edit_roles` the caller does not have, so it cannot
    /// be updated (ADR 0015).
    #[error(
        "permission denied: a `{doctype}` document in state `{state}` can be edited only by the roles of that state"
    )]
    EditDenied {
        /// DocType name.
        doctype: String,
        /// The state the document is in.
        state: String,
    },
    /// A saved view cannot be written: the label or the definition is not valid, or a limit
    /// is reached (M8.3). The message says what to change.
    #[error("invalid saved view: {0}")]
    InvalidView(String),
    /// A view with this label already exists (a private one of the same user, or a shared one),
    /// for the DocType.
    #[error("a view named {label:?} already exists for DocType `{doctype}`")]
    ViewExists {
        /// DocType name.
        doctype: String,
        /// The label.
        label: String,
    },
    /// The caller may not do this with a saved view that they can see: create or change a
    /// shared view without being an Administrator. Never returned for another user's private
    /// view (that one is [`DbError::NotFound`]).
    #[error("permission denied: {0}")]
    ViewDenied(&'static str),
    /// A job cannot be enqueued, or a job command is wrong: the job name is not one of the
    /// binary's jobs, the payload is too large or has a NUL character, an id is not a UUID. For
    /// a hook that enqueues it fails the whole operation, and nothing is written. It is a
    /// mistake of the app (or the operator), not of the caller of an API, so the HTTP API
    /// reports it as an internal error.
    #[error("job: {0}")]
    Job(String),
    /// The report does not exist, or the caller may not run it (the same error for both, so a
    /// caller learns nothing about reports they may not run).
    #[error("report `{report}` not found")]
    ReportNotFound {
        /// The full name asked for.
        report: String,
    },
    /// A report ran over its statement timeout, or waited too long for a lock. Nothing was
    /// changed. The caller can narrow the parameters.
    #[error("report `{report}` took too long and was stopped")]
    ReportTimeout {
        /// The full name.
        report: String,
    },
    /// A report's SQL tried to write (SQLSTATE 25006): a mistake of the app, not of the caller.
    /// Nothing was changed. The HTTP API reports it as an internal error.
    #[error("report `{report}` tried to write, which a report may not do")]
    ReportReadOnly {
        /// The full name.
        report: String,
    },
    /// The statement of a report does not have the declared parameters or columns: a mistake of
    /// the app. The message never contains the SQL.
    #[error("{0}")]
    Report(String),
    /// `forgebase migrate` found reports whose SQL does not fit what they declare, after the
    /// migrations were applied. One message per problem.
    #[error("report check failed ({} problem(s))", .0.len())]
    ReportCheck(Vec<String>),
    /// A lifecycle hook ([`forgebase_core::hooks::DocTypeHooks`]) rejected the operation. The
    /// whole operation was rolled back: nothing was persisted.
    #[error("{hook} hook of DocType `{doctype}` rejected the operation: {message}")]
    Hook {
        /// DocType name.
        doctype: String,
        /// The hook that failed: `before_insert`, `validate`, `before_delete`, `before_submit`,
        /// `before_cancel` or `before_transition`.
        hook: &'static str,
        /// The hook's message.
        message: String,
    },
}

impl From<tokio_postgres::Error> for DbError {
    fn from(error: tokio_postgres::Error) -> Self {
        if error.code() == Some(&SqlState::LOCK_NOT_AVAILABLE) {
            DbError::LockTimeout
        } else {
            DbError::Postgres(error)
        }
    }
}

impl From<deadpool_postgres::PoolError> for DbError {
    /// A failed connection attempt keeps the server's message and SQLSTATE (for example
    /// 28P01 for a wrong password, 3D000 for a missing database); other pool errors keep
    /// their own message.
    fn from(error: deadpool_postgres::PoolError) -> Self {
        match error {
            deadpool_postgres::PoolError::Backend(error) => error.into(),
            other => DbError::Pool(other),
        }
    }
}

impl DbError {
    /// True when the server refused the connection because the database does not exist
    /// (SQLSTATE 3D000).
    pub fn is_missing_database(&self) -> bool {
        matches!(self, DbError::Postgres(e) if e.code() == Some(&SqlState::INVALID_CATALOG_NAME))
    }
}

impl DbError {
    /// Converts an error of an `INSERT` or `UPDATE` on a DocType table: a unique violation
    /// (SQLSTATE 23505) becomes [`DbError::UniqueViolation`].
    ///
    /// For a `unique` Link field the server's detail would show the internal id of the target
    /// document, which users never see elsewhere; the message names the field instead.
    pub(crate) fn from_write(error: tokio_postgres::Error, doctype: &DocType) -> Self {
        if error.code() == Some(&SqlState::UNIQUE_VIOLATION) {
            let link_field = error
                .as_db_error()
                .and_then(|db| db.detail())
                .and_then(|detail| detail.strip_prefix("Key ("))
                .and_then(|rest| rest.split_once(")=("))
                .and_then(|(column, _)| doctype.field(column))
                .filter(|field| matches!(field.field_type, FieldType::Link { .. }));
            let message = match link_field {
                Some(field) => format!(
                    "a `{}` document with this `{}` already exists (unique field `{}`)",
                    doctype.name, field.name, field.name
                ),
                None => describe(&error),
            };
            DbError::UniqueViolation { message }
        } else {
            error.into()
        }
    }
}

impl DbError {
    /// Converts an error of a `DELETE` on a DocType table: a foreign key violation (SQLSTATE
    /// 23503, a Link still points at the document) becomes [`DbError::LinkedDocumentExists`].
    ///
    /// The referencing DocType comes from the table the server names in the error (the table of
    /// the violated foreign key, `doc_<doctype>`), so no further query is needed. The caller maps
    /// a child table to the DocType that owns it.
    pub(crate) fn from_delete(error: tokio_postgres::Error, doctype: &str, name: &str) -> Self {
        if error.code() == Some(&SqlState::FOREIGN_KEY_VIOLATION) {
            let referenced_by = error
                .as_db_error()
                .and_then(|db| db.table())
                .and_then(|table| table.strip_prefix(TABLE_PREFIX))
                .map(str::to_owned);
            DbError::LinkedDocumentExists {
                doctype: doctype.to_owned(),
                name: name.to_owned(),
                referenced_by,
            }
        } else {
            error.into()
        }
    }
}

impl DbError {
    /// A field value that cannot be decoded.
    pub(crate) fn decode(doctype: &str, field: &str, error: &tokio_postgres::Error) -> Self {
        DbError::Decode {
            doctype: doctype.to_owned(),
            field: field.to_owned(),
            message: describe(error),
        }
    }
}

fn join<T: ToString>(items: &[T]) -> String {
    let parts: Vec<String> = items.iter().map(ToString::to_string).collect();
    parts.join("; ")
}

fn describe(error: &tokio_postgres::Error) -> String {
    if let Some(db) = error.as_db_error() {
        let mut text = format!(
            "{}: {} (SQLSTATE {})",
            db.severity(),
            db.message(),
            db.code().code()
        );
        if let Some(detail) = db.detail() {
            let _ = write!(text, "; detail: {detail}");
        }
        return text;
    }
    let mut text = error.to_string();
    let mut source = error.source();
    while let Some(cause) = source {
        let _ = write!(text, ": {cause}");
        source = cause.source();
    }
    text
}
