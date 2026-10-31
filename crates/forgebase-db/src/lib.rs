//! PostgreSQL access: connection pool, framework migrations and DocType schema sync.
//!
//! **Internal crate, not a public API:** applications use the `forgebase` crate, and nothing here
//! has a compatibility promise (ADR 0012). Depends only on `forgebase-core`. See ADR 0002 and `docs/blueprint/architecture.md` (section 5).
//!
//! Implemented so far (milestones M1.2, M1.3, M2.1, M2.2, M2.3, M2.4, M3.1, M3.2, M6.1, M6.2, M8.1, M8.3 and M9.0):
//!
//! - [`connect`]: a connection pool. TLS to PostgreSQL follows the `sslmode` of the URL (M9.0,
//!   ADR 0018): none by default, `sslmode=require` for TLS with full verification (rustls with
//!   `ring`), `sslrootcert` for a private CA.
//! - [`migrate_framework`]: Forgebase's own tables (`fb_migration`, `fb_doctype`, `fb_series`, the
//!   naming series counters, from migration 0002, and the auth tables of migration 0003).
//! - [`sync`]: additive DocType schema sync with dry-run, conflict refusal and `lock_timeout`;
//!   Link fields become `uuid` columns with a foreign key (`ON DELETE RESTRICT`) and an index; a
//!   child DocType's table has `id`, `parent_id` (foreign key `ON DELETE CASCADE`, indexed) and
//!   `idx`; a submittable DocType's table has `docstatus` and `amended_from`.
//! - [`migrate`] and [`migrate_apps`]: both of the above in order, and the SQL data migrations of
//!   apps (migration 0004, M5.1, ADR 0012); [`migrate_all`] adds the sync of the declared
//!   schedules (M6.2, ADR 0013).
//! - [`create_database`] and [`recorded_apps`] (M7.2): `CREATE DATABASE` for `site create`, and the
//!   apps a database records.
//! - [`pg_environment`] and [`inspect_database`] (M7.3): the `PG*` variables for `pg_dump` and
//!   `pg_restore`, and the server version and emptiness check of `site backup` and `site restore`.
//! - [`transition`], [`available_transitions`] and [`history`] (M8.1, ADR 0015): the workflow of a
//!   DocType, run in the update, submit and cancel pipelines; the transition log is the table of
//!   migration 0008, and `migrate` gives existing documents their state.
//! - [`list_views`], [`create_view`], [`update_view`] and [`delete_view`] (M8.3): saved list views,
//!   private per user or shared (Administrator only), on the table of migration 0009; and
//!   [`summary`], the group-by summary of a list, which takes its read scope and filters from
//!   the list code (`views` and `summary` module documentation).
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
//! - [`jobs`]: the job queue and the outbox (M6.1, ADR 0013): the table of migration 0005, enqueue,
//!   claim with leases, guarded completion and failure, and what the `job` commands read. It
//!   never runs a handler; the worker is in the `forgebase` crate. The document functions write
//!   the jobs that hooks enqueue in the operation's own transaction.
//! - [`schedule`]: the scheduler (M6.2, ADR 0013): schedule times ([`schedule::Spec`]), the
//!   `fb_schedule` table of migration 0006, its sync at `migrate` and the tick that enqueues due
//!   jobs exactly once however many workers run. Also here: the SQL of the built-in clean-up jobs
//!   ([`jobs::purge_finished`], [`auth::purge_sessions`], [`auth::purge_login_failures`]).
//! - [`auth`]: users, roles, Argon2id password hashing, server-side sessions with CSRF tokens, API
//!   tokens and login lockout (M3.1, ADR 0010), on the tables of migration 0003. No HTTP or
//!   cookies here (they are in `forgebase-http`); [`auth::AuthUser::caller`] gives the [`Caller`] for the document functions.

mod app_migrations;
pub mod auth;
mod backup;
mod database;
mod document;
mod error;
mod export;
mod ident;
mod import;
pub mod jobs;
pub mod json;
mod link_titles;
mod list;
mod migrate;
mod plan;
mod reports;
pub mod schedule;
mod summary;
mod sync;
pub mod tls;
mod value;
mod views;
pub mod webhooks;
mod workflow;

