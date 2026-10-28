//! End-to-end tests: the built `forgebase` binary against a real PostgreSQL.
//!
//! Each test that needs a database gets its own throwaway `fb_test_cli_*` database and fails
//! loudly (never skips) when `FORGEBASE_TEST_DATABASE_URL` is not set; see CONTRIBUTING.md.

use std::io::Write;
use std::process::{Command, Output, Stdio};
use std::sync::atomic::{AtomicU32, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

use forgebase_db::{connect, quote_ident};
use serde_json::Value as Json;

const ENV_VAR: &str = "FORGEBASE_TEST_DATABASE_URL";
const DOCTYPES: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/../../examples/doctypes");

fn admin_url() -> String {
    match std::env::var(ENV_VAR) {
        Ok(url) if !url.is_empty() => url,
        _ => panic!(
            "{ENV_VAR} is not set. The forgebase-cli integration tests need a PostgreSQL 16 server; \
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

fn block_on<T>(future: impl std::future::Future<Output = T>) -> T {
    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("tokio runtime")
        .block_on(future)
}

/// A database created for one test, dropped (with `FORCE`) when the value is dropped.
struct TestDb {
    admin_url: String,
    name: String,
    url: String,
}

impl TestDb {
    fn new() -> Self {
        static COUNTER: AtomicU32 = AtomicU32::new(0);
        let admin_url = admin_url();
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map_or(0, |d| d.subsec_nanos());
        let name = format!(
            "fb_test_cli_{:x}_{:x}_{}",
            std::process::id(),
            nanos,
            COUNTER.fetch_add(1, Ordering::Relaxed)
        );
        run_admin(
            &admin_url,
            format!("CREATE DATABASE {}", quote_ident(&name)),
        );
        let url = with_database(&admin_url, &name);
        Self {
            admin_url,
            name,
            url,
        }
    }

    /// Runs the binary with this database configured.
    fn forgebase(&self, args: &[&str]) -> Output {
        self.forgebase_with_stdin(args, None)
    }

    fn forgebase_with_stdin(&self, args: &[&str], stdin: Option<&str>) -> Output {
        let mut command = Command::new(env!("CARGO_BIN_EXE_forgebase"));
        command
            .args(args)
            .env("FORGEBASE_DATABASE_URL", &self.url)
            .stdin(if stdin.is_some() {
                Stdio::piped()
            } else {
                Stdio::null()
            })
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        let mut child = command.spawn().expect("start forgebase");
        if let Some(text) = stdin {
            child
                .stdin
                .take()
                .expect("stdin")
                .write_all(text.as_bytes())
                .expect("write stdin");
        }
        child.wait_with_output().expect("run forgebase")
    }

    /// Runs `sql` in this database.
    fn exec(&self, sql: &str) {
        run_admin(&self.url, sql.to_owned());
    }

    /// The first column of the single result row, as text (`None` for SQL NULL).
    fn scalar(&self, sql: &str) -> Option<String> {
        let url = self.url.clone();
        let sql = sql.to_owned();
        block_on(async move {
            let pool = connect(&url).await.expect("connect");
            let client = pool.get().await.expect("connection");
            client
                .query_one(&sql, &[])
                .await
                .expect("query")
                .get::<_, Option<String>>(0)
        })
    }
}

impl Drop for TestDb {
    fn drop(&mut self) {
        let (url, sql) = (
            self.admin_url.clone(),
            format!(
                "DROP DATABASE IF EXISTS {} WITH (FORCE)",
                quote_ident(&self.name)
            ),
        );
        // A separate thread: Drop may run while a failed test unwinds.
        let _ = std::thread::spawn(move || {
            let _ = std::panic::catch_unwind(|| run_admin(&url, sql));
        })
        .join();
    }
}

fn run_admin(url: &str, sql: String) {
    let url = url.to_owned();
    block_on(async move {
        let pool = connect(&url).await.expect("connect");
        let client = pool.get().await.expect("connection");
        client.batch_execute(&sql).await.expect("execute");
        pool.close();
    });
}

fn stdout(output: &Output) -> String {
    String::from_utf8_lossy(&output.stdout).into_owned()
}

fn stderr(output: &Output) -> String {
    String::from_utf8_lossy(&output.stderr).into_owned()
}

fn code(output: &Output) -> Option<i32> {
    output.status.code()
}

fn json(output: &Output) -> Json {
    serde_json::from_str(&stdout(output)).unwrap_or_else(|e| panic!("{e}: {}", stdout(output)))
}

#[test]
fn version_and_usage_errors() {
    let version = Command::new(env!("CARGO_BIN_EXE_forgebase"))
        .arg("--version")
        .output()
        .expect("run");
    assert_eq!(code(&version), Some(0));
    assert_eq!(
        stdout(&version),
        format!("forgebase {}\n", env!("CARGO_PKG_VERSION"))
    );

    for args in [
        &["migrate"][..],
        &["doc", "get", "customer"],
        &["bogus"],
        &[],
    ] {
        let usage = Command::new(env!("CARGO_BIN_EXE_forgebase"))
            .args(args)
            .env_remove("FORGEBASE_DATABASE_URL")
            .output()
            .expect("run");
        assert_eq!(code(&usage), Some(2), "{args:?}: {}", stderr(&usage));
    }
}

#[test]
fn missing_database_url_is_a_clear_error() {
    let output = Command::new(env!("CARGO_BIN_EXE_forgebase"))
        .args(["migrate", "--doctypes", DOCTYPES])
        .env_remove("FORGEBASE_DATABASE_URL")
        .output()
        .expect("run");
    assert_eq!(code(&output), Some(1));
    assert!(stdout(&output).is_empty());
    let message = stderr(&output);
    assert!(
        message.starts_with("error: FORGEBASE_DATABASE_URL is not set"),
        "{message}"
    );
}

#[test]
fn dry_run_creates_nothing() {
    let db = TestDb::new();
    let output = db.forgebase(&["migrate", "--doctypes", DOCTYPES, "--dry-run"]);
    assert_eq!(code(&output), Some(0), "{}", stderr(&output));
    let text = stdout(&output);
    assert!(text.contains("framework migrations: 1 pending"), "{text}");
    assert!(text.contains("0001 init"), "{text}");
    assert_eq!(db.scalar("SELECT to_regclass('fb_migration')::text"), None);
    assert_eq!(db.scalar("SELECT to_regclass('doc_customer')::text"), None);

    // After the framework migrations, a dry run shows the CREATE TABLE and still changes nothing.
    let output = db.forgebase(&["migrate", "--doctypes", DOCTYPES]);
    assert_eq!(code(&output), Some(0), "{}", stderr(&output));
    db.exec("DROP TABLE doc_customer; DELETE FROM fb_doctype");
    let output = db.forgebase(&["migrate", "--doctypes", DOCTYPES, "--dry-run"]);
    assert_eq!(code(&output), Some(0), "{}", stderr(&output));
    let text = stdout(&output);
    assert!(text.contains("would apply"), "{text}");
    assert!(text.contains("CREATE TABLE \"doc_customer\""), "{text}");
    assert!(text.contains("dry run: nothing was changed"), "{text}");
    assert_eq!(db.scalar("SELECT to_regclass('doc_customer')::text"), None);
}

#[test]
fn migrate_creates_tables_and_a_rerun_changes_nothing() {
    let db = TestDb::new();
    let first = db.forgebase(&["migrate", "--doctypes", DOCTYPES]);
    assert_eq!(code(&first), Some(0), "{}", stderr(&first));
    let text = stdout(&first);
    assert!(text.contains("framework migrations: 1 applied"), "{text}");
    assert!(text.contains("schema sync: applied"), "{text}");
    assert_eq!(
        db.scalar("SELECT to_regclass('doc_customer')::text")
            .as_deref(),
        Some("doc_customer")
    );

    let second = db.forgebase(&["migrate", "--doctypes", DOCTYPES]);
    assert_eq!(code(&second), Some(0), "{}", stderr(&second));
    assert_eq!(
        stdout(&second),
        "framework migrations: up to date\nschema sync: up to date\n"
    );
}

#[test]
fn insert_get_update_delete() {
    let db = TestDb::new();
    assert_eq!(
        code(&db.forgebase(&["migrate", "--doctypes", DOCTYPES])),
        Some(0)
    );

    let inserted = db.forgebase(&[
        "doc",
        "insert",
        "customer",
        "--doctypes",
        DOCTYPES,
        "--actor",
        "alice",
        "--data",
        r#"{"email":"a@example.com","full_name":"Ann","loyalty_points":5,
            "credit_limit":"1234.50","active":true,"joined_on":"2024-03-15",
            "last_seen_at":"2024-03-15T17:20:30+07:00","tier":"gold"}"#,
    ]);
    assert_eq!(code(&inserted), Some(0), "{}", stderr(&inserted));
    let doc = json(&inserted);
    assert_eq!(doc["name"], "a@example.com");
    assert_eq!(doc["owner"], "alice");
    assert_eq!(doc["modified_by"], "alice");
    assert_eq!(doc["version"], 1);
    assert_eq!(doc["loyalty_points"], 5);
    assert_eq!(doc["credit_limit"], "1234.50");
    assert_eq!(doc["active"], true);
    assert_eq!(doc["joined_on"], "2024-03-15");
    assert_eq!(doc["last_seen_at"], "2024-03-15T10:20:30Z");
    assert!(doc["notes"].is_null());
    assert_eq!(doc["id"].as_str().map(str::len), Some(36));
    assert!(doc["created_at"].as_str().is_some_and(|t| t.ends_with('Z')));

    let got = db.forgebase(&[
        "doc",
        "get",
        "customer",
        "a@example.com",
        "--doctypes",
        DOCTYPES,
    ]);
    assert_eq!(code(&got), Some(0), "{}", stderr(&got));
    assert_eq!(json(&got), doc);

    // Update through standard input.
    let updated = db.forgebase_with_stdin(
        &[
            "doc",
            "update",
            "customer",
            "a@example.com",
            "--doctypes",
            DOCTYPES,
            "--actor",
            "bob",
            "--data",
            "-",
        ],
        Some(r#"{"loyalty_points":6,"credit_limit":"0.5","notes":null}"#),
    );
    assert_eq!(code(&updated), Some(0), "{}", stderr(&updated));
    let after = json(&updated);
    assert_eq!(after["version"], 2);
    assert_eq!(after["modified_by"], "bob");
    assert_eq!(after["owner"], "alice");
    assert_eq!(after["loyalty_points"], 6);
    assert_eq!(after["credit_limit"], "0.50");
    assert_eq!(after["created_at"], doc["created_at"]);

    let deleted = db.forgebase(&[
        "doc",
        "delete",
        "customer",
        "a@example.com",
        "--doctypes",
        DOCTYPES,
    ]);
    assert_eq!(code(&deleted), Some(0), "{}", stderr(&deleted));
    assert!(stdout(&deleted).is_empty());

    let gone = db.forgebase(&[
        "doc",
        "get",
        "customer",
        "a@example.com",
        "--doctypes",
        DOCTYPES,
    ]);
    assert_eq!(code(&gone), Some(1));
    assert!(
        stderr(&gone).starts_with("error: customer `a@example.com` not found"),
        "{}",
        stderr(&gone)
    );
}

#[test]
fn insert_reads_data_from_standard_input_with_default_actor() {
    let db = TestDb::new();
    assert_eq!(
        code(&db.forgebase(&["migrate", "--doctypes", DOCTYPES])),
        Some(0)
    );
    let output = db.forgebase_with_stdin(
        &[
            "doc",
            "insert",
            "customer",
            "--doctypes",
            DOCTYPES,
            "--data",
            "-",
        ],
        Some(r#"{"email":"s@example.com","full_name":"Sam"}"#),
    );
    assert_eq!(code(&output), Some(0), "{}", stderr(&output));
    assert_eq!(json(&output)["owner"], "cli");
}

#[test]
fn validation_errors_exit_1_with_one_message_per_line() {
    let db = TestDb::new();
    assert_eq!(
        code(&db.forgebase(&["migrate", "--doctypes", DOCTYPES])),
        Some(0)
    );
    let output = db.forgebase(&[
        "doc",
        "insert",
        "customer",
        "--doctypes",
        DOCTYPES,
        "--data",
        r#"{"email":"v@example.com","bogus":1,"tier":"platinum"}"#,
    ]);
    assert_eq!(code(&output), Some(1));
    assert!(stdout(&output).is_empty());
    let message = stderr(&output);
    assert!(
        message.starts_with("error: validation failed:\n"),
        "{message}"
    );
    assert!(
        message.contains("\n  - field `full_name` is required\n"),
        "{message}"
    );
    assert!(
        message.contains("\n  - unknown field `bogus`\n"),
        "{message}"
    );
    assert!(message.contains("platinum"), "{message}");
    assert_eq!(
        db.scalar("SELECT count(*)::text FROM doc_customer")
            .as_deref(),
        Some("0")
    );
}

#[test]
fn bad_input_and_unique_violations_exit_1() {
    let db = TestDb::new();
    assert_eq!(
        code(&db.forgebase(&["migrate", "--doctypes", DOCTYPES])),
        Some(0)
    );
    let insert = |data: &str| {
        db.forgebase(&[
            "doc",
            "insert",
            "customer",
            "--doctypes",
            DOCTYPES,
            "--data",
            data,
        ])
    };

    for (data, expected) in [
        ("{", "invalid JSON"),
        ("[1]", "must be a JSON object"),
        (r#"{"credit_limit":12.5}"#, "must be a JSON string"),
        (r#"{"loyalty_points":1.5}"#, "field `loyalty_points`"),
    ] {
        let output = insert(data);
        assert_eq!(code(&output), Some(1), "{data}");
        assert!(
            stderr(&output).starts_with("error: "),
            "{data}: {}",
            stderr(&output)
        );
        assert!(
            stderr(&output).contains(expected),
            "{data}: {}",
            stderr(&output)
        );
    }

    let ok = r#"{"email":"u@example.com","full_name":"U"}"#;
    assert_eq!(code(&insert(ok)), Some(0));
    let duplicate = insert(ok);
    assert_eq!(code(&duplicate), Some(1));
    assert!(
        stderr(&duplicate).contains("unique"),
        "{}",
        stderr(&duplicate)
    );

    let unknown = db.forgebase(&["doc", "get", "nope", "x", "--doctypes", DOCTYPES]);
    assert_eq!(code(&unknown), Some(1));
    assert!(
        stderr(&unknown).contains("unknown DocType `nope`"),
        "{}",
        stderr(&unknown)
    );

    let nothing = db.forgebase(&[
        "doc",
        "update",
        "customer",
        "u@example.com",
        "--doctypes",
        DOCTYPES,
        "--data",
        "{}",
    ]);
    assert_eq!(code(&nothing), Some(1));
    assert!(
        stderr(&nothing).contains("at least one"),
        "{}",
        stderr(&nothing)
    );

    let rename = db.forgebase(&[
        "doc",
        "update",
        "customer",
        "u@example.com",
        "--doctypes",
        DOCTYPES,
        "--data",
        r#"{"email":"new@example.com"}"#,
    ]);
    assert_eq!(code(&rename), Some(1));
    assert!(
        stderr(&rename).contains("cannot be changed"),
        "{}",
        stderr(&rename)
    );
}

#[test]
fn schema_conflicts_exit_1_and_change_nothing() {
    let db = TestDb::new();
    assert_eq!(
        code(&db.forgebase(&["migrate", "--doctypes", DOCTYPES])),
        Some(0)
    );
    db.exec(
        "DROP TABLE doc_customer; DELETE FROM fb_doctype; CREATE TABLE doc_customer (x integer)",
    );
    let output = db.forgebase(&["migrate", "--doctypes", DOCTYPES]);
    assert_eq!(code(&output), Some(1));
    assert!(stdout(&output).is_empty());
    let message = stderr(&output);
    assert!(
        message.starts_with("error: schema sync refused, 1 conflict(s)"),
        "{message}"
    );
    assert!(
        message.contains("\n  - table doc_customer already exists"),
        "{message}"
    );
    assert_eq!(
        db.scalar("SELECT count(*)::text FROM information_schema.columns WHERE table_name = 'doc_customer'")
            .as_deref(),
        Some("1")
    );
}

#[test]
fn invalid_doctype_files_exit_1_naming_the_file() {
    let dir = std::env::temp_dir().join(format!("forgebase-cli-bad-{}", std::process::id()));
    std::fs::create_dir_all(&dir).expect("dir");
    std::fs::write(dir.join("broken.toml"), "name = ").expect("write");
    // DocType loading fails before any connection is made, so the URL is never used.
    let output = Command::new(env!("CARGO_BIN_EXE_forgebase"))
        .args(["migrate", "--doctypes", dir.to_str().expect("utf-8 path")])
        .env(
            "FORGEBASE_DATABASE_URL",
            "postgres://unused:unused@127.0.0.1:1/unused",
        )
        .stdin(Stdio::null())
        .output()
        .expect("run forgebase");
    std::fs::remove_dir_all(&dir).expect("cleanup");
    assert_eq!(code(&output), Some(1));
    let message = stderr(&output);
    assert!(
        message.starts_with("error: cannot load DocType files:\n"),
        "{message}"
    );
    assert!(message.contains("broken.toml"), "{message}");
}
