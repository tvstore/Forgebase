//! PostgreSQL access: connection pool, framework migrations and DocType schema sync.
//!
//! **Internal crate, not a public API:** applications use the `forgebase` crate, and nothing here
//! has a compatibility promise (ADR 0012). Depends only on `forgebase-core`. See ADR 0002 and `docs/blueprint/architecture.md` (section 5).
//!
//! Implemented so far (milestones M1.2, M1.3, M2.1, M2.2, M2.3, M2.4, M3.1 and M3.2):
//!
//! - [`connect`]: a connection pool. TLS to PostgreSQL is **not supported yet** (planned:
//!   rustls); until then run over a local or private network.
//! - [`migrate_framework`]: Forgebase's own tables (`fb_migration`, `fb_doctype`, `fb_series`, the
//!   naming series counters, from migration 0002, and the auth tables of migration 0003).
//! - [`sync`]: additive DocType schema sync with dry-run, conflict refusal and `lock_timeout`;
//!   Link fields become `uuid` columns with a foreign key (`ON DELETE RESTRICT`) and an index; a
//!   child DocType's table has `id`, `parent_id` (foreign key `ON DELETE CASCADE`, indexed) and
//!   `idx`; a submittable DocType's table has `docstatus` and `amended_from`.
//! - [`migrate`] and [`migrate_apps`]: both of the above in order, and the SQL data migrations of
//!   apps (migration 0004, M5.1, ADR 0012).
//! - [`quote_ident`]: quoting for every identifier in generated SQL.
//! - [`insert`], [`get`], [`update`], [`delete`]: one document (with its child table rows) at a
//!   time, on a pooled client or a caller-owned [`Transaction`]; each call runs in its own
//!   transaction (a savepoint inside a caller's). They take the [`Registry`] and the DocType
//!   name. Link values are target names, resolved to ids internally; `update` and `delete` need
//!   the `expected_version` (optimistic concurrency); an update replaces the rows of the Table
//!   fields it names when they differ from the stored ones. The app's lifecycle hooks
//!   (`forgebase_core::hooks`, registered in the [`Registry`]) run inside each call's
//!   transaction in a fixed order, documented on each of the four functions; a hook error is
//!   [`DbError::Hook`] and rolls the call back. `update` works on the whole stored document with
//!   the changes merged in. [`submit`], [`cancel`] and [`amend`] move a document of a `submittable`
//!   DocType through draft, submitted and cancelled (M2.4), and a naming series takes its number
//!   from `fb_series` inside the insert's transaction. No renames yet. **Permissions (M3.2, ADR
//!   0011):** every operation takes a [`Caller`] (a user with roles, or the trusted system
//!   context) and enforces the DocType's `[[permissions]]` with the one evaluator in
//!   `forgebase-core`; read access is applied in SQL, an unreadable document is "not found", and
//!   [`list`] filters, sorts and pages with a typed [`ListQuery`]. See the `document` module
//!   documentation of the functions for the exact rules.
//! - [`auth`]: users, roles, Argon2id password hashing, server-side sessions with CSRF tokens, API
//!   tokens and login lockout (M3.1, ADR 0010), on the tables of migration 0003. No HTTP or
//!   cookies here (they are in `forgebase-http`); [`auth::AuthUser::caller`] gives the [`Caller`] for the document functions.

mod app_migrations;
pub mod auth;
mod document;
mod error;
mod ident;
mod list;
mod migrate;
mod plan;
mod sync;
mod value;

pub use app_migrations::{AppMigration, AppSpec, AppliedAppMigration};
pub use deadpool_postgres::Pool;
pub use document::{StoredDocument, amend, cancel, delete, get, insert, submit, update};
pub use error::DbError;
pub use forgebase_core::permission::Caller;
pub use ident::quote_ident;
pub use list::{
    DEFAULT_LIMIT, Filter, FilterOp, ListQuery, MAX_IN_VALUES, MAX_LIMIT, Sort, column_type, list,
};
pub use migrate::{MIGRATIONS, Migration, migrate_framework};
pub use plan::{Conflict, Step, Warning};
pub use sync::{SyncOptions, SyncReport, sync};
pub use tokio_postgres::{Client, GenericClient, SimpleQueryMessage, Transaction};

use deadpool_postgres::Runtime;
use forgebase_core::meta::Registry;
use tokio_postgres::NoTls;

/// Creates a connection pool for `database_url` (`postgres://user:password@host:port/database`)
/// and checks that a connection can be made.
///
/// TLS is not supported yet: connections are made without encryption. There is no connect
/// timeout by default; add `connect_timeout=<seconds>` to the URL to bound it.
pub async fn connect(database_url: &str) -> Result<Pool, DbError> {
    let mut config = deadpool_postgres::Config::new();
    config.url = Some(database_url.to_owned());
    let pool = config.create_pool(Some(Runtime::Tokio1), NoTls)?;
    drop(pool.get().await?);
    Ok(pool)
}

