//! Running query reports (M8.4, ADR 0016): read-only, with a statement timeout and a row limit.
//!
//! The SQL is the app's own, compiled into the binary, so it is trusted code: what this module
//! guards against is a **mistake** (a statement that writes, one that never ends, one that
//! returns millions of rows) and **input** (parameters are bound, never put into the text).
//!
//! - **Access.** Only the report's roles, an administrator and the system context
//!   ([`can_run_report`]). Anyone else gets [`DbError::ReportNotFound`], the same error as for a
//!   report that does not exist.
//! - **Row-level rules do not apply** inside the SQL: it shows what it selects, to every role that
//!   may run it.
//! - **Execution.** One `READ ONLY` transaction; `SET LOCAL statement_timeout` (the report's
//!   timeout) and `SET LOCAL lock_timeout`; the statement is prepared, bound and run through a
//!   portal that returns at most `max_rows + 1` rows, so the database stops producing rows after
//!   that. A write fails with SQLSTATE 25006 ([`DbError::ReportReadOnly`]), the timeout with
//!   57014 ([`DbError::ReportTimeout`]).
//! - **Parameters** are the declared ones in order, `$1..$n`; an absent optional one is `NULL`.
//!   A parameter whose type PostgreSQL infers as `text` (for example `$1::text`) takes any kind
//!   of value as its text form; an integer parameter fits `int2`, `int4`, `int8` and `numeric`.
//! - **Never in an error:** the SQL text.

use std::collections::{BTreeMap, BTreeSet};

use bytes::BytesMut;
use forgebase_core::meta::Registry;
use forgebase_core::permission::{Caller, can_run_report};
use forgebase_core::report::{ColumnKind, ParamKind, ReportDef};
use forgebase_core::value::Value;
use rust_decimal::Decimal;
use time::{Date, OffsetDateTime};
use tokio_postgres::error::SqlState;
use tokio_postgres::types::{IsNull, ToSql, Type, to_sql_checked};
use tokio_postgres::{Client, Row};

use crate::error::DbError;
use crate::link_titles::{LinkTitles, resolve};
use crate::value::SqlValue;

/// How long a statement may wait for a lock, in milliseconds. A report that has to wait this
/// long is answered like one that is too slow.
const LOCK_TIMEOUT_MS: u128 = 5_000;

/// The rows of a report, in the declared column order, as the same [`Value`]s the documents use.
#[derive(Debug, Clone, PartialEq)]
pub struct ReportResult {
    /// At most `max_rows` rows. Every row has one value per declared column.
    pub rows: Vec<Vec<Value>>,
    /// There were more rows than `max_rows`: the rest is not returned.
    pub truncated: bool,
}

type BoxError = Box<dyn std::error::Error + Sync + Send>;

/// A parameter value as a bound parameter. The type comes from what PostgreSQL inferred for the
/// placeholder (see the module documentation), never from the value.
#[derive(Debug)]
struct Arg<'a>(&'a Value);

impl Arg<'_> {
    /// The value as text, for a placeholder that PostgreSQL infers as `text`.
    fn text(&self) -> Option<String> {
        match self.0 {
            Value::Text(s) => Some(s.clone()),
            Value::Int(n) => Some(n.to_string()),
            Value::Decimal(d) => Some(d.to_string()),
            Value::Bool(b) => Some(b.to_string()),
            Value::Date(d) => Some(d.to_string()),
            _ => None,
        }
    }
}

impl ToSql for Arg<'_> {
    fn to_sql(&self, ty: &Type, out: &mut BytesMut) -> Result<IsNull, BoxError> {
        match (self.0, ty) {
            (Value::Null, _) => Ok(IsNull::Yes),
            (_, &Type::TEXT | &Type::VARCHAR | &Type::BPCHAR | &Type::NAME) => self
                .text()
                .ok_or_else(|| BoxError::from("cannot bind this value as text"))?
                .to_sql(ty, out),
            (Value::Int(n), &Type::INT2) => i16::try_from(*n)?.to_sql(ty, out),
            (Value::Int(n), &Type::INT4) => i32::try_from(*n)?.to_sql(ty, out),
            (Value::Int(n), &Type::NUMERIC) => Decimal::from(*n).to_sql(ty, out),
            (value, _) => SqlValue(value).to_sql(ty, out),
        }
    }

    fn accepts(_: &Type) -> bool {
        true
    }

    to_sql_checked!();
}

