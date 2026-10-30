//! Shared helpers: one throwaway database per test with the framework tables and the test
//! DocTypes, and a small client that sends requests straight into the router (`oneshot`).

#![allow(dead_code)] // each test file uses a subset

use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{SystemTime, UNIX_EPOCH};

use axum::Router;
use axum::body::Body;
use axum::http::{HeaderMap, Method, Request, StatusCode};
use forgebase_core::meta::{DocType, Registry};
use forgebase_db::auth;
use forgebase_db::{Pool, SyncOptions, connect, migrate_framework, quote_ident, sync};
use forgebase_http::{AppConfig, AppState, router};
use serde_json::Value as Json;
use tower::ServiceExt;

const ENV_VAR: &str = "FORGEBASE_TEST_DATABASE_URL";
pub const PASSWORD: &str = "correct horse battery";

fn admin_url() -> String {
    match std::env::var(ENV_VAR) {
        Ok(url) if !url.is_empty() => url,
        _ => panic!(
            "{ENV_VAR} is not set. The forgebase-http integration tests need a PostgreSQL 16 server; \
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

/// `item` is a Link target that `Clerk` may create and delete.
const ITEM: &str = r#"
name = "item"
label = "Item"
naming = { field = "code" }
[[fields]]
name = "code"
type = "data"
required = true
unique = true
[[permissions]]
role = "Reader"
read = true
[[permissions]]
role = "Clerk"
read = true
create = true
delete = true
"#;

/// A Link to a DocType the Clerk may read (`item`) and to one nobody but Administrator may
/// read (`vault`).
const HOLDER: &str = r#"
name = "holder"
label = "Holder"
[[fields]]
name = "title"
type = "data"
required = true
[[fields]]
name = "item"
type = "link"
target = "item"
[[fields]]
name = "vault"
type = "link"
target = "vault"
[[permissions]]
role = "Reader"
read = true
[[permissions]]
role = "Clerk"
read = true
create = true
write = true
delete = true
"#;

const VAULT: &str = r#"
name = "vault"
label = "Vault"
naming = { field = "code" }
[[fields]]
name = "code"
type = "data"
required = true
unique = true
"#;

/// No rules, and a Link to `item`: a document here blocks the deletion of its item.
const LOCKED: &str = r#"
name = "locked"
label = "Locked"
[[fields]]
name = "item"
type = "link"
target = "item"
"#;

/// `Reader` reads, `Writer` has every right except the lifecycle ones, `Own` only on its own.
const NOTE: &str = r#"
name = "note"
label = "Note"
[[fields]]
name = "title"
type = "data"
required = true
[[fields]]
name = "body"
type = "text"
[[fields]]
name = "points"
type = "int"
[[fields]]
name = "lines"
type = "table"
target = "note_line"
[[permissions]]
role = "Reader"
read = true
[[permissions]]
role = "Writer"
read = true
write = true
create = true
delete = true
[[permissions]]
role = "Own"
read = true
write = true
create = true
delete = true
if_owner = true
"#;

const NOTE_LINE: &str = r#"
name = "note_line"
label = "Note Line"
is_child = true
[[fields]]
name = "text"
type = "data"
"#;

const BILL: &str = r#"
name = "bill"
label = "Bill"
submittable = true
[[fields]]
name = "title"
type = "data"
required = true
[[permissions]]
role = "Viewer"
read = true
[[permissions]]
role = "Biller"
read = true
write = true
create = true
delete = true
submit = true
cancel = true
amend = true
"#;

/// Read-only fields (a computed `total` and a computed line `amount`), a title and list fields.
/// `Keeper` has every plain right.
const LEDGER: &str = r#"
name = "ledger"
label = "Ledger"
title_field = "title"
list_fields = ["title", "total"]
[[fields]]
name = "title"
type = "data"
required = true
[[fields]]
name = "total"
type = "int"
read_only = true
[[fields]]
name = "lines"
type = "table"
target = "ledger_line"
[[permissions]]
role = "Keeper"
read = true
write = true
create = true
delete = true
"#;

const LEDGER_LINE: &str = r#"
name = "ledger_line"
label = "Ledger Line"
is_child = true
[[fields]]
name = "qty"
type = "int"
[[fields]]
name = "amount"
type = "int"
read_only = true
"#;

/// No rules at all: closed to everyone but Administrator.
const SECRET: &str = r#"
name = "secret"
label = "Secret"
[[fields]]
name = "x"
type = "data"
"#;

/// `Dropper` may write and create but not read: the update pre-check must refuse it.
const DROPBOX: &str = r#"
name = "dropbox"
label = "Dropbox"
[[fields]]
name = "x"
type = "data"
[[permissions]]
role = "Dropper"
write = true
create = true
"#;

pub fn registry() -> Registry {
    let mut registry = Registry::new();
    for text in [
        ITEM,
        HOLDER,
        VAULT,
        LOCKED,
        NOTE,
        NOTE_LINE,
        BILL,
        LEDGER,
        LEDGER_LINE,
        SECRET,
        DROPBOX,
    ] {
        registry
            .add(DocType::from_toml_str(text).expect("valid DocType"))
            .expect("valid registry");
    }
    registry
}

/// What a request got back.
#[derive(Debug, Clone)]
pub struct Reply {
    pub status: StatusCode,
    pub headers: HeaderMap,
    pub text: String,
}

impl Reply {
    pub fn json(&self) -> Json {
        serde_json::from_str(&self.text)
            .unwrap_or_else(|e| panic!("not JSON ({e}): {:?}", self.text))
    }

    /// `data` of a success response.
    pub fn data(&self) -> Json {
        self.json()["data"].clone()
    }

    /// `error.code` of an error response.
    pub fn code(&self) -> String {
        self.json()["error"]["code"]
            .as_str()
            .unwrap_or_else(|| panic!("no error code in {:?}", self.text))
            .to_owned()
    }

    pub fn header(&self, name: &str) -> Option<String> {
        self.headers
            .get(name)
            .map(|v| v.to_str().expect("ASCII header").to_owned())
    }

    pub fn set_cookies(&self) -> Vec<String> {
        self.headers
            .get_all("set-cookie")
            .iter()
            .map(|v| v.to_str().expect("ASCII header").to_owned())
            .collect()
    }
}

/// A login: the cookie value and the CSRF token.
#[derive(Debug, Clone)]
pub struct Session {
    pub token: String,
    pub csrf: String,
}

/// A request under construction.
pub struct Req {
    method: Method,
    uri: String,
    headers: Vec<(String, String)>,
    host: Option<String>,
    body: Option<(String, Vec<u8>)>,
}

pub fn req(method: Method, uri: &str) -> Req {
    Req {
        method,
        uri: uri.to_owned(),
        headers: Vec::new(),
        host: Some("localhost".to_owned()),
        body: None,
    }
}

pub fn get(uri: &str) -> Req {
    req(Method::GET, uri)
}

pub fn post(uri: &str) -> Req {
    req(Method::POST, uri)
}

impl Req {
    pub fn header(mut self, name: &str, value: &str) -> Self {
        self.headers.push((name.to_owned(), value.to_owned()));
        self
    }

    pub fn host(mut self, host: Option<&str>) -> Self {
        self.host = host.map(str::to_owned);
        self
    }

    /// The session cookie only.
    pub fn cookie(self, session: &Session) -> Self {
        self.cookie_value(&session.token)
    }

    pub fn cookie_value(self, value: &str) -> Self {
        self.header("cookie", &format!("fb_session={value}"))
    }

    pub fn csrf(self, token: &str) -> Self {
        self.header("x-forgebase-csrf", token)
    }

    /// The session cookie and its CSRF token.
    pub fn session(self, session: &Session) -> Self {
        self.cookie(session).csrf(&session.csrf)
    }

    /// `Authorization: token <key>:<secret>`.
    pub fn token(self, key_secret: &str) -> Self {
        self.header("authorization", &format!("token {key_secret}"))
    }

    pub fn json(mut self, body: Json) -> Self {
        self.body = Some((
            "application/json".to_owned(),
            serde_json::to_vec(&body).expect("JSON"),
        ));
        self
    }

    pub fn raw(mut self, content_type: &str, body: &[u8]) -> Self {
        self.body = Some((content_type.to_owned(), body.to_vec()));
        self
    }
}

/// A database, the router on it and a log of every reply.
pub struct Env {
    pub db: TestDb,
    pub state: AppState,
    app: Router,
    pub log: Mutex<Vec<Reply>>,
}

impl Env {
    pub async fn new() -> Self {
        Self::with_config(AppConfig::default()).await
    }

    pub async fn with_config(config: AppConfig) -> Self {
        let db = TestDb::migrated().await;
        let registry = registry();
        {
            let mut client = db.pool.get().await.expect("connection");
            sync(&mut client, &registry, &SyncOptions::default())
                .await
                .expect("sync");
        }
        let state = AppState::new(db.pool.clone(), Arc::new(registry), config);
        let app = router(state.clone());
        Self {
            db,
            state,
            app,
            log: Mutex::new(Vec::new()),
        }
    }

    pub async fn send(&self, request: Req) -> Reply {
        let mut builder = Request::builder().method(request.method).uri(&request.uri);
        if let Some(host) = &request.host {
            builder = builder.header("host", host);
        }
        for (name, value) in &request.headers {
            builder = builder.header(name, value);
        }
        let body = match request.body {
            Some((content_type, bytes)) => {
                builder = builder.header("content-type", content_type);
                Body::from(bytes)
            }
            None => Body::empty(),
        };
        let response = self
            .app
            .clone()
            .oneshot(builder.body(body).expect("request"))
            .await
            .expect("infallible");
        let (parts, body) = response.into_parts();
        let bytes = axum::body::to_bytes(body, usize::MAX).await.expect("body");
        let reply = Reply {
            status: parts.status,
            headers: parts.headers,
            text: String::from_utf8(bytes.to_vec()).expect("UTF-8 body"),
        };
        self.log.lock().expect("log").push(reply.clone());
        reply
    }

    /// Creates a user with `roles` (password [`PASSWORD`]).
    pub async fn user(&self, username: &str, roles: &[&str]) {
        let mut client = self.db.pool.get().await.expect("connection");
        auth::create_user(&mut **client, username, PASSWORD, roles)
            .await
            .expect("create user");
    }

    /// Logs in over HTTP.
    pub async fn login(&self, username: &str) -> Session {
        let reply = self.try_login(username, PASSWORD).await;
        assert_eq!(reply.status, StatusCode::OK, "{}", reply.text);
        let cookie = reply.set_cookies().remove(0);
        let token = cookie
            .strip_prefix("fb_session=")
            .and_then(|rest| rest.split(';').next())
            .expect("session cookie")
            .to_owned();
        let csrf = reply.data()["csrf_token"]
            .as_str()
            .expect("csrf token")
            .to_owned();
        Session { token, csrf }
    }

    pub async fn try_login(&self, username: &str, password: &str) -> Reply {
        self.send(post("/api/v1/login").json(serde_json::json!({
            "username": username,
            "password": password,
        })))
        .await
    }

    /// An API token `key:secret` for `username`.
    pub async fn api_token(&self, username: &str) -> String {
        let client = self.db.pool.get().await.expect("connection");
        let token = auth::create_api_token(&**client, username)
            .await
            .expect("token");
        format!("{}:{}", token.key, token.secret)
    }

    /// Inserts a document of `doctype` as Administrator and returns its name.
    pub async fn admin_insert(&self, admin: &Session, doctype: &str, body: Json) -> String {
        let reply = self
            .send(
                post(&format!("/api/v1/resource/{doctype}"))
                    .session(admin)
                    .json(body),
            )
            .await;
        assert_eq!(reply.status, StatusCode::CREATED, "{}", reply.text);
        reply.data()["name"].as_str().expect("name").to_owned()
    }
}

/// A database created for one test, dropped (with `FORCE`) when the value is dropped.
pub struct TestDb {
    pub pool: Pool,
    admin_url: String,
    name: String,
}

impl TestDb {
    pub async fn migrated() -> Self {
        static COUNTER: AtomicU32 = AtomicU32::new(0);
        let admin_url = admin_url();
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map_or(0, |d| d.subsec_nanos());
        let name = format!(
            "fb_test_http_{:x}_{:x}_{}",
            std::process::id(),
            nanos,
            COUNTER.fetch_add(1, Ordering::Relaxed)
        );
        let admin = connect(&admin_url).await.expect("connect admin");
        admin
            .get()
            .await
            .expect("connection")
            .batch_execute(&format!("CREATE DATABASE {}", quote_ident(&name)))
            .await
            .expect("create test database");
        admin.close();
        let pool = connect(&with_database(&admin_url, &name))
            .await
            .expect("connect to test database");
        {
            let mut client = pool.get().await.expect("connection");
            migrate_framework(&mut client, false)
                .await
                .expect("migrate");
        }
        Self {
            pool,
            admin_url,
            name,
        }
    }

    pub async fn exec(&self, sql: &str) {
        let client = self.pool.get().await.expect("connection");
        client
            .batch_execute(sql)
            .await
            .unwrap_or_else(|e| panic!("{sql}: {e:?}"));
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
                let Ok(admin) = connect(&url).await else {
                    return;
                };
                if let Ok(client) = admin.get().await {
                    let _ = client
                        .batch_execute(&format!(
                            "DROP DATABASE IF EXISTS {} WITH (FORCE)",
                            quote_ident(&name)
                        ))
                        .await;
                }
            });
        })
        .join();
    }
}
