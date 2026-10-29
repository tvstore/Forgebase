//! Shared helpers: one throwaway database per test, dropped when the test ends.

#![allow(dead_code)] // each test file uses a subset

use std::sync::atomic::{AtomicU32, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

use forgebase_core::meta::{DocType, Registry};
use forgebase_db::{Pool, SyncOptions, connect, migrate_framework, quote_ident, sync};
use tokio_postgres::NoTls;

const ENV_VAR: &str = "FORGEBASE_TEST_DATABASE_URL";

fn admin_url() -> String {
    match std::env::var(ENV_VAR) {
        Ok(url) if !url.is_empty() => url,
        _ => panic!(
            "{ENV_VAR} is not set. The forgebase-db integration tests need a PostgreSQL 16 server; \
             set it to a superuser/CREATEDB URL, for example \
             {ENV_VAR}=postgres://forgebase:forgebase@localhost:5432/postgres (see CONTRIBUTING.md). \
             The tests never skip."
        ),
    }
}

/// Replaces the database name in `postgres://user:pass@host:port/db[?query]`.
fn with_database(url: &str, database: &str) -> String {
    let after_scheme = url.find("://").map_or(0, |i| i + 3);
    let path_start = url[after_scheme..]
        .find('/')
        .map_or(url.len(), |i| after_scheme + i);
    let path_end = url[path_start..]
        .find('?')
        .map_or(url.len(), |i| path_start + i);
    format!("{}/{database}{}", &url[..path_start], &url[path_end..])
}

/// The admin URL with another database, for tests that connect on their own.
pub fn url_for_database(database: &str) -> String {
    with_database(&admin_url(), database)
}

/// The admin URL with another password.
pub fn url_with_password(password: &str) -> String {
    let url = admin_url();
    let start = url.find("://").map_or(0, |i| i + 3);
    let at = url.rfind('@').expect("test URL has user info");
    let colon = url[start..at].find(':').map_or(at, |i| start + i);
    format!("{}:{password}{}", &url[..colon], &url[at..])
}

async fn admin_client(url: &str) -> tokio_postgres::Client {
    let (client, connection) = tokio_postgres::connect(url, NoTls)
        .await
        .unwrap_or_else(|e| panic!("cannot connect to {ENV_VAR}: {e}"));
    tokio::spawn(connection);
    client
}

/// A database created for one test. Dropped (with `FORCE`) when this value is dropped, also
/// when the test panics.
pub struct TestDb {
    pub pool: Pool,
    admin_url: String,
    name: String,
}

impl TestDb {
    /// A fresh, empty database.
    pub async fn new() -> Self {
        static COUNTER: AtomicU32 = AtomicU32::new(0);
        let admin_url = admin_url();
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map_or(0, |d| d.subsec_nanos());
        let name = format!(
            "fb_test_{:x}_{:x}_{}",
            std::process::id(),
            nanos,
            COUNTER.fetch_add(1, Ordering::Relaxed)
        );
        let admin = admin_client(&admin_url).await;
        admin
            .batch_execute(&format!("CREATE DATABASE {}", quote_ident(&name)))
            .await
            .expect("create test database");
        let pool = connect(&with_database(&admin_url, &name))
            .await
            .expect("connect to test database");
        Self {
            pool,
            admin_url,
            name,
        }
    }

    /// The URL of this database, for a test that needs a pool of its own.
    pub fn url(&self) -> String {
        with_database(&self.admin_url, &self.name)
    }

    /// A fresh database with the framework migrations applied.
    pub async fn migrated() -> Self {
        let db = Self::new().await;
        let mut client = db.pool.get().await.expect("connection");
        migrate_framework(&mut client, false)
            .await
            .expect("migrate");
        drop(client);
        db
    }

    /// Runs `sql` on a pooled connection.
    pub async fn exec(&self, sql: &str) {
        let client = self.pool.get().await.expect("connection");
        client
            .batch_execute(sql)
            .await
            .unwrap_or_else(|e| panic!("{sql}: {e:?}"));
    }

    /// Runs a query returning rows of text columns (cast in the query).
    pub async fn rows(&self, sql: &str) -> Vec<Vec<String>> {
        let client = self.pool.get().await.expect("connection");
        let rows = client
            .query(sql, &[])
            .await
            .unwrap_or_else(|e| panic!("{sql}: {e:?}"));
        rows.iter()
            .map(|r| (0..r.len()).map(|i| r.get::<_, String>(i)).collect())
            .collect()
    }

    /// `column -> "<data_type>[(len|precision,scale)]/<nullable>"` of a table, via information_schema.
    pub async fn columns(&self, table: &str) -> Vec<(String, String)> {
        let sql = format!(
            "SELECT column_name::text, data_type::text || coalesce('(' || character_maximum_length || ')', '') \
             || CASE WHEN data_type = 'numeric' THEN '(' || numeric_precision || ',' || numeric_scale || ')' ELSE '' END || '/' || is_nullable::text \
             FROM information_schema.columns WHERE table_schema = current_schema() AND table_name = '{table}' \
             ORDER BY ordinal_position"
        );
        self.rows(&sql)
            .await
            .into_iter()
            .map(|r| (r[0].clone(), r[1].clone()))
            .collect()
    }

    pub async fn sync(
        &self,
        registry: &Registry,
    ) -> Result<forgebase_db::SyncReport, forgebase_db::DbError> {
        let mut client = self.pool.get().await.expect("connection");
        sync(&mut client, registry, &SyncOptions::default()).await
    }
}

impl Drop for TestDb {
    fn drop(&mut self) {
        self.pool.close();
        let (url, name) = (self.admin_url.clone(), self.name.clone());
        // A separate thread and runtime: Drop is synchronous and may run while unwinding.
        let _ = std::thread::spawn(move || {
            let Ok(runtime) = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
            else {
                return;
            };
            runtime.block_on(async {
                let admin = admin_client(&url).await;
                let _ = admin
                    .batch_execute(&format!(
                        "DROP DATABASE IF EXISTS {} WITH (FORCE)",
                        quote_ident(&name)
                    ))
                    .await;
            });
        })
        .join();
    }
}

/// Builds a registry from DocType TOML texts.
pub fn registry(texts: &[&str]) -> Registry {
    let mut registry = Registry::new();
    for text in texts {
        registry
            .add(DocType::from_toml_str(text).expect("valid DocType"))
            .expect("valid registry");
    }
    registry
}

pub const CUSTOMER: &str = r#"
name = "customer"
label = "Customer"
[[fields]]
name = "email"
type = "data"
max_length = 100
required = true
unique = true
[[fields]]
name = "notes"
type = "text"
[[fields]]
name = "points"
type = "int"
[[fields]]
name = "credit_limit"
type = "decimal"
precision = 12
scale = 2
[[fields]]
name = "active"
type = "check"
[[fields]]
name = "joined_on"
type = "date"
[[fields]]
name = "last_seen"
type = "datetime"
[[fields]]
name = "tier"
type = "select"
options = ["gold", "silver"]
"#;

/// A DocType named by its `email`, a typical Link target.
pub const PERSON: &str = r#"
name = "person"
label = "Person"
naming = { field = "email" }
[[fields]]
name = "email"
type = "data"
required = true
unique = true
"#;

/// Links to `person` (optional) and to itself (optional); `approver` is a required link.
pub const SALES_ORDER: &str = r#"
name = "sales_order"
label = "Sales Order"
[[fields]]
name = "title"
type = "data"
[[fields]]
name = "customer"
type = "link"
target = "person"
[[fields]]
name = "follows"
type = "link"
target = "sales_order"
[[fields]]
name = "approver"
type = "link"
target = "person"
required = true
"#;
