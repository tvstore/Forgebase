//! The registry of one running site, replaceable while it runs (M10.1, ADR 0023).
//!
//! A site's effective metadata is the registry of the apps plus the custom fields stored in its
//! database. Custom fields change while a server or worker runs, so the process keeps the
//! registry behind a [`RegistryHolder`]:
//!
//! - **Snapshots.** [`RegistryHolder::snapshot`] returns an `Arc<Registry>`. A request takes one
//!   at its start and uses it to the end, so it never sees a half-built registry or two
//!   different ones; a registry that is swapped out lives until the last request that holds it
//!   finishes.
//! - **Atomic swap.** A rebuild builds the new registry completely, then replaces the `Arc`
//!   under a write lock held for the length of one assignment. Readers never wait for a rebuild.
//! - **Version polling.** [`RegistryHolder::poll`] reads `fb_meta_version` and rebuilds only when
//!   it differs from the version the current registry was built from; a server calls it every
//!   `meta_reload_interval`. [`RegistryHolder::reload`] rebuilds unconditionally (the process that
//!   changed a field uses it to see the change at once). Rebuilds are serialised, so an old
//!   build can never replace a newer one.
//! - **No request path I/O.** Nothing here runs per request.

use std::sync::{Arc, RwLock};

use deadpool_postgres::Pool;
use forgebase_core::meta::Registry;
use tokio::sync::Mutex;

use crate::custom_fields::effective_registry;
use crate::error::DbError;

/// The current registry of a site, and what it takes to rebuild it.
pub struct RegistryHolder {
    site: String,
    base: Arc<Registry>,
    current: RwLock<Arc<Registry>>,
    /// The `fb_meta_version` of `current`, `None` before the first build. The lock serialises
    /// rebuilds.
    built_from: Mutex<Option<i64>>,
}

impl RegistryHolder {
    /// A holder that serves `current` until the first rebuild. `base` is the registry of the apps
    /// and their overlays (what every rebuild starts from) and `site` names the site in warnings.
    pub fn new(site: &str, base: Arc<Registry>, current: Arc<Registry>) -> Self {
        Self {
            site: site.to_owned(),
            base,
            current: RwLock::new(current),
            built_from: Mutex::new(None),
        }
    }

    /// A holder whose registry is already built from the database (the first build of a site).
    /// Warnings of fields that were skipped are printed to standard error.
    pub async fn load(site: &str, base: Arc<Registry>, pool: &Pool) -> Result<Self, DbError> {
        let holder = Self::new(site, Arc::clone(&base), base);
        holder.reload(pool).await?;
        Ok(holder)
    }

    /// The registry as it is now. Cheap; take one per request and keep it.
    pub fn snapshot(&self) -> Arc<Registry> {
        // A poisoned lock only means that a thread panicked while holding it; the `Arc` inside
        // is always a whole registry, so it is still good.
        match self.current.read() {
            Ok(guard) => Arc::clone(&guard),
            Err(poisoned) => Arc::clone(&poisoned.into_inner()),
        }
    }

    /// The registry of the apps and their overlays, without custom fields.
    pub fn base(&self) -> &Arc<Registry> {
        &self.base
    }

    /// The site name used in warnings.
    pub fn site(&self) -> &str {
        &self.site
    }

    /// Replaces the registry.
    fn swap(&self, next: Arc<Registry>) {
        match self.current.write() {
            Ok(mut guard) => *guard = next,
            Err(poisoned) => *poisoned.into_inner() = next,
        }
    }

    /// Rebuilds the registry from the database now and swaps it in. Returns the version it was
    /// built from.
    pub async fn reload(&self, pool: &Pool) -> Result<i64, DbError> {
        let mut built_from = self.built_from.lock().await;
        self.rebuild(pool, &mut built_from).await
    }

    /// Reads the metadata version and rebuilds the registry when it is not the one the current
    /// registry was built from. Returns whether it rebuilt. A failure (the database is down) is
    /// returned and the current registry is kept.
    pub async fn poll(&self, pool: &Pool) -> Result<bool, DbError> {
        let pooled = pool.get().await?;
        let client: &tokio_postgres::Client = &pooled;
        let version = crate::custom_fields::meta_version(client).await?;
        drop(pooled);
        let mut built_from = self.built_from.lock().await;
        if *built_from == Some(version) {
            return Ok(false);
        }
        self.rebuild(pool, &mut built_from).await?;
        Ok(true)
    }

    async fn rebuild(&self, pool: &Pool, built_from: &mut Option<i64>) -> Result<i64, DbError> {
        let mut pooled = pool.get().await?;
        let client: &mut tokio_postgres::Client = &mut pooled;
        // One snapshot for the version and the rows, so they belong together.
        let tx = client
            .build_transaction()
            .isolation_level(tokio_postgres::IsolationLevel::RepeatableRead)
            .read_only(true)
            .start()
            .await?;
        let built = effective_registry(&tx, &self.base, &self.site, false).await?;
        tx.rollback().await?;
        drop(pooled);
        for warning in &built.warnings {
            // A warning for every rebuild that meets the field: a bad field stays visible
            // until someone fixes or disables it.
            eprintln!("warning: {warning}");
        }
        self.swap(Arc::new(built.registry));
        *built_from = Some(built.version);
        Ok(built.version)
    }
}

impl std::fmt::Debug for RegistryHolder {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("RegistryHolder")
            .field("site", &self.site)
            .finish_non_exhaustive()
    }
}
