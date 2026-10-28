//! The error type of `forgebase-db`.

use std::error::Error as _;
use std::fmt::Write as _;

use tokio_postgres::error::SqlState;

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

fn join(conflicts: &[Conflict]) -> String {
    let parts: Vec<String> = conflicts.iter().map(ToString::to_string).collect();
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
