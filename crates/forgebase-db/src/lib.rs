//! PostgreSQL access: connection pool, framework migrations and DocType schema sync.
//!
//! Depends only on `forgebase-core`. See ADR 0002 and `docs/blueprint/architecture.md` (section 5).
//!
//! Implemented so far (milestone M1.2):
//!
//! - [`connect`]: a connection pool. TLS to PostgreSQL is **not supported yet** (planned:
//!   rustls); until then run over a local or private network.
//! - [`migrate_framework`]: Forgebase's own tables (`fb_migration`, `fb_doctype`).
//! - [`sync`]: additive DocType schema sync with dry-run, conflict refusal and `lock_timeout`.
//! - [`migrate`]: both of the above in order.
//! - [`quote_ident`]: quoting for every identifier in generated SQL.

mod error;
mod ident;
mod migrate;
mod plan;
mod sync;

pub use deadpool_postgres::Pool;
pub use error::DbError;
pub use ident::quote_ident;
pub use migrate::{MIGRATIONS, Migration, migrate_framework};
pub use plan::{Conflict, Step, Warning};
pub use sync::{SyncOptions, SyncReport, sync};
pub use tokio_postgres::Client;

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
