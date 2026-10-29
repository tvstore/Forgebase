//! PostgreSQL access: connection pool, framework migrations and DocType schema sync.
//!
//! Depends only on `forgebase-core`. See ADR 0002 and `docs/blueprint/architecture.md` (section 5).
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
//! - [`migrate`]: both of the above in order.
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
//!   cookies yet; [`auth::AuthUser::caller`] gives the [`Caller`] for the document functions.

pub mod auth;
mod document;
mod error;
mod ident;
mod list;
mod migrate;
mod plan;
mod sync;
mod value;

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
pub use tokio_postgres::{Client, GenericClient, Transaction};

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
}

/// Applies the framework migrations, then syncs the DocType schema.
///
/// The two steps use separate transactions: the migrations stay applied if the sync fails
/// (including with [`DbError::LockTimeout`]); only the sync step is not applied.
pub async fn migrate(
    pool: &Pool,
    registry: &Registry,
    options: &SyncOptions,
) -> Result<MigrateReport, DbError> {
    let mut client = pool.get().await?;
    let migrations = migrate_framework(&mut client, options.dry_run).await?;
    if options.dry_run && !migrations.is_empty() {
        return Ok(MigrateReport {
            migrations,
            sync: None,
        });
    }
    let sync = sync(&mut client, registry, options).await?;
    Ok(MigrateReport {
        migrations,
        sync: Some(sync),
    })
}
