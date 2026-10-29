//! DocType schema sync: introspect, plan, and apply additive changes in one transaction.

use std::time::Duration;

use forgebase_core::ident::table_name;
use forgebase_core::meta::Registry;
use tokio_postgres::{Client, Transaction};

use crate::error::DbError;
use crate::ident::quote_ident;
use crate::plan::{Catalog, Column, ForeignKey, Step, Warning, plan};

/// Advisory lock key serializing schema syncs (transaction-level lock).
const SYNC_LOCK_KEY: i64 = 0x4642_5359_4e43_0001;

/// Options for [`sync`].
#[derive(Debug, Clone)]
pub struct SyncOptions {
    /// Compute and return the plan without keeping any change.
    pub dry_run: bool,
    /// Longest wait for any lock (including the sync advisory lock). Hitting it fails the sync
    /// with [`DbError::LockTimeout`]. Clamped to 1 ms ..= `i32::MAX` ms (about 24.8 days): 0
    /// would disable the timeout, and PostgreSQL rejects larger values.
    pub lock_timeout: Duration,
}

impl Default for SyncOptions {
    fn default() -> Self {
        Self {
            dry_run: false,
            lock_timeout: Duration::from_secs(5),
        }
    }
}

/// What a sync did (or, in a dry run, would do).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SyncReport {
    /// True if this was a dry run: nothing was changed.
    pub dry_run: bool,
    /// The steps applied, in order. Empty when the database already matches.
    pub steps: Vec<Step>,
    /// Differences noticed but intentionally left alone.
    pub warnings: Vec<Warning>,
}

/// Brings the database in line with `registry` using additive changes only.
///
/// Runs in one transaction: sets a transaction-local `lock_timeout`, takes a transaction-level
/// advisory lock, then introspects the connection's current schema (so concurrent syncs see
/// each other's result), plans, and applies. Conflicts ([`DbError::SyncConflicts`]) refuse the
/// whole sync and nothing is applied. No destructive DDL is generated. Index creation is plain
/// `CREATE INDEX`, which blocks writes on that table while it builds. Adding a foreign key
/// takes a lock on both tables and validates the existing rows.
///
/// A Link or Table field whose target DocType is not in `registry`, and the other reference rules
/// of child tables (a child belongs to exactly one parent), are refused first
/// ([`DbError::InvalidMetadata`], from [`Registry::check_references`]), before anything touches
/// the database. A child DocType's table gets `parent_id` and `idx` columns and a `parent_id`
/// foreign key with `ON DELETE CASCADE`.
///
/// The framework migrations must have been applied ([`DbError::FrameworkNotMigrated`]).
pub async fn sync(
    client: &mut Client,
    registry: &Registry,
    options: &SyncOptions,
) -> Result<SyncReport, DbError> {
    registry
        .check_references()
        .map_err(DbError::InvalidMetadata)?;
    let tx = client.transaction().await?;
    let timeout_ms = options
        .lock_timeout
        .as_millis()
        .clamp(1, u128::from(i32::MAX.unsigned_abs()))
        .to_string()
        + "ms";
    tx.query(
        "SELECT set_config('lock_timeout', $1, true)",
        &[&timeout_ms],
    )
    .await?;
    tx.query("SELECT pg_advisory_xact_lock($1)", &[&SYNC_LOCK_KEY])
        .await?;

    // Qualify fb_doctype with the current schema, like the introspection below.
    let schema: String = tx
        .query_one("SELECT current_schema()::text", &[])
        .await?
        .try_get(0)?;
    let doctype_table = format!("{}.fb_doctype", quote_ident(&schema));
    let registered = tx
        .query_one("SELECT to_regclass($1) IS NOT NULL", &[&doctype_table])
        .await?;
    if !registered.try_get::<_, bool>(0)? {
        return Err(DbError::FrameworkNotMigrated);
    }

    let catalog = load_catalog(&tx, registry, &doctype_table, &schema).await?;
    let plan = plan(registry, &catalog);
    if !plan.conflicts.is_empty() {
        tx.rollback().await?;
        return Err(DbError::SyncConflicts(plan.conflicts));
    }
    if options.dry_run {
        tx.rollback().await?;
    } else {
        for step in &plan.steps {
            apply(&tx, step, &doctype_table).await?;
        }
        tx.commit().await?;
    }
    Ok(SyncReport {
        dry_run: options.dry_run,
        steps: plan.steps,
        warnings: plan.warnings,
    })
}

async fn apply(tx: &Transaction<'_>, step: &Step, doctype_table: &str) -> Result<(), DbError> {
    match step {
        Step::CreateTable { sql, .. }
        | Step::AddKeys { sql, .. }
        | Step::AddForeignKeys { sql, .. }
        | Step::AddColumn { sql, .. }
        | Step::CreateUniqueIndex { sql, .. }
        | Step::CreateIndex { sql, .. } => tx.batch_execute(sql).await?,
        Step::RegisterDocType { doctype } => {
            tx.execute(
                &format!(
                    "INSERT INTO {doctype_table} (name, table_name) VALUES ($1, $2) \
                     ON CONFLICT (name) DO UPDATE SET table_name = excluded.table_name, synced_at = now()"
                ),
                &[doctype, &table_name(doctype)],
            )
            .await?;
        }
    }
    Ok(())
}