pub use app_migrations::{
    AppMigration, AppSpec, AppliedAppMigration, tables_exist as app_migration_tables_exist,
};
pub use backup::{DatabaseInfo, EMPTY_CHECK_SQL, inspect_database, pg_environment};
pub use database::{DEFAULT_MAINTENANCE_DB, create_database, recorded_apps};
pub use deadpool_postgres::Pool;
pub use document::{StoredDocument, amend, cancel, delete, get, insert, submit, update};
pub use error::DbError;
pub use export::{Export, ExportQuery, PAGE_ROWS, begin_export};
pub use forgebase_core::permission::Caller;
pub use ident::quote_ident;
pub use import::{ImportError, ImportMode, ImportOutcome, ImportRow, MAX_REPORTED_ERRORS, import};
pub use link_titles::{LinkTitles, link_titles};
pub use list::{
    DEFAULT_LIMIT, Filter, FilterOp, ListQuery, MAX_IN_VALUES, MAX_LIMIT, Sort, column_type, list,
};
pub use migrate::{MIGRATIONS, Migration, migrate_framework};
pub use plan::{Conflict, Step, Warning};
pub use reports::{ReportConnection, ReportResult, check_reports, report_link_titles, run_report};
pub use summary::{AggregateFn, Group, MAX_AGGREGATES, MAX_GROUPS, Summary, SummaryQuery, summary};
pub use sync::{SyncOptions, SyncReport, sync};
pub use tokio_postgres::{Client, GenericClient, SimpleQueryMessage, Transaction};
pub use uuid::Uuid;
pub use views::{
    MAX_DEFINITION_BYTES, MAX_LABEL_CHARS, MAX_PRIVATE_VIEWS, MAX_SHARED_VIEWS, MAX_VIEW_COLUMNS,
    View, ViewChanges, ViewDefinition, create_view, delete_view, list_views, update_view,
};
pub use workflow::{
    AvailableTransitions, HistoryEntry, MAX_HISTORY, available_transitions, history, transition,
};

use deadpool_postgres::Runtime;
use forgebase_core::meta::Registry;

/// Creates a connection pool for `database_url` (`postgres://user:password@host:port/database`)
/// and checks that a connection can be made.
///
/// TLS follows the `sslmode` of the URL (ADR 0018): none without `sslmode` or with `disable`;
/// `require`, `verify-ca` and `verify-full` all give TLS that verifies the certificate chain and
/// the host name, against the operating system's trust store or only the PEM file
/// `sslrootcert=<path>`; `prefer` and every other value is an error. There is no connect
/// timeout by default; add `connect_timeout=<seconds>` to the URL to bound it.
pub async fn connect(database_url: &str) -> Result<Pool, DbError> {
    connect_sized(database_url, None).await
}

