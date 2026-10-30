//! Helpers for the tests of an app (cargo feature `testing`).
//!
//! Enable the feature in the app's `[dev-dependencies]`:
//!
//! ```toml
//! [dev-dependencies]
//! forgebase = { path = "../forgebase", features = ["testing"] }
//! tokio = { version = "1", features = ["macros", "rt-multi-thread"] }
//! ```
//!
//! What it gives: [`run_jobs`] runs the background jobs that are due, with the worker's own code.
//! [`TestDb`], a throwaway PostgreSQL database that can run `migrate` for a
//! [`Forgebase`] and hands out an [`HttpClient`] that talks to the HTTP API in process (no
//! socket), and [`assert_doctypes_listed`], a guard for the embedded DocType list.
//!
//! ```ignore
//! #[tokio::test]
//! async fn an_order_needs_a_customer() {
//!     let binary = Forgebase::new().app(crm::app()).app(sales::app());
//!     let db = TestDb::new().await;
//!     db.migrate(&binary, false).await.expect("migrate");
//!     db.create_user("ann", &["Sales User"]).await;
//!     let ann = db.client(&binary).login("ann").await;
//!     let reply = ann.post("/api/v1/resource/sales_order", json!({"total": 5})).await;
//!     assert_eq!(reply.status, 422);
//!     db.cleanup().await;
//! }
//! ```
//!
//! The database comes from the environment variable `FORGEBASE_TEST_DATABASE_URL`: the URL of a
//! PostgreSQL 16 server with a superuser or `CREATEDB` role, for example
//! `postgres://forgebase:forgebase@localhost:5432/postgres`. Every [`TestDb`] creates its own
//! database (`fb_test_app_*`) and drops it with [`TestDb::cleanup`] or, as a safety net, when
//! it is dropped. These helpers are for tests: they panic with a clear message instead of
//! returning errors, except where the test wants to look at the error.

use std::path::Path;
use std::sync::Arc;
use std::sync::atomic::{AtomicU32, Ordering};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use axum::Router;
use axum::body::Body;
use axum::http::{Method, Request};
use forgebase_db::{Pool, SyncOptions, auth, connect, quote_ident};
use forgebase_http::{AppConfig, AppState, router};
use tower::ServiceExt;

use crate::app::App;
use crate::builder::Forgebase;
use crate::compose::{StartupError, compose};
use crate::doctypes::toml_files;
use crate::worker::{Outcome, Stop, Worker, WorkerConfig};

/// JSON, for request bodies and replies: `serde_json`, re-exported so that the version matches.
pub use serde_json::{self, Value as Json, json};

const ENV_VAR: &str = "FORGEBASE_TEST_DATABASE_URL";

/// The password of every user made with [`TestDb::create_user`].
pub const PASSWORD: &str = "correct horse battery";

