//! PostgreSQL access: connection pool, framework migrations and DocType schema sync.
//!
//! Depends only on `forgebase-core`. See ADR 0002 and `docs/blueprint/architecture.md` (section 5).
//!
//! Implemented so far (milestones M1.2, M1.3, M2.1 and M2.2):
//!
//! - [`connect`]: a connection pool. TLS to PostgreSQL is **not supported yet** (planned:
//!   rustls); until then run over a local or private network.
//! - [`migrate_framework`]: Forgebase's own tables (`fb_migration`, `fb_doctype`).
//! - [`sync`]: additive DocType schema sync with dry-run, conflict refusal and `lock_timeout`;
//!   Link fields become `uuid` columns with a foreign key (`ON DELETE RESTRICT`) and an index; a
//!   child DocType's table has `id`, `parent_id` (foreign key `ON DELETE CASCADE`, indexed) and
//!   `idx`.
//! - [`migrate`]: both of the above in order.
//! - [`quote_ident`]: quoting for every identifier in generated SQL.
//! - [`insert`], [`get`], [`update`], [`delete`]: one document (with its child table rows) at a
//!   time, on a pooled client or a caller-owned [`Transaction`]; each call runs in its own
//!   transaction (a savepoint inside a caller's). They take the [`Registry`] and the DocType
//!   name. Link values are target names, resolved to ids internally; `update` and `delete` need
//!   the `expected_version` (optimistic concurrency); an update replaces the rows of the Table
//!   fields it names. No permissions and no renames yet.

mod document;
mod error;
mod ident;
mod migrate;
mod plan;
mod sync;
mod value;

pub use deadpool_postgres::Pool;
pub use document::{StoredDocument, delete, get, insert, update};
pub use error::DbError;
pub use ident::quote_ident;
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