/// Reads column `index` of `row` as a value of the declared kind. A column of another SQL type is
/// an error (the check at `migrate` finds that earlier), not a panic.
fn decode(row: &Row, index: usize, kind: &ColumnKind) -> Result<Value, tokio_postgres::Error> {
    Ok(match kind {
        ColumnKind::Data | ColumnKind::Link { .. } => row
            .try_get::<_, Option<String>>(index)?
            .map_or(Value::Null, Value::Text),
        ColumnKind::Int => {
            let n = match *row.columns()[index].type_() {
                Type::INT2 => row.try_get::<_, Option<i16>>(index)?.map(i64::from),
                Type::INT4 => row.try_get::<_, Option<i32>>(index)?.map(i64::from),
                _ => row.try_get::<_, Option<i64>>(index)?,
            };
            n.map_or(Value::Null, Value::Int)
        }
        ColumnKind::Decimal => row
            .try_get::<_, Option<Decimal>>(index)?
            .map_or(Value::Null, Value::Decimal),
        ColumnKind::Date => row
            .try_get::<_, Option<Date>>(index)?
            .map_or(Value::Null, Value::Date),
        ColumnKind::Datetime => row
            .try_get::<_, Option<OffsetDateTime>>(index)?
            .map_or(Value::Null, Value::Datetime),
        ColumnKind::Check => row
            .try_get::<_, Option<bool>>(index)?
            .map_or(Value::Null, Value::Bool),
    })
}

/// The report's error for a database error: the timeout, the read-only refusal, or the ordinary
/// database error.
fn report_error(report: &ReportDef, error: tokio_postgres::Error) -> DbError {
    let code = error.code();
    if code == Some(&SqlState::QUERY_CANCELED) || code == Some(&SqlState::LOCK_NOT_AVAILABLE) {
        DbError::ReportTimeout {
            report: report.full_name(),
        }
    } else if code == Some(&SqlState::READ_ONLY_SQL_TRANSACTION) {
        DbError::ReportReadOnly {
            report: report.full_name(),
        }
    } else {
        DbError::Postgres(error)
    }
}

/// A connection for **one** report run. When it is dropped the connection is closed, not
/// returned to the pool: a report's SQL is the app's own, and it may leave session state behind
/// (a session-level `set_config`, a `PREPARE`, a temporary table, `LISTEN`, an advisory lock) that
/// the next request on a pooled connection would inherit. Closing the connection is the one way
/// that cannot miss a kind of state; `DISCARD ALL` would also drop the prepared statements that
/// the pool caches. The cost is a new connection for the next checkout, which is acceptable for
/// a report (it is heavier than a document read anyway).
///
/// It dereferences to the [`Client`], so it can also serve what the handler does after the run
/// (the titles of Link columns) on the same connection.
pub struct ReportConnection {
    object: Option<deadpool_postgres::Object>,
}

impl ReportConnection {
    /// Takes a connection from the pool.
    pub async fn get(pool: &crate::Pool) -> Result<Self, DbError> {
        Ok(Self {
            object: Some(pool.get().await?),
        })
    }
}

impl std::ops::Deref for ReportConnection {
    type Target = Client;

    fn deref(&self) -> &Client {
        // `object` is `Some` until `drop`.
        let object = self.object.as_ref().expect("the connection is present");
        &***object
    }
}

impl std::ops::DerefMut for ReportConnection {
    fn deref_mut(&mut self) -> &mut Client {
        let object = self.object.as_mut().expect("the connection is present");
        &mut ***object
    }
}

impl Drop for ReportConnection {
    fn drop(&mut self) {
        if let Some(object) = self.object.take() {
            // Taken out of the pool: dropping it closes the connection.
            drop(deadpool_postgres::Object::take(object));
        }
    }
}