fn admin_url() -> String {
    match std::env::var(ENV_VAR) {
        Ok(url) if !url.is_empty() => url,
        _ => panic!(
            "{ENV_VAR} is not set. forgebase::testing needs a PostgreSQL 16 server; set it to a \
             superuser/CREATEDB URL, for example \
             {ENV_VAR}=postgres://forgebase:forgebase@localhost:5432/postgres. Tests never skip."
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

/// Why [`TestDb::migrate`] failed. `Display` gives the message `forgebase migrate` would print.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[non_exhaustive]
pub enum MigrateError {
    /// The apps do not fit together (nothing was run).
    #[error("{0}")]
    Startup(#[from] StartupError),
    /// The migration failed: a refused schema sync, a changed or unknown applied migration, a
    /// failing migration SQL, a lock timeout or a database error. The message says which.
    #[error("{0}")]
    Failed(String),
}

/// A migration that [`TestDb::migrate`] applied (in a dry run: would apply). Read the fields;
/// build one to compare with [`AppliedMigration::new`] (the type is `#[non_exhaustive]`, so a
/// field can be added later without breaking tests).
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub struct AppliedMigration {
    /// The app.
    pub app: String,
    /// The migration version.
    pub version: i32,
    /// The migration name.
    pub name: String,
}

impl AppliedMigration {
    /// A migration from its app, version and name, for comparing with what
    /// [`TestDb::migrate`] returned.
    pub fn new(app: &str, version: i32, name: &str) -> Self {
        Self {
            app: app.to_owned(),
            version,
            name: name.to_owned(),
        }
    }
}

/// What [`TestDb::migrate`] did. Only read it: the type is `#[non_exhaustive]`, so a field can
/// be added later without breaking tests.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub struct Migrated {
    /// The app migrations applied, in the order they ran; with a dry run, the pending ones.
    pub app_migrations: Vec<AppliedMigration>,
}

/// A database created for one test, and dropped by [`TestDb::cleanup`] or when it is dropped
/// (also when the test panics).
pub struct TestDb {
    pub(crate) pool: Pool,
    admin_url: String,
    name: String,
    url: String,
    dropped: bool,
}

impl TestDb {
    /// Creates an empty database. Nothing is migrated yet ([`TestDb::migrate`]).
    ///
    /// # Panics
    ///
    /// When `FORGEBASE_TEST_DATABASE_URL` is not set or the server cannot be used.
    pub async fn new() -> Self {
        static COUNTER: AtomicU32 = AtomicU32::new(0);
        let admin_url = admin_url();
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map_or(0, |d| d.subsec_nanos());
        let name = format!(
            "fb_test_app_{:x}_{:x}_{}",
            std::process::id(),
            nanos,
            COUNTER.fetch_add(1, Ordering::Relaxed)
        );
        let admin = connect(&admin_url)
            .await
            .unwrap_or_else(|e| panic!("cannot connect to {ENV_VAR}: {e}"));
        admin
            .get()
            .await
            .expect("connection")
            .batch_execute(&format!("CREATE DATABASE {}", quote_ident(&name)))
            .await
            .expect("create the test database");
        admin.close();
        let url = with_database(&admin_url, &name);
        let pool = connect(&url).await.expect("connect to the test database");
        Self {
            pool,
            admin_url,
            name,
            url,
            dropped: false,
        }
    }

    /// The URL of this database, for code that connects on its own.
    pub fn url(&self) -> &str {
        &self.url
    }

    /// Runs what `forgebase migrate` runs for `forgebase`: the framework migrations, the
    /// DocType schema sync and the app migrations in app order. With `dry_run` nothing is kept.
    pub async fn migrate(
        &self,
        forgebase: &Forgebase,
        dry_run: bool,
    ) -> Result<Migrated, MigrateError> {
        let composed = compose(&forgebase.apps, None)?;
        let options = SyncOptions {
            dry_run,
            lock_timeout: Duration::from_secs(5),
        };
        let report = composed
            .migrate(&self.pool, &options)
            .await
            .map_err(|e| MigrateError::Failed(e.to_string()))?;
        Ok(Migrated {
            app_migrations: report
                .app_migrations
                .into_iter()
                .map(|m| AppliedMigration {
                    app: m.app,
                    version: m.version,
                    name: m.name,
                })
                .collect(),
        })
    }

    /// Runs SQL (several statements are fine).
    ///
    /// # Panics
    ///
    /// When the SQL fails.
    pub async fn exec(&self, sql: &str) {
        let client = self.pool.get().await.expect("connection");
        client
            .batch_execute(sql)
            .await
            .unwrap_or_else(|e| panic!("{sql}: {e:?}"));
    }

    /// Runs a query and returns its rows as text (every value as PostgreSQL prints it, `NULL`
    /// for null). Cast to `::text` in the SQL when the format matters.
    ///
    /// # Panics
    ///
    /// When the SQL fails.
    pub async fn rows(&self, sql: &str) -> Vec<Vec<String>> {
        use forgebase_db::SimpleQueryMessage;
        let client = self.pool.get().await.expect("connection");
        let messages = client
            .simple_query(sql)
            .await
            .unwrap_or_else(|e| panic!("{sql}: {e:?}"));
        messages
            .into_iter()
            .filter_map(|message| match message {
                SimpleQueryMessage::Row(row) => Some(
                    (0..row.len())
                        .map(|i| row.get(i).unwrap_or("NULL").to_owned())
                        .collect(),
                ),
                _ => None,
            })
            .collect()
    }

    /// Creates an enabled user with `roles` and the password [`PASSWORD`]. Needs the framework
    /// tables ([`TestDb::migrate`] first).
    ///
    /// # Panics
    ///
    /// When the user cannot be created.
    pub async fn create_user(&self, username: &str, roles: &[&str]) {
        let mut client = self.pool.get().await.expect("connection");
        auth::create_user(&mut **client, username, PASSWORD, roles)
            .await
            .expect("create the user");
    }

    /// An HTTP client for the API of `forgebase` on this database, not logged in. The database
    /// must have been migrated.
    ///
    /// # Panics
    ///
    /// When the apps of `forgebase` do not fit together ([`Forgebase::check`]).
    pub fn client(&self, forgebase: &Forgebase) -> HttpClient {
        let composed = compose(&forgebase.apps, None)
            .unwrap_or_else(|e| panic!("the apps do not fit together: {e}"));
        let state = AppState::new(
            self.pool.clone(),
            Arc::new(composed.registry),
            AppConfig::default(),
        );
        HttpClient {
            router: router(state),
            session: None,
        }
    }

    /// Drops the database now. Dropping the `TestDb` does the same, in a helper thread.
    pub async fn cleanup(mut self) {
        self.dropped = true;
        self.pool.close();
        drop_database(&self.admin_url, &self.name).await;
    }
}

/// What [`run_jobs`] did. Only read it: the type is `#[non_exhaustive]`, so a field can be added
/// later without breaking tests.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
#[non_exhaustive]
pub struct JobRun {
    done: usize,
    retried: usize,
    dead: usize,
}

impl JobRun {
    /// Jobs that ended `done`.
    pub fn done(&self) -> usize {
        self.done
    }

    /// Attempts that failed and were queued again (after a delay; see [`run_jobs`]).
    pub fn retried(&self) -> usize {
        self.retried
    }

    /// Jobs that ended `dead`.
    pub fn dead(&self) -> usize {
        self.dead
    }
}

/// The most jobs [`run_jobs`] runs: more means a job keeps enqueueing work for itself.
const RUN_JOBS_LIMIT: usize = 10_000;

/// Runs every job of `forgebase` that is due, one at a time, until none is due, with the same
/// code the worker uses (claim, handler in one transaction with the completion, retry or
/// dead). Jobs that hooks and other jobs enqueue while it runs are run too.
///
/// A job that fails and is retried is queued again **after its backoff** (30 seconds by
/// default), so it is not run again by this call: `retried()` counts it, and the test can check
/// the row, or make it due with SQL (`UPDATE fb_job SET run_at = now()`) and call `run_jobs`
/// again. Jobs of names the binary does not know are left alone.
///
/// # Panics
///
/// When the apps do not fit together, on a database error, and after 10 000 jobs (a job that
/// enqueues itself without end).
pub async fn run_jobs(forgebase: &Forgebase, db: &TestDb) -> JobRun {
    let composed = compose(&forgebase.apps, None)
        .unwrap_or_else(|e| panic!("the apps do not fit together: {e}"));
    let worker = Worker::new(
        db.pool.clone(),
        composed.registry,
        composed.jobs,
        WorkerConfig::default(),
    );
    let stop = Stop::never();
    let mut run = JobRun::default();
    for _ in 0..RUN_JOBS_LIMIT {
        match worker.run_one("test", &stop).await {
            Ok(Some(Outcome::Done)) => run.done += 1,
            Ok(Some(Outcome::Retried)) => run.retried += 1,
            Ok(Some(Outcome::Dead)) => run.dead += 1,
            Ok(Some(_)) => {}
            Ok(None) => return run,
            Err(error) => panic!("run_jobs: {error}"),
        }
    }
    panic!(
        "run_jobs stopped after {RUN_JOBS_LIMIT} jobs: a job probably enqueues itself again and again"
    );
}

async fn drop_database(admin_url: &str, name: &str) {
    let Ok(admin) = connect(admin_url).await else {
        return;
    };
    if let Ok(client) = admin.get().await {
        let _ = client
            .batch_execute(&format!(
                "DROP DATABASE IF EXISTS {} WITH (FORCE)",
                quote_ident(name)
            ))
            .await;
    }
}

impl Drop for TestDb {
    fn drop(&mut self) {
        if self.dropped {
            return;
        }
        self.pool.close();
        let (url, name) = (self.admin_url.clone(), self.name.clone());
        // A separate thread and runtime: Drop is synchronous and may run while unwinding.
        let _ = std::thread::spawn(move || {
            if let Ok(runtime) = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
            {
                runtime.block_on(drop_database(&url, &name));
            }
        })
        .join();
    }
}

/// What a request got back. Only read it: the type is `#[non_exhaustive]`, so a field can be
/// added later without breaking tests.
#[derive(Debug, Clone)]
#[non_exhaustive]
pub struct Reply {
    /// The HTTP status code.
    pub status: u16,
    /// The JSON body; `null` when the reply has no body (204) or it is not JSON.
    pub body: Json,
}

impl Reply {
    /// `data` of a success reply.
    pub fn data(&self) -> &Json {
        &self.body["data"]
    }

    /// `error.code` of an error reply.
    pub fn error_code(&self) -> Option<&str> {
        self.body["error"]["code"].as_str()
    }

    /// `error.message` of an error reply.
    pub fn error_message(&self) -> Option<&str> {
        self.body["error"]["message"].as_str()
    }
}

#[derive(Clone)]
struct Session {
    cookie: String,
    csrf: String,
}

/// A client for the HTTP API that sends requests straight into the router (no socket). Paths
/// are the full ones (`/api/v1/resource/customer`). After [`HttpClient::login`] the session
/// cookie and the CSRF token are sent with every request.
#[derive(Clone)]
pub struct HttpClient {
    router: Router,
    session: Option<Session>,
}

impl HttpClient {
    /// Logs in as `username` (a user made with [`TestDb::create_user`]) and returns a client
    /// that carries the session.
    ///
    /// # Panics
    ///
    /// When the login fails.
    pub async fn login(&self, username: &str) -> HttpClient {
        let (reply, cookie) = self
            .send_with_cookie(
                Method::POST,
                "/api/v1/login",
                Some(json!({
                    "username": username,
                    "password": PASSWORD,
                })),
                None,
            )
            .await;
        assert_eq!(
            reply.status, 200,
            "login of {username} failed: {:?}",
            reply.body
        );
        let cookie = cookie.expect("the login sets the session cookie");
        let csrf = reply.data()["csrf_token"]
            .as_str()
            .expect("the login returns a CSRF token")
            .to_owned();
        HttpClient {
            router: self.router.clone(),
            session: Some(Session { cookie, csrf }),
        }
    }

    /// `GET path`.
    pub async fn get(&self, path: &str) -> Reply {
        self.send(Method::GET, path, None).await
    }

    /// `POST path` with a JSON body.
    pub async fn post(&self, path: &str, body: Json) -> Reply {
        self.send(Method::POST, path, Some(body)).await
    }

    /// `PATCH path` with a JSON body.
    pub async fn patch(&self, path: &str, body: Json) -> Reply {
        self.send(Method::PATCH, path, Some(body)).await
    }

    /// `DELETE path`.
    pub async fn delete(&self, path: &str) -> Reply {
        self.send(Method::DELETE, path, None).await
    }

    async fn send(&self, method: Method, path: &str, body: Option<Json>) -> Reply {
        self.send_with_cookie(method, path, body, self.session.as_ref())
            .await
            .0
    }

    /// Sends a request; returns the reply and the value of the session cookie it set, if any.
    async fn send_with_cookie(
        &self,
        method: Method,
        path: &str,
        body: Option<Json>,
        session: Option<&Session>,
    ) -> (Reply, Option<String>) {
        let mut builder = Request::builder()
            .method(method)
            .uri(path)
            .header("host", "localhost");
        if let Some(session) = session {
            builder = builder
                .header("cookie", format!("fb_session={}", session.cookie))
                .header("x-forgebase-csrf", &session.csrf);
        }
        let body = match body {
            Some(json) => {
                builder = builder.header("content-type", "application/json");
                Body::from(serde_json::to_vec(&json).expect("JSON"))
            }
            None => Body::empty(),
        };
        let response = self
            .router
            .clone()
            .oneshot(builder.body(body).expect("request"))
            .await
            .expect("infallible");
        let (parts, body) = response.into_parts();
        let cookie = parts
            .headers
            .get_all("set-cookie")
            .iter()
            .filter_map(|v| v.to_str().ok())
            .find_map(|c| c.strip_prefix("fb_session="))
            .and_then(|rest| rest.split(';').next())
            .map(str::to_owned);
        let bytes = axum::body::to_bytes(body, usize::MAX).await.expect("body");
        let reply = Reply {
            status: parts.status.as_u16(),
            body: serde_json::from_slice(&bytes).unwrap_or(Json::Null),
        };
        (reply, cookie)
    }
}

/// Fails the test when the DocType files in a directory of the app crate and the DocType files
/// embedded in `app` are not the same set: a file in the directory that the app does not list
/// (it would silently be missing from the binary), or a listed file that is not in the
/// directory.
///
/// `manifest_dir` is the root of the app crate (`env!("CARGO_MANIFEST_DIR")`, evaluated in the
/// app's own test) and `dir` the directory with the DocType files, relative to it. Paths are
/// compared in full, relative to the crate root, as they are written in
/// [`doctypes!`](crate::doctypes): `doctypes/customer.toml`, not just `customer.toml`, so a file
/// listed from another directory does not count.
///
/// ```ignore
/// #[test]
/// fn every_doctype_file_is_listed() {
///     forgebase::testing::assert_doctypes_listed(&app(), env!("CARGO_MANIFEST_DIR"), "doctypes");
/// }
/// ```
///
/// A leading `./` is ignored in `dir` and in the listed paths.
///
/// The same file rules as `--doctypes DIR` apply: `*.toml` files directly in the directory,
/// hidden files and subdirectories ignored.
///
/// # Panics
///
/// On a difference, or when the directory cannot be read.
pub fn assert_doctypes_listed(app: &App, manifest_dir: &str, dir: &str) {
    let relative_dir = dir.trim_start_matches("./").trim_end_matches('/');
    let full = Path::new(manifest_dir).join(relative_dir);
    let (files, problems) = toml_files(&full).unwrap_or_else(|problem| panic!("{problem}"));
    assert!(
        problems.is_empty(),
        "DocType directory {}: {problems:?}",
        full.display()
    );
    let on_disk: Vec<String> = files
        .iter()
        .filter_map(|path| path.file_name())
        .map(|name| {
            if relative_dir.is_empty() {
                name.to_string_lossy().into_owned()
            } else {
                format!("{relative_dir}/{}", name.to_string_lossy())
            }
        })
        .collect();
    // `doctypes!["./doctypes/x.toml"]` is the same file as `doctypes!["doctypes/x.toml"]`.
    let listed: Vec<&str> = app
        .doctypes
        .iter()
        .map(|file| file.path().trim_start_matches("./"))
        .collect();
    let not_listed: Vec<&String> = on_disk
        .iter()
        .filter(|n| !listed.contains(&n.as_str()))
        .collect();
    let not_found: Vec<&&str> = listed
        .iter()
        .filter(|n| !on_disk.iter().any(|d| d == **n))
        .collect();
    assert!(
        not_listed.is_empty() && not_found.is_empty(),
        "app `{}`: the embedded DocType list and {} differ; in the directory but not in \
         doctypes![..]: {not_listed:?}; in doctypes![..] but not in the directory: {not_found:?}",
        app.name,
        full.display()
    );
}