/// Reads the live schema (current schema only) for the tables of the registry. Rows are read
/// with `try_get`, so unexpected types in the database give an error, never a panic.
async fn load_catalog(
    tx: &Transaction<'_>,
    registry: &Registry,
    doctype_table: &str,
    schema: &str,
) -> Result<Catalog, DbError> {
    let tables: Vec<String> = registry.iter().map(|d| table_name(&d.name)).collect();
    let mut catalog = Catalog::default();

    let rows = tx
        .query(
            "SELECT table_name::text, column_name::text, data_type::text, \
                    character_maximum_length::int4, numeric_precision::int4, \
                    numeric_scale::int4, is_nullable::text, datetime_precision::int4 \
             FROM information_schema.columns \
             WHERE table_schema = current_schema() AND table_name = ANY($1::text[])",
            &[&tables],
        )
        .await?;
    for row in &rows {
        let (table, column, data_type): (String, String, String) =
            (row.try_get(0)?, row.try_get(1)?, row.try_get(2)?);
        let (length, precision, scale): (Option<i32>, Option<i32>, Option<i32>) =
            (row.try_get(3)?, row.try_get(4)?, row.try_get(5)?);
        let datetime_precision: Option<i32> = row.try_get(7)?;
        let data_type = match (data_type.as_str(), length, precision, scale) {
            ("character varying", Some(n), ..) => format!("character varying({n})"),
            ("numeric", _, Some(p), Some(s)) => format!("numeric({p},{s})"),
            // 6 is the default; anything else (for example timestamptz(0)) is drift.
            ("timestamp with time zone", ..) if datetime_precision.is_some_and(|p| p != 6) => {
                format!(
                    "timestamp({}) with time zone",
                    datetime_precision.unwrap_or(6)
                )
            }
            _ => data_type,
        };
        let not_null = row.try_get::<_, String>(6)? == "NO";
        catalog.columns.entry(table).or_default().insert(
            column,
            Column {
                data_type,
                not_null,
            },
        );
    }

    let rows = tx
        .query(
            "SELECT c.relname::text, a.attname::text, i.indisprimary, i.indisunique \
             FROM pg_index i \
             JOIN pg_class c ON c.oid = i.indrelid \
             JOIN pg_attribute a ON a.attrelid = c.oid AND a.attnum = i.indkey[0] \
             WHERE c.relnamespace = (SELECT oid FROM pg_namespace WHERE nspname = current_schema()) \
               AND c.relname = ANY($1::text[]) \
               AND i.indisvalid AND i.indnatts = 1 AND i.indpred IS NULL",
            &[&tables],
        )
        .await?;
    for row in &rows {
        let key = (row.try_get::<_, String>(0)?, row.try_get::<_, String>(1)?);
        catalog.indexed.insert(key.clone());
        if row.try_get::<_, bool>(2)? {
            catalog.primary.insert(key);
        } else if row.try_get::<_, bool>(3)? {
            catalog.unique.insert(key);
        }
    }

    // Single-column foreign keys. `confdeltype` is of the type "char"; it is cast for reading.
    let rows = tx
        .query(
            "SELECT c.relname::text, a.attname::text, rn.nspname::text, rt.relname::text, \
                    ra.attname::text, con.confdeltype::text, con.convalidated \
             FROM pg_constraint con \
             JOIN pg_class c ON c.oid = con.conrelid \
             JOIN pg_attribute a ON a.attrelid = c.oid AND a.attnum = con.conkey[1] \
             JOIN pg_class rt ON rt.oid = con.confrelid \
             JOIN pg_namespace rn ON rn.oid = rt.relnamespace \
             JOIN pg_attribute ra ON ra.attrelid = rt.oid AND ra.attnum = con.confkey[1] \
             WHERE con.contype = 'f' AND array_length(con.conkey, 1) = 1 \
               AND c.relnamespace = (SELECT oid FROM pg_namespace WHERE nspname = current_schema()) \
               AND c.relname = ANY($1::text[])",
            &[&tables],
        )
        .await?;
    for row in &rows {
        let key = (row.try_get::<_, String>(0)?, row.try_get::<_, String>(1)?);
        let (referenced_schema, referenced_table): (String, String) =
            (row.try_get(2)?, row.try_get(3)?);
        let table = if referenced_schema == schema {
            referenced_table
        } else {
            format!("{referenced_schema}.{referenced_table}")
        };
        let rule: String = row.try_get(5)?;
        catalog
            .foreign_keys
            .entry(key)
            .or_default()
            .push(ForeignKey {
                table,
                column: row.try_get(4)?,
                on_delete: rule.chars().next().unwrap_or('?'),
                validated: row.try_get(6)?,
            });
    }

    let rows = tx
        .query(
            "SELECT relname::text, relkind::text FROM pg_class \
             WHERE relnamespace = (SELECT oid FROM pg_namespace WHERE nspname = current_schema()) AND relname = ANY($1::text[])",
            &[&tables],
        )
        .await?;
    for row in &rows {
        let name: String = row.try_get(0)?;
        if matches!(row.try_get::<_, String>(1)?.as_str(), "r" | "p") {
            catalog.tables.insert(name.clone());
        }
        catalog.relations.insert(name);
    }

    // Standalone types (domain, enum, ...) share the namespace of table row types. Row types
    // of tables (typrelid <> 0) are covered by the pg_class query above.
    let rows = tx
        .query(
            "SELECT typname::text FROM pg_type \
             WHERE typnamespace = (SELECT oid FROM pg_namespace WHERE nspname = current_schema()) AND typname = ANY($1::text[]) \
               AND typrelid = 0",
            &[&tables],
        )
        .await?;
    for row in &rows {
        catalog.relations.insert(row.try_get(0)?);
    }

    let rows = tx
        .query(&format!("SELECT name::text FROM {doctype_table}"), &[])
        .await?;
    for row in &rows {
        catalog.managed.insert(row.try_get(0)?);
    }
    Ok(catalog)
}
