//! The error type of `forgebase-db`.

use std::error::Error as _;
use std::fmt::Write as _;

use tokio_postgres::error::SqlState;

use forgebase_core::document::ValidationError;
use forgebase_core::ident::TABLE_PREFIX;
use forgebase_core::meta::{DefinitionError, DocType, FieldType};

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
    /// Schema sync needs the framework tables, which are not there yet.
    #[error("framework tables are missing; run the framework migrations before schema sync")]
    FrameworkNotMigrated,
    /// The metadata cannot be applied without a destructive or risky change; nothing was applied.
    #[error("schema sync refused, {} conflict(s): {}", .0.len(), join(.0))]
    SyncConflicts(Vec<Conflict>),
    /// The DocTypes as a whole are inconsistent, for example a Link field names a DocType that
    /// is not in the registry ([`forgebase_core::meta::Registry::check_references`]). Nothing
    /// was changed.
    #[error("invalid DocType metadata: {}", join(.0))]
    InvalidMetadata(Vec<DefinitionError>),
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
        /// DocType of the linking documents, when it can be derived from the error.
        referenced_by: Option<String>,
    },
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
    /// the violated foreign key, `doc_<doctype>`), so no further query is needed; that matters
    /// inside a caller's transaction, which a failed statement leaves aborted.
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
