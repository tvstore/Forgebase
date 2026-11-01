//! Import: many documents of one DocType in **one transaction**, each row inside a savepoint
//! (M9.3, ADR 0021).
//!
//! Every row goes through the normal pipeline as the caller: [`crate::insert`] (or, for an
//! existing document in [`ImportMode::Upsert`], [`crate::get`] and [`crate::update`]), so the
//! hooks, validation, naming, links, workflow rules, the per-row rights, the outbox jobs and the
//! webhooks of M9.2 all apply. A row that fails is rolled back to its savepoint and its error is
//! collected, so one bad row does not hide the others. When `commit` is true and no row failed,
//! the transaction commits; otherwise (a dry run, or any error) it is **rolled back**, and with
//! it every document, job and webhook delivery the rows wrote.
//!
//! **Rights.** The caller needs `import` and `create`, and `write` for an upsert. The rows check
//! their own rights as always.
//!
//! **Transaction length.** The whole import is one transaction: for the 10 000 rows of the
//! default limit it holds its locks (series counters, unique values) until the end. Run big
//! imports when the site is quiet.

use std::collections::BTreeMap;

use forgebase_core::document::{Changes, Document, Row};
use forgebase_core::meta::{Naming, Registry};
use forgebase_core::permission::{Caller, Right};
use forgebase_core::value::Value;
use tokio_postgres::GenericClient;

use crate::document::{access_to, check_actor, document_doctype};
use crate::error::DbError;

/// How many row errors an import reports in full; the rest is only counted.
pub const MAX_REPORTED_ERRORS: usize = 100;

/// Whether an import only inserts, or updates documents that exist.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ImportMode {
    /// Every row is a new document.
    Insert,
    /// Every row names a document (`name`): it is updated if it exists and inserted if not.
    Upsert,
}

/// One row of an import, already mapped to values.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct ImportRow {
    /// The 1-based number of the row among the data rows.
    pub row: usize,
    /// For a CSV file, the line of the file the row starts on.
    pub line: Option<u64>,
    /// The `name` given for the row, if any.
    pub name: Option<String>,
    /// The values the row gives.
    pub values: BTreeMap<String, Value>,
    /// The rows of the Table fields the row gives (JSON only).
    pub tables: BTreeMap<String, Vec<Row>>,
    /// Fields whose CSV cell was empty. An insert leaves them out (the default or null applies);
    /// an upsert of an existing document sets them to null.
    pub empty: Vec<String>,
}

/// A row that failed.
#[derive(Debug)]
pub struct ImportError {
    /// The 1-based data row number.
    pub row: usize,
    /// The line of a CSV file.
    pub line: Option<u64>,
    /// The document name, if the row gave one.
    pub name: Option<String>,
    /// Why.
    pub error: DbError,
}

/// What an import did (or, rolled back, would have done).
#[derive(Debug)]
pub struct ImportOutcome {
    /// Rows that inserted a document.
    pub inserted: u64,
    /// Rows that updated a document.
    pub updated: u64,
    /// The first [`MAX_REPORTED_ERRORS`] row errors, in row order.
    pub errors: Vec<ImportError>,
    /// The number of rows that failed.
    pub error_count: u64,
    /// The transaction was committed.
    pub committed: bool,
}

