//! Framework migrations: Forgebase's own tables, as embedded ordered SQL.

use std::collections::BTreeMap;

use tokio_postgres::Client;

use crate::error::DbError;

/// Advisory lock key serializing migrations: taken per transaction by [`migrate_framework`] and
/// for the whole of [`crate::migrate_apps`] (session-level lock).
pub(crate) const MIGRATE_LOCK_KEY: i64 = 0x4642_4d49_4752_0001;

/// The migration table, created before the migrations run so applied ones can be read.
const BOOTSTRAP_SQL: &str = "CREATE TABLE IF NOT EXISTS fb_migration (\
    version integer PRIMARY KEY, name text NOT NULL, checksum text NOT NULL, \
    applied_at timestamptz NOT NULL DEFAULT now())";

/// One embedded framework migration.
#[derive(Debug, PartialEq, Eq)]
pub struct Migration {
    /// Strictly increasing version number.
    pub version: i32,
    /// Short name.
    pub name: &'static str,
    /// The SQL. Runs inside the migration transaction, so it must not contain `BEGIN`/`COMMIT`.
    /// Never edit an applied migration: its checksum is verified.
    pub sql: &'static str,
}

/// All framework migrations, in order.
pub const MIGRATIONS: &[Migration] = &[
    Migration {
        version: 1,
        name: "init",
        sql: include_str!("../migrations/0001_init.sql"),
    },
    Migration {
        version: 2,
        name: "series",
        sql: include_str!("../migrations/0002_series.sql"),
    },
    Migration {
        version: 3,
        name: "auth",
        sql: include_str!("../migrations/0003_auth.sql"),
    },
    Migration {
        version: 4,
        name: "apps",
        sql: include_str!("../migrations/0004_apps.sql"),
    },
];

/// Applies the pending framework migrations and returns them.
///
/// Everything runs in one transaction guarded by a transaction-level advisory lock, so
/// concurrent callers queue and a failure leaves nothing behind. Before applying, every
/// applied migration is checked: it must exist in this build ([`DbError::UnknownMigration`])
/// and its SQL checksum (SHA-256, computed by PostgreSQL) must match
/// ([`DbError::ChecksumMismatch`]). With `dry_run` nothing is kept and the pending migrations
/// are returned. A dry run still executes the migration SQL (and takes its locks) before
/// rolling back, so a slow or lock-heavy migration is just as slow in a dry run.
pub async fn migrate_framework(
    client: &mut Client,
    dry_run: bool,
) -> Result<Vec<&'static Migration>, DbError> {
    let tx = client.transaction().await?;
    tx.query("SELECT pg_advisory_xact_lock($1)", &[&MIGRATE_LOCK_KEY])
        .await?;
    tx.batch_execute(BOOTSTRAP_SQL).await?;

    let rows = tx
        .query("SELECT version, name, checksum FROM fb_migration", &[])
        .await?;
    let mut applied: BTreeMap<i32, String> = BTreeMap::new();
    for row in &rows {
        // try_get: a pre-existing fb_migration with other column types is an error, not a panic.
        let (version, name, actual): (i32, String, String) =
            (row.try_get(0)?, row.try_get(1)?, row.try_get(2)?);
        let Some(migration) = MIGRATIONS.iter().find(|m| m.version == version) else {
            return Err(DbError::UnknownMigration { version, name });
        };
        let expected = checksum(&tx, migration.sql).await?;
        if expected != actual {
            return Err(DbError::ChecksumMismatch {
                version,
                name,
                expected,
                actual,
            });
        }
        applied.insert(version, name);
    }

    let pending: Vec<&'static Migration> = MIGRATIONS
        .iter()
        .filter(|m| !applied.contains_key(&m.version))
        .collect();
    for migration in &pending {
        tx.batch_execute(migration.sql).await?;
        let checksum = checksum(&tx, migration.sql).await?;
        tx.execute(
            "INSERT INTO fb_migration (version, name, checksum) VALUES ($1, $2, $3)",
            &[&migration.version, &migration.name, &checksum],
        )
        .await?;
    }
    if dry_run {
        tx.rollback().await?;
    } else {
        tx.commit().await?;
    }
    Ok(pending)
}

/// The SHA-256 of `sql` as lower-case hex, computed by PostgreSQL.
pub(crate) async fn checksum(
    client: &impl tokio_postgres::GenericClient,
    sql: &str,
) -> Result<String, DbError> {
    let row = client
        .query_one(
            "SELECT encode(sha256(convert_to($1::text, 'UTF8')), 'hex')",
            &[&sql],
        )
        .await?;
    Ok(row.try_get(0)?)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn migrations_are_strictly_ordered() {
        assert!(MIGRATIONS.windows(2).all(|w| w[0].version < w[1].version));
        assert!(MIGRATIONS.iter().all(|m| !m.sql.trim().is_empty()));
    }

    #[test]
    fn bootstrap_matches_first_migration() {
        // The bootstrap table must stay compatible with the CREATE TABLE IF NOT EXISTS in 0001.
        let first = MIGRATIONS[0].sql;
        assert!(first.contains("CREATE TABLE IF NOT EXISTS fb_migration"));
        assert!(BOOTSTRAP_SQL.contains("version integer PRIMARY KEY"));
    }
}
