//! App data migrations: versioned SQL that an app declares, applied once per database.
//!
//! The tables (`fb_app`, `fb_app_migration`) come from framework migration 0004. App code is
//! trusted and compiled in (ADR 0006, ADR 0012): the SQL runs with the same database role as
//! everything else.

use std::collections::BTreeMap;
use std::time::Duration;

use tokio_postgres::Client;

use crate::error::DbError;
use crate::migrate::checksum;
use crate::sync::lock_timeout_setting;

/// One migration an app declares.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct AppMigration<'a> {
    /// Positive version; strictly increasing within the app.
    pub version: i32,
    /// Short name, recorded in `fb_app_migration`.
    pub name: &'a str,
    /// The SQL. It runs inside the migration's transaction, so it must not contain
    /// `BEGIN`/`COMMIT`. Never edit an applied migration: its checksum is verified.
    pub sql: &'a str,
}

/// An app as far as the database layer needs to know it.
#[derive(Debug, Clone, Copy)]
pub struct AppSpec<'a> {
    /// The app name.
    pub name: &'a str,
    /// The version the app declares, recorded in `fb_app`.
    pub version: Option<&'a str>,
    /// The migrations, in the order they run (strictly increasing versions).
    pub migrations: &'a [AppMigration<'a>],
}

/// An app migration that was applied (in a dry run: would be applied).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AppliedAppMigration {
    /// App name.
    pub app: String,
    /// Migration version.
    pub version: i32,
    /// Migration name.
    pub name: String,
}

/// Whether the app migration tables exist (framework migration 0004 has been applied). `migrate_apps`
/// asks this in a dry run while framework migrations are pending; public (internal crate) so
/// that a test can check the decision, because no later framework migration exists yet to make
/// the case happen for real.
pub async fn tables_exist(client: &Client) -> Result<bool, DbError> {
    let row = client
        .query_one("SELECT to_regclass('fb_app_migration') IS NOT NULL", &[])
        .await?;
    Ok(row.try_get(0)?)
}

/// The migrations of `apps` that are not applied yet, in app order and then version order.
/// Every applied migration of these apps is checked first: it must still be declared
/// ([`DbError::UnknownAppMigration`]) and its checksum must match
/// ([`DbError::AppChecksumMismatch`]), and no declared migration that is not applied may have a
/// lower version than an applied one ([`DbError::OutOfOrderAppMigration`]). Apps that are not
/// in `apps` are not looked at.
pub(crate) async fn pending<'a>(
    client: &Client,
    apps: &[AppSpec<'a>],
) -> Result<Vec<(&'a str, AppMigration<'a>)>, DbError> {
    let names: Vec<&str> = apps.iter().map(|a| a.name).collect();
    let rows = client
        .query(
            "SELECT app, version, name, checksum FROM fb_app_migration WHERE app = ANY($1)",
            &[&names],
        )
        .await?;
    let mut applied: BTreeMap<(String, i32), (String, String)> = BTreeMap::new();
    for row in &rows {
        let (app, version, name, actual): (String, i32, String, String) = (
            row.try_get(0)?,
            row.try_get(1)?,
            row.try_get(2)?,
            row.try_get(3)?,
        );
        applied.insert((app, version), (name, actual));
    }
    let mut pending = Vec::new();
    for app in apps {
        for ((_, version), (name, _)) in applied.iter().filter(|((a, _), _)| a == app.name) {
            if !app.migrations.iter().any(|m| m.version == *version) {
                return Err(DbError::UnknownAppMigration {
                    app: app.name.to_owned(),
                    version: *version,
                    name: name.clone(),
                });
            }
        }
        let highest_applied = applied
            .keys()
            .filter(|(a, _)| a == app.name)
            .map(|(_, version)| *version)
            .max();
        for migration in app.migrations {
            match applied.get(&(app.name.to_owned(), migration.version)) {
                None => {
                    if let Some(applied_version) =
                        highest_applied.filter(|v| *v > migration.version)
                    {
                        return Err(DbError::OutOfOrderAppMigration {
                            app: app.name.to_owned(),
                            version: migration.version,
                            name: migration.name.to_owned(),
                            applied_version,
                        });
                    }
                    pending.push((app.name, *migration));
                }
                Some((name, actual)) => {
                    let expected = checksum(client, migration.sql).await?;
                    if expected != *actual {
                        return Err(DbError::AppChecksumMismatch {
                            app: app.name.to_owned(),
                            version: migration.version,
                            name: name.clone(),
                            expected,
                            actual: actual.clone(),
                        });
                    }
                }
            }
        }
    }
    Ok(pending)
}

/// Applies one migration in its own transaction together with its `fb_app_migration` row: the
/// SQL and the record commit or roll back together. A failing SQL is
/// [`DbError::AppMigrationFailed`]. The transaction has a `lock_timeout` of `lock_timeout`
/// (`SET LOCAL`, so it ends with the transaction): a migration that waits longer for a lock
/// fails and is rolled back.
pub(crate) async fn apply(
    client: &mut Client,
    app: &str,
    migration: &AppMigration<'_>,
    lock_timeout: Duration,
) -> Result<(), DbError> {
    let failed = |error: tokio_postgres::Error| DbError::AppMigrationFailed {
        app: app.to_owned(),
        version: migration.version,
        name: migration.name.to_owned(),
        message: DbError::from(error).to_string(),
    };
    let tx = client.transaction().await?;
    // Dropping `tx` on an error rolls it back.
    tx.query(
        "SELECT set_config('lock_timeout', $1, true)",
        &[&lock_timeout_setting(lock_timeout)],
    )
    .await?;
    tx.batch_execute(migration.sql).await.map_err(failed)?;
    let checksum = checksum(&tx, migration.sql).await?;
    tx.execute(
        "INSERT INTO fb_app_migration (app, version, name, checksum) VALUES ($1, $2, $3, $4)",
        &[&app, &migration.version, &migration.name, &checksum],
    )
    .await?;
    tx.commit().await?;
    Ok(())
}

/// Records that `app` was present at this migrate, with the version it declares.
pub(crate) async fn record(client: &Client, app: &AppSpec<'_>) -> Result<(), DbError> {
    client
        .execute(
            "INSERT INTO fb_app (name, version) VALUES ($1, $2) \
             ON CONFLICT (name) DO UPDATE SET version = excluded.version, migrated_at = now()",
            &[&app.name, &app.version],
        )
        .await?;
    Ok(())
}