/// Runs `report` for `caller` with the parameter values `params` (by parameter name; see
/// [`ReportDef::bind_params`] for what is checked) and returns its rows.
///
/// Errors: [`DbError::ReportNotFound`] when the caller may not run it; [`DbError::Validation`]
/// with every parameter problem; [`DbError::ReportTimeout`] when the statement ran over its
/// timeout or waited too long for a lock; [`DbError::ReportReadOnly`] when it tried to write;
/// [`DbError::Report`] when the prepared statement does not have the declared parameters or
/// columns; any other database error as it is. Nothing is ever changed.
pub async fn run_report(
    client: &mut Client,
    report: &ReportDef,
    caller: &Caller,
    params: &BTreeMap<String, Value>,
) -> Result<ReportResult, DbError> {
    if !can_run_report(&report.roles, caller) {
        return Err(DbError::ReportNotFound {
            report: report.full_name(),
        });
    }
    let values = report.bind_params(params).map_err(DbError::Validation)?;
    let tx = client
        .build_transaction()
        .read_only(true)
        .start()
        .await
        .map_err(DbError::from)?;
    // The numbers come from validated definitions (a `Duration` and a `u32`), never from input.
    let timeout_ms = report.timeout.as_millis().max(1);
    tx.batch_execute(&format!(
        "SET LOCAL statement_timeout = {timeout_ms}; SET LOCAL lock_timeout = {LOCK_TIMEOUT_MS}"
    ))
    .await
    .map_err(DbError::from)?;
    let rows = fetch(&tx, report, &values).await;
    // Read-only: nothing to keep either way. A failed statement leaves the transaction aborted,
    // and the rollback ends it.
    let _ = tx.rollback().await;
    let mut rows = rows?;
    let truncated = rows.len() > report.max_rows as usize;
    rows.truncate(report.max_rows as usize);
    let rows = rows
        .iter()
        .map(|row| {
            report
                .columns
                .iter()
                .enumerate()
                .map(|(i, column)| decode(row, i, &column.kind))
                .collect::<Result<Vec<_>, _>>()
        })
        .collect::<Result<Vec<_>, _>>()
        .map_err(DbError::from)?;
    Ok(ReportResult { rows, truncated })
}

/// Prepares the statement, checks its shape, binds `values` and reads at most `max_rows + 1`
/// rows through a portal.
async fn fetch(
    tx: &tokio_postgres::Transaction<'_>,
    report: &ReportDef,
    values: &[Value],
) -> Result<Vec<Row>, DbError> {
    let statement = tx
        .prepare(&report.sql)
        .await
        .map_err(|e| report_error(report, e))?;
    // The migrate check says the same earlier; this stops a statement that was never checked (a
    // `COMMIT`, a wrong number of columns) before it runs.
    if statement.params().len() != report.params.len()
        || statement.columns().len() != report.columns.len()
    {
        return Err(DbError::Report(format!(
            "report `{}`: the SQL has {} parameter(s) and {} column(s), but {} and {} are declared",
            report.full_name(),
            statement.params().len(),
            statement.columns().len(),
            report.params.len(),
            report.columns.len()
        )));
    }
    let args: Vec<Arg<'_>> = values.iter().map(Arg).collect();
    let refs: Vec<&(dyn ToSql + Sync)> = args.iter().map(|a| a as &(dyn ToSql + Sync)).collect();
    let portal = tx
        .bind(&statement, &refs)
        .await
        .map_err(|e| report_error(report, e))?;
    let limit = i32::try_from(u64::from(report.max_rows) + 1).unwrap_or(i32::MAX);
    tx.query_portal(&portal, limit)
        .await
        .map_err(|e| report_error(report, e))
}

/// Titles for the Link columns of a result, under **the caller's read rules** like
/// [`link_titles`](crate::link_titles) (a target the caller cannot read gets no entry). At most
/// [`MAX_LIMIT`](crate::MAX_LIMIT) distinct names per target DocType are resolved; the others show
/// as their names.
pub async fn report_link_titles(
    client: &impl tokio_postgres::GenericClient,
    registry: &Registry,
    caller: &Caller,
    report: &ReportDef,
    rows: &[Vec<Value>],
) -> Result<LinkTitles, DbError> {
    let mut found: BTreeMap<String, BTreeSet<String>> = BTreeMap::new();
    for (i, column) in report.columns.iter().enumerate() {
        let ColumnKind::Link { doctype } = &column.kind else {
            continue;
        };
        let names = found.entry(doctype.clone()).or_default();
        for row in rows {
            if let Some(Value::Text(name)) = row.get(i)
                && names.len() < crate::list::MAX_LIMIT as usize
            {
                names.insert(name.clone());
            }
        }
    }
    resolve(client, registry, caller, found).await
}

/// Whether a placeholder PostgreSQL typed as `ty` can take a parameter of `kind`.
fn param_fits(kind: &ParamKind, ty: &Type) -> bool {
    // Text (and the unknown type) take every kind, as the text form of the value.
    if matches!(
        *ty,
        Type::TEXT | Type::VARCHAR | Type::BPCHAR | Type::NAME | Type::UNKNOWN
    ) {
        return true;
    }
    match kind {
        ParamKind::Int => matches!(*ty, Type::INT2 | Type::INT4 | Type::INT8 | Type::NUMERIC),
        ParamKind::Decimal => *ty == Type::NUMERIC,
        ParamKind::Date => *ty == Type::DATE,
        ParamKind::Check => *ty == Type::BOOL,
        ParamKind::Data | ParamKind::Link { .. } | ParamKind::Select { .. } => false,
    }
}