/// Like [`connect`], with the largest number of connections the pool opens (`None`: the
/// default of the pool, four per CPU). A job worker needs one connection for each job it runs at
/// the same time, so it sets the size from its concurrency.
pub async fn connect_sized(database_url: &str, max_size: Option<usize>) -> Result<Pool, DbError> {
    let (config, tls) = tls::prepare(database_url)?;
    let manager = deadpool_postgres::Manager::from_config(
        config,
        tls,
        deadpool_postgres::ManagerConfig::default(),
    );
    let mut builder = Pool::builder(manager).runtime(Runtime::Tokio1);
    if let Some(max_size) = max_size {
        builder = builder.config(deadpool_postgres::PoolConfig::new(max_size));
    }
    let pool = builder
        .build()
        .map_err(deadpool_postgres::CreatePoolError::Build)?;
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
    /// The changes to the schedules (`add`, `change`, `remove`; see [`schedule::sync`]), one line
    /// each; with a dry run, the planned ones. Empty when the schedules were not given
    /// ([`migrate_apps`]) or, in a dry run, while framework migrations are pending.
    pub schedules: Vec<String>,
    /// Warnings of the workflow backfill (the state of existing documents, see ADR 0015), one line per DocType
    /// with documents in a state its workflow no longer defines. Empty in a dry run (nothing is
    /// backfilled) and when there is nothing to say.
    pub workflow_warnings: Vec<String>,
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
/// lock on one connection that is taken out of the pool and closed at the end, even on a panic or
/// when the future is dropped), so two `migrate` runs never interleave. The steps still
/// use separate transactions: framework migrations and the schema sync stay applied if a later
/// step fails, and each app migration is its own transaction together with its
/// `fb_app_migration` row, so a failing one is rolled back
/// ([`DbError::AppMigrationFailed`]) and stops the run; the ones before it stay applied. Each app
/// migration runs with `lock_timeout` set to `options.lock_timeout` (`SET LOCAL`).
///
/// Before the schema sync every applied migration of `apps` is verified against the SQL the
/// apps declare now ([`DbError::AppChecksumMismatch`], [`DbError::UnknownAppMigration`]), and a
/// declared migration that is not applied must not have a lower version than an applied one of
/// the same app ([`DbError::OutOfOrderAppMigration`]), so a changed or out-of-order migration is
/// refused before anything but the framework migrations is applied. After
/// an app's migrations, `fb_app` is upserted with the app's name and version (apps that are not
/// in `apps` are left as they are).
///
/// With `dry_run` nothing is kept: the report lists the pending framework and app migrations
/// and the sync plan. While framework migrations are pending the sync plan is `None`. The app
/// migrations are still verified and listed when `fb_app_migration` exists; before migration 0004
/// creates it, every declared app migration is listed as pending (none can have been applied).
pub async fn migrate_apps(
    pool: &Pool,
    registry: &Registry,
    apps: &[AppSpec<'_>],
    options: &SyncOptions,
) -> Result<MigrateReport, DbError> {
    migrate_all(pool, registry, apps, None, options).await
}

/// [`migrate_apps`], and then, in the same run and under the same lock, the sync of the
/// schedules the binary declares ([`schedule::sync`]) when `schedules` is `Some`: schedules the
/// binary does not declare are **deleted**, so the binary that migrates is the source of truth.
/// With `None` the schedule table is left alone.
pub async fn migrate_all(
    pool: &Pool,
    registry: &Registry,
    apps: &[AppSpec<'_>],
    schedules: Option<&[schedule::Declared<'_>]>,
    options: &SyncOptions,
) -> Result<MigrateReport, DbError> {
    // The connection is taken out of the pool: it holds a session-level lock, so it must never
    // go back to the pool alive. When this function ends, by returning, by a panic or because the
    // future is dropped (cancellation), the connection is closed and PostgreSQL releases the lock.
    let mut client = deadpool_postgres::Object::take(pool.get().await?);
    lock(&client, options).await?;
    let result = migrate_locked(&mut client, registry, apps, schedules, options).await;
    // Released explicitly as well, so a caller that keeps going does not wait for the close. A
    // failure here means the connection is gone, and then so is its lock.
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
    client
        .execute(
            "SELECT set_config('lock_timeout', $1, false)",
            &[&sync::lock_timeout_setting(options.lock_timeout)],
        )
        .await?;
    let locked = client
        .execute("SELECT pg_advisory_lock($1)", &[&migrate::MIGRATE_LOCK_KEY])
        .await;
    // Back to the connection's default, so the migrations that follow have no timeout of their
    // own. This runs before the lock result is looked at, but must not hide it: when both fail,
    // the lock error (the cause) is the one reported.
    let reset = client.batch_execute("RESET lock_timeout").await;
    locked?;
    reset?;
    Ok(())
}

async fn migrate_locked(
    client: &mut Client,
    registry: &Registry,
    apps: &[AppSpec<'_>],
    schedules: Option<&[schedule::Declared<'_>]>,
    options: &SyncOptions,
) -> Result<MigrateReport, DbError> {
    let migrations = migrate_framework(client, options.dry_run).await?;
    let applied = |app: &str, m: &AppMigration<'_>| AppliedAppMigration {
        app: app.to_owned(),
        version: m.version,
        name: m.name.to_owned(),
    };
    if options.dry_run && !migrations.is_empty() {
        // Framework migrations are pending, so the sync plan cannot be computed. The app
        // migrations are still verified when their tables already exist (a later framework
        // migration is pending); when they do not, no app migration can have been applied.
        let app_migrations = if app_migration_tables_exist(client).await? {
            app_migrations::pending(client, apps)
                .await?
                .iter()
                .map(|(app, m)| applied(app, m))
                .collect()
        } else {
            apps.iter()
                .flat_map(|a| a.migrations.iter().map(|m| applied(a.name, m)))
                .collect()
        };
        return Ok(MigrateReport {
            migrations,
            sync: None,
            app_migrations,
            schedules: Vec::new(),
            workflow_warnings: Vec::new(),
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
                app_migrations::apply(client, app.name, migration, options.lock_timeout).await?;
                app_migrations.push(applied(app.name, migration));
            }
            app_migrations::record(client, app).await?;
        }
    }
    let schedules = match schedules {
        Some(declared) => schedule::sync(client, declared, options.dry_run).await?,
        None => Vec::new(),
    };
    // After the sync, so the `workflow_state` columns exist; a dry run keeps nothing, so there
    // is nothing to backfill.
    let workflow_warnings = if options.dry_run {
        Vec::new()
    } else {
        workflow::backfill_workflow_states(client, registry).await?
    };
    Ok(MigrateReport {
        migrations,
        sync: Some(sync),
        app_migrations,
        schedules,
        workflow_warnings,
    })
}