/// What [`migrate`] did (or, in a dry run, would do).
#[derive(Debug)]
pub struct MigrateReport {
    /// Framework migrations applied (pending ones in a dry run).
    pub migrations: Vec<&'static Migration>,
    /// The schema sync report. `None` in a dry run while framework migrations are pending,
    /// because the sync plan depends on tables those migrations create.
    pub sync: Option<SyncReport>,
    /// App migrations applied (pending ones in a dry run), in app order and version order.
    pub app_migrations: Vec<AppliedAppMigration>,
}

/// Applies the framework migrations, then syncs the DocType schema. Same as [`migrate_apps`]
/// without apps.
pub async fn migrate(
    pool: &Pool,
    registry: &Registry,
    options: &SyncOptions,
) -> Result<MigrateReport, DbError> {
    migrate_apps(pool, registry, &[], options).await
}

/// Applies the framework migrations, syncs the DocType schema of `registry` and then applies
/// the data migrations of `apps`, in the order of `apps` and, per app, by version.
///
/// The advisory lock of the framework migrations is held for the whole call (a session-level
/// lock on the one connection used), so two `migrate` runs never interleave. The steps still
/// use separate transactions: framework migrations and the schema sync stay applied if a later
/// step fails, and each app migration is its own transaction together with its
/// `fb_app_migration` row, so a failing one is rolled back
/// ([`DbError::AppMigrationFailed`]) and stops the run; the ones before it stay applied.
///
/// Before the schema sync every applied migration of `apps` is verified against the SQL the
/// apps declare now ([`DbError::AppChecksumMismatch`], [`DbError::UnknownAppMigration`]), so a
/// changed migration is refused before anything but the framework migrations is applied. After
/// an app's migrations, `fb_app` is upserted with the app's name and version (apps that are not
/// in `apps` are left as they are).
///
/// With `dry_run` nothing is kept: the report lists the pending framework and app migrations
/// and the sync plan. While framework migrations are pending the sync plan is `None` and every
/// declared app migration is listed as pending (no app migration can have been applied before
/// migration 0004 exists).
pub async fn migrate_apps(
    pool: &Pool,
    registry: &Registry,
    apps: &[AppSpec<'_>],
    options: &SyncOptions,
) -> Result<MigrateReport, DbError> {
    let mut client = pool.get().await?;
    lock(&client, options).await?;
    let result = migrate_locked(&mut client, registry, apps, options).await;
    // A failure here means the connection is gone, and then so is its lock.
    let _ = client
        .execute(
            "SELECT pg_advisory_unlock($1)",
            &[&migrate::MIGRATE_LOCK_KEY],
        )
        .await;
    result
}

/// Takes the session-level migration lock, waiting at most `options.lock_timeout`.
async fn lock(client: &Client, options: &SyncOptions) -> Result<(), DbError> {
    let timeout = options
        .lock_timeout
        .as_millis()
        .clamp(1, u128::from(i32::MAX.unsigned_abs()))
        .to_string()
        + "ms";
    client
        .execute("SELECT set_config('lock_timeout', $1, false)", &[&timeout])
        .await?;
    let locked = client
        .execute("SELECT pg_advisory_lock($1)", &[&migrate::MIGRATE_LOCK_KEY])
        .await;
    // Back to the connection's default, so a pooled connection is not left changed.
    client.batch_execute("RESET lock_timeout").await?;
    locked?;
    Ok(())
}

async fn migrate_locked(
    client: &mut Client,
    registry: &Registry,
    apps: &[AppSpec<'_>],
    options: &SyncOptions,
) -> Result<MigrateReport, DbError> {
    let migrations = migrate_framework(client, options.dry_run).await?;
    let applied = |app: &str, m: &AppMigration<'_>| AppliedAppMigration {
        app: app.to_owned(),
        version: m.version,
        name: m.name.to_owned(),
    };
    if options.dry_run && !migrations.is_empty() {
        let app_migrations = apps
            .iter()
            .flat_map(|a| a.migrations.iter().map(|m| applied(a.name, m)))
            .collect();
        return Ok(MigrateReport {
            migrations,
            sync: None,
            app_migrations,
        });
    }
    let pending = app_migrations::pending(client, apps).await?;
    let sync = sync(client, registry, options).await?;
    let mut app_migrations = Vec::new();
    if options.dry_run {
        app_migrations.extend(pending.iter().map(|(app, m)| applied(app, m)));
    } else {
        for app in apps {
            for (_, migration) in pending.iter().filter(|(name, _)| *name == app.name) {
                app_migrations::apply(client, app.name, migration).await?;
                app_migrations.push(applied(app.name, migration));
            }
            app_migrations::record(client, app).await?;
        }
    }
    Ok(MigrateReport {
        migrations,
        sync: Some(sync),
        app_migrations,
    })
}