/// Imports `rows` as `caller`; see the module documentation. With `commit` false nothing is
/// kept (a dry run).
///
/// Errors: [`DbError::PermissionDenied`] when the caller lacks `import`, `create` or (upsert)
/// `write`, before any row; a database error that is not about a row's data (a lost connection,
/// for example) ends the import and is returned, rolled back. Row errors are in the outcome.
pub async fn import(
    client: &mut impl GenericClient,
    registry: &Registry,
    doctype: &str,
    caller: &Caller,
    rows: &[ImportRow],
    mode: ImportMode,
    commit: bool,
) -> Result<ImportOutcome, DbError> {
    let doctype = document_doctype(registry, doctype)?;
    check_actor(caller.name())?;
    access_to(doctype, caller, Right::Import)?;
    access_to(doctype, caller, Right::Create)?;
    if mode == ImportMode::Upsert {
        access_to(doctype, caller, Right::Write)?;
    }
    let mut tx = client.transaction().await?;
    let mut outcome = ImportOutcome {
        inserted: 0,
        updated: 0,
        errors: Vec::new(),
        error_count: 0,
        committed: false,
    };
    for item in rows {
        // The row's own savepoint: dropped (rolled back) on an error, so the transaction goes on.
        let mut savepoint = tx.transaction().await?;
        match import_row(&mut savepoint, registry, &doctype.name, caller, item, mode).await {
            Ok(Done::Inserted) => {
                savepoint.commit().await?;
                outcome.inserted += 1;
            }
            Ok(Done::Updated) => {
                savepoint.commit().await?;
                outcome.updated += 1;
            }
            Err(error) if is_row_error(&error) => {
                savepoint.rollback().await?;
                outcome.error_count += 1;
                if outcome.errors.len() < MAX_REPORTED_ERRORS {
                    outcome.errors.push(ImportError {
                        row: item.row,
                        line: item.line,
                        name: item.name.clone(),
                        error,
                    });
                }
            }
            // Dropping the transaction rolls everything back.
            Err(error) => return Err(error),
        }
    }
    if commit && outcome.error_count == 0 {
        tx.commit().await?;
        outcome.committed = true;
    } else {
        tx.rollback().await?;
    }
    Ok(outcome)
}

enum Done {
    Inserted,
    Updated,
}

/// An error about the row's data or the document's state, as opposed to a failure of the
/// database or the app that would fail every row.
fn is_row_error(error: &DbError) -> bool {
    match error {
        DbError::Postgres(e) => e.code().is_some_and(|code| {
            // Data exceptions (22...) and integrity violations (23...) are about the row.
            code.code().starts_with("22") || code.code().starts_with("23")
        }),
        DbError::Pool(_)
        | DbError::LockTimeout
        | DbError::Decode { .. }
        | DbError::Job(_)
        | DbError::Webhook(_)
        | DbError::InvalidActor
        | DbError::UnknownDocType { .. }
        | DbError::ChildDocType { .. } => false,
        _ => true,
    }
}

async fn import_row(
    client: &mut impl GenericClient,
    registry: &Registry,
    doctype: &str,
    caller: &Caller,
    item: &ImportRow,
    mode: ImportMode,
) -> Result<Done, DbError> {
    if mode == ImportMode::Insert {
        let document = Document {
            name: item.name.clone(),
            values: item.values.clone(),
            tables: item.tables.clone(),
        };
        crate::insert(client, registry, doctype, document, caller).await?;
        return Ok(Done::Inserted);
    }
    let Some(name) = item.name.as_deref() else {
        return Err(DbError::Naming(
            "an upsert needs a `name` for every row".to_owned(),
        ));
    };
    let Some(stored) = crate::get(client, registry, doctype, name, caller).await? else {
        let document = Document {
            name: Some(name.to_owned()),
            values: item.values.clone(),
            tables: item.tables.clone(),
        };
        crate::insert(client, registry, doctype, document, caller).await?;
        return Ok(Done::Inserted);
    };
    let mut values = item.values.clone();
    for field in &item.empty {
        values.insert(field.clone(), Value::Null);
    }
    // The naming field gives the name and cannot be changed: its value is the name already (the
    // row is the same document), so it is left out; a different value is a rename, refused.
    if let Some(Naming::Field(field)) = document_doctype(registry, doctype)
        .ok()
        .map(|d| d.naming.clone())
        && let Some(given) = values.remove(&field)
        && given != Value::Null
        && given != Value::Text(stored.name.clone())
    {
        return Err(DbError::Naming(format!(
            "field `{field}` gives the document its name and cannot be changed (renames are not supported yet)"
        )));
    }
    let changes = Changes {
        values,
        tables: item.tables.clone(),
    };
    crate::update(
        client,
        registry,
        doctype,
        name,
        stored.version,
        &changes,
        caller,
    )
    .await?;
    Ok(Done::Updated)
}