/// Whether a result column of SQL type `ty` fits the declared `kind`.
fn column_fits(kind: &ColumnKind, ty: &Type) -> bool {
    match kind {
        ColumnKind::Data | ColumnKind::Link { .. } => {
            matches!(*ty, Type::TEXT | Type::VARCHAR | Type::BPCHAR)
        }
        ColumnKind::Int => matches!(*ty, Type::INT2 | Type::INT4 | Type::INT8),
        ColumnKind::Decimal => *ty == Type::NUMERIC,
        ColumnKind::Date => *ty == Type::DATE,
        ColumnKind::Datetime => *ty == Type::TIMESTAMPTZ,
        ColumnKind::Check => *ty == Type::BOOL,
    }
}

/// Prepares every report's statement, in a `READ ONLY` transaction that is rolled back, and
/// returns one message per problem (empty when all fit). Nothing is run. It checks that
///
/// 1. the SQL parses and its tables and columns exist;
/// 2. it has as many parameters as declared;
/// 3. it returns as many columns as declared;
/// 4. each column's type fits its kind: `int2`/`int4`/`int8` for int, `numeric` for decimal,
///    `text`/`varchar` (and `bpchar`) for data and link, `date`, `timestamptz` for datetime,
///    `bool` for check;
/// 5. each parameter's type, as PostgreSQL infers it, fits its kind: int takes the integer types
///    and `numeric`, decimal `numeric`, date `date`, check `bool`, and data, link and select
///    `text` or `varchar`. **A parameter inferred as `text` or `unknown` is accepted for every
///    kind** (the value is bound as its text form), so `$1::text` is always fine.
///
/// A database error that is not about the statement (a lost connection) is returned as an error.
pub async fn check_reports(
    client: &mut Client,
    reports: &[&ReportDef],
) -> Result<Vec<String>, DbError> {
    let tx = client
        .build_transaction()
        .read_only(true)
        .start()
        .await
        .map_err(DbError::from)?;
    let mut problems = Vec::new();
    for report in reports {
        let name = report.full_name();
        // A savepoint per report: a statement that fails to prepare aborts the transaction.
        tx.batch_execute("SAVEPOINT fb_report_check")
            .await
            .map_err(DbError::from)?;
        let prepared = tx.prepare(&report.sql).await;
        tx.batch_execute("ROLLBACK TO SAVEPOINT fb_report_check")
            .await
            .map_err(DbError::from)?;
        let statement = match prepared {
            Ok(statement) => statement,
            Err(error) if error.as_db_error().is_some() => {
                let message = error
                    .as_db_error()
                    .map_or_else(String::new, |db| db.message().to_owned());
                problems.push(format!(
                    "report `{name}`: the SQL does not prepare: {message}"
                ));
                continue;
            }
            Err(error) => return Err(error.into()),
        };
        if statement.params().len() != report.params.len() {
            problems.push(format!(
                "report `{name}`: the SQL has {} parameter(s) but {} are declared",
                statement.params().len(),
                report.params.len()
            ));
        } else {
            for (i, (param, ty)) in report.params.iter().zip(statement.params()).enumerate() {
                if !param_fits(&param.kind, ty) {
                    problems.push(format!(
                        "report `{name}`: parameter ${} (`{}`) is declared {}, but the SQL expects {}",
                        i + 1,
                        param.name,
                        param.kind.name(),
                        ty.name()
                    ));
                }
            }
        }
        if statement.columns().len() != report.columns.len() {
            problems.push(format!(
                "report `{name}`: the SQL returns {} column(s) but {} are declared",
                statement.columns().len(),
                report.columns.len()
            ));
        } else {
            for (i, (column, actual)) in report.columns.iter().zip(statement.columns()).enumerate()
            {
                if !column_fits(&column.kind, actual.type_()) {
                    problems.push(format!(
                        "report `{name}`: column {} (`{}`) is declared {}, but the SQL returns {}",
                        i + 1,
                        column.name,
                        column.kind.name(),
                        actual.type_().name()
                    ));
                }
            }
        }
    }
    let _ = tx.rollback().await;
    Ok(problems)
}
