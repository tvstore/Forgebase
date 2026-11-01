//! End-to-end tests: the built `forgebase` binary against a real PostgreSQL.
//!
//! Each test that needs a database gets its own throwaway `fb_test_cli_*` database and fails
//! loudly (never skips) when `FORGEBASE_TEST_DATABASE_URL` is not set; see CONTRIBUTING.md.

mod common;

use std::io::Write;
use std::process::{Command, Output, Stdio};
use std::sync::atomic::{AtomicU32, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

use forgebase_db::{connect, quote_ident};
use serde_json::Value as Json;

const ENV_VAR: &str = "FORGEBASE_TEST_DATABASE_URL";
const DOCTYPES: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/tests/fixtures/quickstart");
/// A submittable DocType with a workflow (M8.1).
const WORKFLOW_DOCTYPES: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/tests/fixtures/workflow");

/// Removes `customer` and the example DocTypes that depend on it, as if they had never been synced.
const DROP_CUSTOMER: &str = "DROP TABLE doc_sales_order_item, doc_sales_order, doc_customer; \
     DELETE FROM fb_doctype WHERE name IN ('customer', 'sales_order', 'sales_order_item')";

fn admin_url() -> String {
    match std::env::var(ENV_VAR) {
        Ok(url) if !url.is_empty() => url,
        _ => panic!(
            "{ENV_VAR} is not set. The forgebase integration tests need a PostgreSQL 16 server; \
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
        // `update` and `delete` need --version.
        &[
            "doc",
            "update",
            "customer",
            "x",
            "--doctypes",
            DOCTYPES,
            "--data",
            "{}",
        ],
        &["doc", "delete", "customer", "x", "--doctypes", DOCTYPES],
        &[
            "doc",
            "delete",
            "customer",
            "x",
            "--doctypes",
            DOCTYPES,
            "--version",
            "abc",
        ],
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
fn the_stock_binary_without_doctypes_is_a_usage_error_that_names_the_subcommand() {
    // The stock binary has no apps, so a command that needs DocTypes needs `--doctypes`.
    for (args, usage) in [
        (&["migrate"][..], "Usage: forgebase migrate "),
        (&["serve"], "Usage: forgebase serve "),
        (
            &["doc", "get", "customer", "x"],
            "Usage: forgebase doc get ",
        ),
    ] {
        let output = Command::new(env!("CARGO_BIN_EXE_forgebase"))
            .args(args)
            .env_remove("FORGEBASE_DATABASE_URL")
            .output()
            .expect("run");
        assert_eq!(code(&output), Some(2), "{args:?}");
        assert!(stdout(&output).is_empty());
        let message = stderr(&output);
        assert!(
            message.contains("--doctypes <DIR>") && message.contains(usage),
            "{args:?}: {message}"
        );
    }
    // The help says when the option is required.
    let help = Command::new(env!("CARGO_BIN_EXE_forgebase"))
        .args(["migrate", "--help"])
        .output()
        .expect("run");
    assert_eq!(code(&help), Some(0));
    assert!(
        stdout(&help).contains("Required when the binary has no apps"),
        "{}",
        stdout(&help)
    );
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
    assert!(text.contains("framework migrations: 12 pending"), "{text}");
    assert!(
        text.contains("0001 init")
            && text.contains("0002 series")
            && text.contains("0003 auth")
            && text.contains("0004 apps")
            && text.contains("0005 jobs")
            && text.contains("0006 schedules")
            && text.contains("0007 finished_jobs_index")
            && text.contains("0008 workflow_log")
            && text.contains("0009 saved_views")
            && text.contains("0010 webhooks"),
        "{text}"
    );
    assert_eq!(db.scalar("SELECT to_regclass('fb_migration')::text"), None);
    assert_eq!(db.scalar("SELECT to_regclass('doc_customer')::text"), None);

    // After the framework migrations, a dry run shows the CREATE TABLE and still changes nothing.
    let output = db.forgebase(&["migrate", "--doctypes", DOCTYPES]);
    assert_eq!(code(&output), Some(0), "{}", stderr(&output));
    db.exec(DROP_CUSTOMER);
    let output = db.forgebase(&["migrate", "--doctypes", DOCTYPES, "--dry-run"]);
    assert_eq!(code(&output), Some(0), "{}", stderr(&output));
    let text = stdout(&output);
    assert!(text.contains("would apply"), "{text}");
    assert!(text.contains("CREATE TABLE \"doc_customer\""), "{text}");
    assert!(
        text.contains(
            "ALTER TABLE \"doc_customer\" ADD FOREIGN KEY (\"customer_group\") REFERENCES \"doc_customer_group\" (\"id\") ON DELETE RESTRICT;"
        ),
        "{text}"
    );
    assert!(
        text.contains("CREATE INDEX ON \"doc_customer\" (\"customer_group\");"),
        "{text}"
    );
    assert!(text.contains("dry run: nothing was changed"), "{text}");
    assert_eq!(db.scalar("SELECT to_regclass('doc_customer')::text"), None);
}

#[test]
fn migrate_creates_tables_and_a_rerun_changes_nothing() {
    let db = TestDb::new();
    let first = db.forgebase(&["migrate", "--doctypes", DOCTYPES]);
    assert_eq!(code(&first), Some(0), "{}", stderr(&first));
    let text = stdout(&first);
    assert!(text.contains("framework migrations: 12 applied"), "{text}");
    assert!(text.contains("schema sync: applied"), "{text}");
    assert_eq!(
        db.scalar("SELECT to_regclass('doc_customer')::text")
            .as_deref(),
        Some("doc_customer")
    );
    assert_eq!(
        db.scalar("SELECT count(*)::text FROM pg_constraint WHERE contype = 'f' AND conrelid::regclass::text LIKE 'doc\\_%'")
            .as_deref(),
        // customer -> customer_group, sales_order -> customer, sales_order_item -> sales_order
        Some("3")
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
            "--version",
            "1",
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

    // An empty actor is refused for delete too, and nothing is deleted.
    let refused = db.forgebase(&[
        "doc",
        "delete",
        "customer",
        "a@example.com",
        "--version",
        "2",
        "--doctypes",
        DOCTYPES,
        "--actor",
        "",
    ]);
    assert_eq!(code(&refused), Some(1), "{}", stderr(&refused));

    let deleted = db.forgebase(&[
        "doc",
        "delete",
        "customer",
        "a@example.com",
        "--version",
        "2",
        "--doctypes",
        DOCTYPES,
        "--actor",
        "dee",
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
        "--version",
        "1",
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
        "--version",
        "1",
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
    db.exec(&format!(
        "{DROP_CUSTOMER}; CREATE TABLE doc_customer (x integer)"
    ));
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
    let dir = std::env::temp_dir().join(format!("forgebase-test-bad-{}", std::process::id()));
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

#[test]
fn links_and_versions_end_to_end() {
    let db = TestDb::new();
    assert_eq!(
        code(&db.forgebase(&["migrate", "--doctypes", DOCTYPES])),
        Some(0)
    );
    let run = |args: &[&str]| {
        let mut full = args.to_vec();
        full.extend_from_slice(&["--doctypes", DOCTYPES]);
        db.forgebase(&full)
    };

    // A link to a group that does not exist is a validation error naming the field.
    let customer = r#"{"email":"l@example.com","full_name":"L","customer_group":"retail"}"#;
    let missing = run(&["doc", "insert", "customer", "--data", customer]);
    assert_eq!(code(&missing), Some(1));
    assert!(
        stderr(&missing).contains("field `customer_group`: customer_group `retail` does not exist"),
        "{}",
        stderr(&missing)
    );

    let group = run(&[
        "doc",
        "insert",
        "customer_group",
        "--data",
        r#"{"title":"retail"}"#,
    ]);
    assert_eq!(code(&group), Some(0), "{}", stderr(&group));
    let inserted = run(&["doc", "insert", "customer", "--data", customer]);
    assert_eq!(code(&inserted), Some(0), "{}", stderr(&inserted));
    // A link is the target's name in the output.
    assert_eq!(json(&inserted)["customer_group"], "retail");

    // A linked group cannot be deleted; the message names the linking DocType.
    let blocked = run(&[
        "doc",
        "delete",
        "customer_group",
        "retail",
        "--version",
        "1",
    ]);
    assert_eq!(code(&blocked), Some(1));
    assert_eq!(
        stderr(&blocked),
        "error: customer_group `retail` cannot be deleted: it is linked from customer\n"
    );

    // A stale version is refused and reports the current one.
    let updated = run(&[
        "doc",
        "update",
        "customer",
        "l@example.com",
        "--version",
        "1",
        "--data",
        r#"{"customer_group":null}"#,
    ]);
    assert_eq!(code(&updated), Some(0), "{}", stderr(&updated));
    assert!(json(&updated)["customer_group"].is_null());
    let stale = run(&[
        "doc",
        "update",
        "customer",
        "l@example.com",
        "--version",
        "1",
        "--data",
        r#"{"loyalty_points":1}"#,
    ]);
    assert_eq!(code(&stale), Some(1));
    assert!(
        stderr(&stale).contains("expected version 1, current version 2"),
        "{}",
        stderr(&stale)
    );
    let stale_delete = run(&[
        "doc",
        "delete",
        "customer",
        "l@example.com",
        "--version",
        "1",
    ]);
    assert_eq!(code(&stale_delete), Some(1));
    assert!(
        stderr(&stale_delete).contains("current version 2"),
        "{}",
        stderr(&stale_delete)
    );
    assert_eq!(
        db.scalar("SELECT count(*)::text FROM doc_customer")
            .as_deref(),
        Some("1")
    );

    // Now nothing links to the group any more.
    let deleted = run(&[
        "doc",
        "delete",
        "customer_group",
        "retail",
        "--version",
        "1",
    ]);
    assert_eq!(code(&deleted), Some(0), "{}", stderr(&deleted));
}

#[test]
fn unknown_link_targets_are_reported_with_the_file() {
    let dir = std::env::temp_dir().join(format!("forgebase-test-links-{}", std::process::id()));
    std::fs::create_dir_all(&dir).expect("dir");
    std::fs::write(
        dir.join("order.toml"),
        "name = \"sales_order\"\nlabel = \"O\"\n[[fields]]\nname = \"customer\"\ntype = \"link\"\ntarget = \"customer\"\n",
    )
    .expect("write");
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
    assert!(message.contains("order.toml"), "{message}");
    assert!(
        message.contains("field `customer` links to unknown doctype `customer`"),
        "{message}"
    );
}

#[test]
fn child_tables_end_to_end() {
    let db = TestDb::new();
    let migrate = db.forgebase(&["migrate", "--doctypes", DOCTYPES]);
    assert_eq!(code(&migrate), Some(0), "{}", stderr(&migrate));
    assert!(
        stdout(&migrate).contains("CREATE TABLE \"doc_sales_order_item\""),
        "{}",
        stdout(&migrate)
    );
    let run = |args: &[&str]| {
        let mut full = args.to_vec();
        full.extend_from_slice(&["--doctypes", DOCTYPES]);
        db.forgebase(&full)
    };
    let customer = run(&[
        "doc",
        "insert",
        "customer",
        "--data",
        r#"{"email":"c@example.com","full_name":"C"}"#,
    ]);
    assert_eq!(code(&customer), Some(0), "{}", stderr(&customer));

    // Insert with rows: the output has the rows in order, with their fields.
    let data = r#"{"customer":"c@example.com","order_date":"2026-10-28","items":[
        {"description":"Widget","qty":2,"rate":"9.50"},
        {"description":"Gadget","qty":1}]}"#;
    let inserted = run(&["doc", "insert", "sales_order", "--data", data]);
    assert_eq!(code(&inserted), Some(0), "{}", stderr(&inserted));
    let order = json(&inserted);
    let name = order["name"].as_str().expect("name").to_owned();
    assert_eq!(order["version"], 1);
    assert_eq!(order["customer"], "c@example.com");
    assert!(order.get("items").is_some_and(Json::is_array));
    assert_eq!(
        order["items"],
        serde_json::json!([
            {"description": "Widget", "qty": 2, "rate": "9.50"},
            {"description": "Gadget", "qty": 1, "rate": null},
        ])
    );
    let fetched = json(&run(&["doc", "get", "sales_order", &name]));
    assert_eq!(fetched, order);
    assert_eq!(
        db.scalar("SELECT count(*)::text FROM doc_sales_order_item")
            .as_deref(),
        Some("2")
    );

    // An update that leaves `items` out keeps the rows; one that has it replaces them all.
    let kept = run(&[
        "doc",
        "update",
        "sales_order",
        &name,
        "--version",
        "1",
        "--data",
        r#"{"order_date":"2026-10-29"}"#,
    ]);
    assert_eq!(code(&kept), Some(0), "{}", stderr(&kept));
    assert_eq!(json(&kept)["items"], order["items"]);
    let replaced = run(&[
        "doc",
        "update",
        "sales_order",
        &name,
        "--version",
        "2",
        "--data",
        r#"{"items":[{"description":"Only","qty":5}]}"#,
    ]);
    assert_eq!(code(&replaced), Some(0), "{}", stderr(&replaced));
    let replaced = json(&replaced);
    assert_eq!(replaced["version"], 3);
    assert_eq!(
        replaced["items"],
        serde_json::json!([{"description": "Only", "qty": 5, "rate": null}])
    );

    // The table is required: no rows is refused, and nothing changed.
    let empty = run(&[
        "doc",
        "update",
        "sales_order",
        &name,
        "--version",
        "3",
        "--data",
        r#"{"items":[]}"#,
    ]);
    assert_eq!(code(&empty), Some(1));
    assert_eq!(
        stderr(&empty),
        "error: validation failed:\n  - field `items` is required\n"
    );

    // Row problems are listed with the row position, one per line; nothing is written.
    let bad = run(&[
        "doc",
        "insert",
        "sales_order",
        "--data",
        r#"{"customer":"c@example.com","items":[{"description":"ok","qty":1},{"qty":1},{"description":"x","qty":2,"nope":1}]}"#,
    ]);
    assert_eq!(code(&bad), Some(1));
    assert_eq!(
        stderr(&bad),
        "error: validation failed:\n  - field `items` row 2: field `description` is required\n  - field `items` row 3: unknown field `nope`\n"
    );
    assert_eq!(
        db.scalar("SELECT count(*)::text FROM doc_sales_order")
            .as_deref(),
        Some("1")
    );

    // Wrong JSON shapes fail before the database is used.
    for (data, expected) in [
        (
            r#"{"items":"x"}"#,
            "error: field `items`: expected an array of rows, found a string\n",
        ),
        (
            r#"{"items":[1]}"#,
            "error: field `items`: row 1: expected an object, found a number\n",
        ),
        (
            r#"{"items":[{"qty":"two"}]}"#,
            "error: field `items`: row 1: field `qty`: expected an integer, found a string\n",
        ),
    ] {
        let output = run(&["doc", "insert", "sales_order", "--data", data]);
        assert_eq!(code(&output), Some(1), "{data}");
        assert_eq!(stderr(&output), expected, "{data}");
    }

    // A child DocType has no commands of its own.
    let child = run(&[
        "doc",
        "insert",
        "sales_order_item",
        "--data",
        r#"{"description":"x","qty":1}"#,
    ]);
    assert_eq!(code(&child), Some(1));
    assert!(
        stderr(&child).starts_with("error: DocType `sales_order_item` is a child DocType"),
        "{}",
        stderr(&child)
    );

    // Deleting the order deletes its rows.
    let deleted = run(&["doc", "delete", "sales_order", &name, "--version", "3"]);
    assert_eq!(code(&deleted), Some(0), "{}", stderr(&deleted));
    assert_eq!(
        db.scalar("SELECT count(*)::text FROM doc_sales_order_item")
            .as_deref(),
        Some("0")
    );
}

#[test]
fn submit_cancel_and_amend_end_to_end() {
    let db = TestDb::new();
    let migrate = db.forgebase(&["migrate", "--doctypes", DOCTYPES]);
    assert_eq!(code(&migrate), Some(0), "{}", stderr(&migrate));
    let run = |args: &[&str]| {
        let mut full = args.to_vec();
        full.extend_from_slice(&["--doctypes", DOCTYPES]);
        db.forgebase(&full)
    };
    let year = db
        .scalar("SELECT to_char(now() AT TIME ZONE 'UTC', 'YYYY')")
        .expect("year");
    let customer = run(&[
        "doc",
        "insert",
        "customer",
        "--data",
        r#"{"email":"c@example.com","full_name":"C"}"#,
    ]);
    assert_eq!(code(&customer), Some(0), "{}", stderr(&customer));
    // A customer has no docstatus, so its output has no such keys.
    assert!(json(&customer).get("docstatus").is_none());

    // The series names the order, and it starts as a draft.
    let data = r#"{"customer":"c@example.com","items":[{"description":"Widget","qty":2}]}"#;
    let inserted = run(&["doc", "insert", "sales_order", "--data", data]);
    assert_eq!(code(&inserted), Some(0), "{}", stderr(&inserted));
    let order = json(&inserted);
    let name = format!("SO-{year}-00001");
    assert_eq!(order["name"], name.as_str());
    assert_eq!(
        (&order["docstatus"], &order["amended_from"]),
        (&Json::from(0), &Json::Null)
    );

    // docstatus and amended_from cannot be set through the data.
    let forced = run(&[
        "doc",
        "update",
        "sales_order",
        &name,
        "--version",
        "1",
        "--data",
        r#"{"docstatus":1}"#,
    ]);
    assert_eq!(code(&forced), Some(1));
    assert!(
        stderr(&forced).contains("unknown field `docstatus`"),
        "{}",
        stderr(&forced)
    );

    // Submit: needs --version; the output shows docstatus 1 and the next version.
    let stale = run(&["doc", "submit", "sales_order", &name, "--version", "9"]);
    assert_eq!(code(&stale), Some(1));
    assert!(
        stderr(&stale).contains("current version 1"),
        "{}",
        stderr(&stale)
    );
    let submitted = run(&[
        "doc",
        "submit",
        "sales_order",
        &name,
        "--version",
        "1",
        "--actor",
        "bob",
    ]);
    assert_eq!(code(&submitted), Some(0), "{}", stderr(&submitted));
    let submitted = json(&submitted);
    assert_eq!(
        (&submitted["docstatus"], &submitted["version"]),
        (&Json::from(1), &Json::from(2))
    );
    assert_eq!(submitted["modified_by"], "bob");
    // Submitting again, deleting and changing a locked field are refused ...
    let again = run(&["doc", "submit", "sales_order", &name, "--version", "2"]);
    assert_eq!(code(&again), Some(1));
    assert!(
        stderr(&again).contains("it is submitted"),
        "{}",
        stderr(&again)
    );
    let delete = run(&["doc", "delete", "sales_order", &name, "--version", "2"]);
    assert_eq!(code(&delete), Some(1));
    assert!(
        stderr(&delete).contains("cannot delete"),
        "{}",
        stderr(&delete)
    );
    let locked = run(&[
        "doc",
        "update",
        "sales_order",
        &name,
        "--version",
        "2",
        "--data",
        r#"{"order_date":"2026-10-30"}"#,
    ]);
    assert_eq!(code(&locked), Some(1));
    assert!(
        stderr(&locked).contains("`order_date` cannot be changed"),
        "{}",
        stderr(&locked)
    );
    // ... while the allow_on_submit field can change.
    let remark = run(&[
        "doc",
        "update",
        "sales_order",
        &name,
        "--version",
        "2",
        "--data",
        r#"{"remarks":"rush"}"#,
    ]);
    assert_eq!(code(&remark), Some(0), "{}", stderr(&remark));
    assert_eq!(json(&remark)["docstatus"], 1);

    // Amend needs a cancelled document; cancel, then amend.
    let early = run(&["doc", "amend", "sales_order", &name, "--version", "3"]);
    assert_eq!(code(&early), Some(1));
    assert!(
        stderr(&early).contains("it is submitted"),
        "{}",
        stderr(&early)
    );
    let cancelled = run(&["doc", "cancel", "sales_order", &name, "--version", "3"]);
    assert_eq!(code(&cancelled), Some(0), "{}", stderr(&cancelled));
    let cancelled = json(&cancelled);
    assert_eq!(
        (&cancelled["docstatus"], &cancelled["version"]),
        (&Json::from(2), &Json::from(4))
    );
    let amended = run(&["doc", "amend", "sales_order", &name, "--version", "4"]);
    assert_eq!(code(&amended), Some(0), "{}", stderr(&amended));
    let amended = json(&amended);
    assert_eq!(amended["name"], format!("{name}-1").as_str());
    assert_eq!(amended["amended_from"], name.as_str());
    assert_eq!(
        (&amended["docstatus"], &amended["version"]),
        (&Json::from(0), &Json::from(1))
    );
    assert_eq!(amended["remarks"], "rush");
    assert_eq!(amended["items"], cancelled["items"]);
    // The cancelled document can be amended once.
    let twice = run(&["doc", "amend", "sales_order", &name, "--version", "4"]);
    assert_eq!(code(&twice), Some(1));
    assert!(
        stderr(&twice).contains("already amended"),
        "{}",
        stderr(&twice)
    );

    // A cancelled document and a draft can be deleted.
    let gone = run(&["doc", "delete", "sales_order", &name, "--version", "4"]);
    assert_eq!(code(&gone), Some(0), "{}", stderr(&gone));
    let draft = format!("{name}-1");
    let gone = run(&["doc", "delete", "sales_order", &draft, "--version", "1"]);
    assert_eq!(code(&gone), Some(0), "{}", stderr(&gone));

    // A DocType that is not submittable says so; amend takes no data.
    let plain = run(&[
        "doc",
        "submit",
        "customer",
        "c@example.com",
        "--version",
        "1",
    ]);
    assert_eq!(code(&plain), Some(1));
    assert!(
        stderr(&plain).contains("not submittable"),
        "{}",
        stderr(&plain)
    );
    let data = run(&[
        "doc",
        "amend",
        "sales_order",
        &name,
        "--version",
        "1",
        "--data",
        "{}",
    ]);
    assert_eq!(code(&data), Some(2), "a usage error: {}", stderr(&data));
    let counter = db.scalar("SELECT current::text FROM fb_series");
    assert_eq!(counter.as_deref(), Some("1"));
}

#[test]
fn list_filters_sorts_and_pages_as_json() {
    let db = migrated_db();
    for (email, points, tier) in [
        ("a@x.y", 30, "gold"),
        ("b@x.y", 10, "silver"),
        ("c@x.y", 20, "gold"),
    ] {
        let data = format!(
            r#"{{"email":"{email}","full_name":"N","loyalty_points":{points},"tier":"{tier}"}}"#
        );
        let out = db.forgebase(&[
            "doc",
            "insert",
            "customer",
            "--doctypes",
            DOCTYPES,
            "--data",
            &data,
        ]);
        assert_eq!(code(&out), Some(0), "{}", stderr(&out));
    }
    let list = |extra: &[&str]| {
        let mut args = vec!["doc", "list", "customer", "--doctypes", DOCTYPES];
        args.extend_from_slice(extra);
        db.forgebase(&args)
    };
    let names = |output: &Output| -> Vec<String> {
        assert_eq!(code(output), Some(0), "{}", stderr(output));
        json(output)
            .as_array()
            .expect("an array")
            .iter()
            .map(|d| d["name"].as_str().expect("name").to_owned())
            .collect()
    };
    // Default: everything (three is below the page size), ordered by name.
    assert_eq!(names(&list(&[])), ["a@x.y", "b@x.y", "c@x.y"]);
    // A document in the list has the same shape as `doc get` (standard columns and values).
    let first = json(&list(&["--limit", "1"]));
    assert_eq!(first[0]["owner"], "cli");
    assert_eq!(first[0]["loyalty_points"], 30);
    assert_eq!(first[0]["version"], 1);
    // Sorting, `:desc`, several keys.
    assert_eq!(
        names(&list(&["--sort", "loyalty_points:desc"])),
        ["a@x.y", "c@x.y", "b@x.y"]
    );
    assert_eq!(
        names(&list(&["--sort", "tier:desc", "--sort", "loyalty_points"])),
        ["b@x.y", "c@x.y", "a@x.y"]
    );
    // Filters, all operators.
    let filter = |text: &str| names(&list(&["--filter", text]));
    assert_eq!(
        filter(
            r#"[{"field":"tier","op":"=","value":"gold"},{"field":"loyalty_points","op":">","value":20}]"#
        ),
        ["a@x.y"]
    );
    assert_eq!(
        filter(r#"[{"field":"tier","op":"!=","value":"gold"}]"#),
        ["b@x.y"]
    );
    assert_eq!(
        filter(r#"[{"field":"loyalty_points","op":"in","value":[10,30]}]"#),
        ["a@x.y", "b@x.y"]
    );
    assert_eq!(
        filter(r#"[{"field":"loyalty_points","op":"<=","value":10}]"#),
        ["b@x.y"]
    );
    assert_eq!(
        filter(r#"[{"field":"name","op":"like","value":"%@x.y"}]"#).len(),
        3
    );
    assert_eq!(filter(r#"[{"field":"notes","op":"is null"}]"#).len(), 3);
    assert!(filter(r#"[{"field":"notes","op":"is not null"}]"#).is_empty());
    assert!(filter(r#"[{"field":"customer_group","op":"=","value":"nope"}]"#).is_empty());
    assert_eq!(filter("[]").len(), 3);
    // Paging.
    assert_eq!(names(&list(&["--limit", "2"])), ["a@x.y", "b@x.y"]);
    assert_eq!(names(&list(&["--limit", "2", "--offset", "2"])), ["c@x.y"]);
    assert!(names(&list(&["--offset", "3"])).is_empty());
    // Every kind of mistake is an exit-1 error that names the problem.
    for (args, needle) in [
        (&["--limit", "501"][..], "maximum"),
        (&["--limit", "0"], "at least 1"),
        (&["--sort", "nope"], "unknown field"),
        (&["--sort", "name:sideways"], "FIELD:desc"),
        (&["--filter", "not json"], "invalid JSON"),
        (&["--filter", "{}"], "JSON array"),
        (
            &["--filter", r#"[{"field":"nope","op":"=","value":1}]"#],
            "unknown field",
        ),
        (
            &["--filter", r#"[{"field":"tier","op":"~","value":1}]"#],
            "unknown operator",
        ),
        (
            &["--filter", r#"[{"field":"tier","op":"=","value":1}]"#],
            "tier",
        ),
        (
            &["--filter", r#"[{"field":"tier","op":"="}]"#],
            "needs a `value`",
        ),
        (
            &["--filter", r#"[{"field":"tier","op":"is null","value":1}]"#],
            "takes no `value`",
        ),
        (
            &[
                "--filter",
                r#"[{"field":"loyalty_points","op":"like","value":"1%"}]"#,
            ],
            "text fields only",
        ),
        (
            &[
                "--filter",
                r#"[{"field":"tier","op":"=","value":"gold","extra":1}]"#,
            ],
            "unknown key",
        ),
    ] {
        let out = list(args);
        assert_eq!(code(&out), Some(1), "{args:?}: {}", stdout(&out));
        assert!(stderr(&out).contains(needle), "{args:?}: {}", stderr(&out));
    }
    // An unknown DocType is reported before the database is touched.
    let out = db.forgebase(&["doc", "list", "nope", "--doctypes", DOCTYPES]);
    assert_eq!(code(&out), Some(1));
    assert!(stderr(&out).contains("unknown DocType"), "{}", stderr(&out));
}

const SECRET_PASSWORD: &str = "s3cret pass phrase";

fn migrated_db() -> TestDb {
    let db = TestDb::new();
    let out = db.forgebase(&["migrate", "--doctypes", DOCTYPES]);
    assert_eq!(code(&out), Some(0), "{}", stderr(&out));
    db
}

#[test]
fn user_commands_manage_users_and_roles() {
    let db = migrated_db();

    // The password comes from the first line of standard input only.
    let out = db.forgebase_with_stdin(
        &[
            "user", "create", "alice", "--role", "Sales", "--role", "Admin",
        ],
        Some(&format!("{SECRET_PASSWORD}\nsecond line is ignored\n")),
    );
    assert_eq!(code(&out), Some(0), "{}", stderr(&out));
    assert_eq!(stdout(&out), "created user alice\n");
    assert!(!stdout(&out).contains(SECRET_PASSWORD) && !stderr(&out).contains(SECRET_PASSWORD));
    assert_eq!(
        db.scalar("SELECT string_agg(role, ',' ORDER BY role) FROM fb_user_role"),
        Some("Admin,Sales".to_owned())
    );
    let hash = db.scalar("SELECT password_hash FROM fb_user").unwrap();
    assert!(hash.starts_with("$argon2id$"), "{hash}");
    assert!(!hash.contains(SECRET_PASSWORD));

    // A duplicate, a bad password and a missing password are errors that never show the password.
    let out = db.forgebase_with_stdin(&["user", "create", "alice"], Some("another password\n"));
    assert_eq!(code(&out), Some(1));
    assert!(stderr(&out).contains("already exists"), "{}", stderr(&out));
    let out = db.forgebase_with_stdin(&["user", "create", "bob"], Some("short\n"));
    assert_eq!(code(&out), Some(1));
    assert!(
        stderr(&out).contains("invalid password"),
        "{}",
        stderr(&out)
    );
    assert!(!stderr(&out).contains("short"));
    let out = db.forgebase_with_stdin(&["user", "create", "bob"], Some(""));
    assert_eq!(code(&out), Some(1));
    assert!(
        stderr(&out).contains("invalid password"),
        "{}",
        stderr(&out)
    );
    // The password is never an argument: a second positional argument is a usage error.
    let out = db.forgebase(&["user", "create", "bob", "hunter2hunter2"]);
    assert_eq!(code(&out), Some(2));

    let out = db.forgebase(&["user", "add-role", "alice", "Accounts"]);
    assert_eq!(stdout(&out), "added role Accounts to user alice\n");
    let out = db.forgebase(&["user", "add-role", "alice", "Accounts"]);
    assert_eq!(stdout(&out), "user alice has role Accounts already\n");
    let out = db.forgebase(&["user", "remove-role", "alice", "Admin"]);
    assert_eq!(stdout(&out), "removed role Admin from user alice\n");
    let out = db.forgebase(&["user", "remove-role", "alice", "Admin"]);
    assert_eq!(stdout(&out), "user alice does not have role Admin\n");
    assert_eq!(
        db.scalar("SELECT string_agg(role, ',' ORDER BY role) FROM fb_user_role"),
        Some("Accounts,Sales".to_owned())
    );
    let out = db.forgebase(&["user", "add-role", "nobody", "Admin"]);
    assert_eq!(code(&out), Some(1));
    assert!(
        stderr(&out).contains("user `nobody` not found"),
        "{}",
        stderr(&out)
    );

    let out = db.forgebase(&["user", "disable", "alice"]);
    assert_eq!(code(&out), Some(0), "{}", stderr(&out));
    assert_eq!(
        db.scalar("SELECT enabled::text FROM fb_user"),
        Some("false".into())
    );
    let out = db.forgebase(&["user", "enable", "alice"]);
    assert_eq!(code(&out), Some(0), "{}", stderr(&out));
    assert_eq!(
        db.scalar("SELECT enabled::text FROM fb_user"),
        Some("true".into())
    );

    // Set the password (CRLF line ending stripped) and end the sessions.
    db.exec(
        "INSERT INTO fb_session (token_hash, csrf_hash, user_id, expires_at) \
         SELECT '\\x00', '\\x01', id, now() + interval '1 day' FROM fb_user",
    );
    let before = db.scalar("SELECT password_hash FROM fb_user").unwrap();
    let out = db.forgebase_with_stdin(
        &["user", "set-password", "alice"],
        Some("a new password phrase\r\n"),
    );
    assert_eq!(code(&out), Some(0), "{}", stderr(&out));
    assert!(!stdout(&out).contains("a new password phrase"));
    assert_ne!(
        db.scalar("SELECT password_hash FROM fb_user").unwrap(),
        before
    );
    assert_eq!(
        db.scalar("SELECT count(*)::text FROM fb_session"),
        Some("0".into())
    );
}

#[test]
fn token_create_prints_the_secret_once_and_revoke_works() {
    let db = migrated_db();
    let out = db.forgebase_with_stdin(&["user", "create", "alice"], Some("an ordinary password\n"));
    assert_eq!(code(&out), Some(0), "{}", stderr(&out));

    let out = db.forgebase(&["token", "create", "alice"]);
    assert_eq!(code(&out), Some(0), "{}", stderr(&out));
    assert_eq!(stderr(&out), "");
    let printed = stdout(&out);
    let (key, secret) = printed.trim_end().split_once(':').expect("key:secret");
    assert_eq!((key.len(), secret.len()), (32, 64), "{printed}");
    assert!(printed.ends_with('\n') && printed.lines().count() == 1);
    // The secret is not stored; the digest is.
    assert_eq!(
        db.scalar("SELECT key FROM fb_api_token"),
        Some(key.to_owned())
    );
    let dump = db.scalar("SELECT t::text FROM fb_api_token t").unwrap();
    assert!(!dump.contains(secret), "{dump}");
    assert_eq!(
        db.scalar(&format!(
            "SELECT (secret_hash = sha256(convert_to('{secret}', 'UTF8')))::text FROM fb_api_token"
        )),
        Some("true".into())
    );

    let out = db.forgebase(&["token", "revoke", key]);
    assert_eq!(code(&out), Some(0), "{}", stderr(&out));
    assert_eq!(stdout(&out), format!("revoked API token {key}\n"));
    assert!(!stdout(&out).contains(secret));
    assert_eq!(
        db.scalar("SELECT (revoked_at IS NOT NULL)::text FROM fb_api_token"),
        Some("true".into())
    );

    // A well-formed but unknown key.
    let out = db.forgebase(&["token", "revoke", "00000000000000000000000000000000"]);
    assert_eq!(code(&out), Some(1));
    assert!(stderr(&out).contains("not found"), "{}", stderr(&out));
    // A malformed key, also a pasted `key:secret`, is rejected without being echoed.
    let pasted = format!("{key}:{secret}");
    for bad in ["nope", pasted.as_str()] {
        let out = db.forgebase(&["token", "revoke", bad]);
        assert_eq!(code(&out), Some(1));
        assert!(
            stderr(&out).contains("invalid API token key"),
            "{}",
            stderr(&out)
        );
        assert!(!stderr(&out).contains(bad), "{}", stderr(&out));
        assert!(!stderr(&out).contains(secret), "{}", stderr(&out));
        assert_eq!(stdout(&out), "");
    }
    let out = db.forgebase(&["token", "create", "nobody"]);
    assert_eq!(code(&out), Some(1));
    assert_eq!(stdout(&out), "");
    // A disabled user gets no token.
    let out = db.forgebase(&["user", "disable", "alice"]);
    assert_eq!(code(&out), Some(0), "{}", stderr(&out));
    let out = db.forgebase(&["token", "create", "alice"]);
    assert_eq!(code(&out), Some(1));
    assert!(stderr(&out).contains("is disabled"), "{}", stderr(&out));
    assert_eq!(stdout(&out), "");
}

// ---- serve ---------------------------------------------------------------------------------

/// Reads a child's standard output to its end in the background, so a line it prints can never
/// block it or meet a closed pipe.
fn drain<R: std::io::Read + Send + 'static>(mut pipe: R) {
    std::thread::spawn(move || {
        let _ = std::io::copy(&mut pipe, &mut std::io::sink());
    });
}

/// A running `forgebase serve`, killed when dropped so a failing test leaves no process behind.
struct Server {
    child: std::process::Child,
    address: String,
}

impl Server {
    /// Starts `serve` on an ephemeral port and waits for its "listening on" line.
    fn start(db: &TestDb, env: &[(&str, &str)]) -> Server {
        let mut command = Command::new(env!("CARGO_BIN_EXE_forgebase"));
        command
            .args(["serve", "--doctypes", DOCTYPES])
            .env("FORGEBASE_DATABASE_URL", &db.url)
            .env("FORGEBASE_LISTEN", "127.0.0.1:0")
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        for (name, value) in env {
            command.env(name, value);
        }
        let mut child = command.spawn().expect("start forgebase serve");
        let mut line = String::new();
        let mut reader = std::io::BufReader::new(child.stdout.take().expect("stdout"));
        std::io::BufRead::read_line(&mut reader, &mut line).expect("read the listening line");
        drain(reader);
        let address = line
            .trim()
            .strip_prefix("listening on http://")
            .unwrap_or_else(|| panic!("unexpected first line {line:?}"))
            .to_owned();
        Server { child, address }
    }

    /// One HTTP/1.1 request; returns the status code, the head and the body.
    fn request(
        &self,
        method: &str,
        path: &str,
        host: &str,
        headers: &[&str],
        body: &str,
    ) -> (u16, String, String) {
        use std::io::Read;
        let mut stream = std::net::TcpStream::connect(&self.address).expect("connect");
        let mut text = format!("{method} {path} HTTP/1.1\r\nHost: {host}\r\nConnection: close\r\n");
        for header in headers {
            text.push_str(header);
            text.push_str("\r\n");
        }
        text.push_str(&format!("Content-Length: {}\r\n\r\n{body}", body.len()));
        stream.write_all(text.as_bytes()).expect("send");
        let mut reply = String::new();
        stream.read_to_string(&mut reply).expect("read");
        let (head, body) = reply.split_once("\r\n\r\n").expect("head and body");
        let status = head
            .split(' ')
            .nth(1)
            .expect("status")
            .parse()
            .expect("number");
        (status, head.to_owned(), body.to_owned())
    }

    /// Stops the server and returns what it wrote to standard error.
    fn stop(mut self) -> String {
        use std::io::Read;
        let _ = self.child.kill();
        let _ = self.child.wait();
        let mut text = String::new();
        let _ = self
            .child
            .stderr
            .take()
            .expect("stderr")
            .read_to_string(&mut text);
        text
    }
}

impl Drop for Server {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

#[test]
fn serve_answers_http_requests() {
    let db = TestDb::new();
    let out = db.forgebase(&["migrate", "--doctypes", DOCTYPES]);
    assert_eq!(code(&out), Some(0), "{}", stderr(&out));
    let out = db.forgebase_with_stdin(
        &["user", "create", "root", "--role", "Administrator"],
        Some("correct horse battery\n"),
    );
    assert_eq!(code(&out), Some(0), "{}", stderr(&out));

    let server = Server::start(&db, &[]);
    let host = server.address.clone();
    let get =
        |path: &str, host: &str, headers: &[&str]| server.request("GET", path, host, headers, "");
    // No credential: the envelope with a 401.
    let (status, _, body) = get("/api/v1/resource/customer", &host, &[]);
    assert_eq!(status, 401, "{body}");
    assert!(body.contains("\"unauthenticated\""), "{body}");
    // A Host that is not allowed: refused before anything else.
    let (status, _, body) = get("/api/v1/resource/customer", "evil.example", &[]);
    assert_eq!(status, 400, "{body}");
    assert!(body.contains("\"invalid_host\""), "{body}");
    // Login and read, with the cookie as a browser would send it.
    let (status, head, body) = server.request(
        "POST",
        "/api/v1/login",
        &host,
        &["Content-Type: application/json"],
        r#"{"username":"root","password":"correct horse battery"}"#,
    );
    assert_eq!(status, 200, "{body}");
    let cookie = head
        .lines()
        .find_map(|l| l.strip_prefix("set-cookie: "))
        .expect("Set-Cookie");
    assert!(
        cookie.contains("; HttpOnly; Secure; SameSite=Lax;"),
        "{cookie}"
    );
    let pair = cookie.split(';').next().expect("name=value");
    let (status, _, body) = get(
        "/api/v1/resource/customer",
        &host,
        &[&format!("Cookie: {pair}")],
    );
    assert_eq!(status, 200, "{body}");
    assert_eq!(body, r#"{"data":[]}"#);
    let errors = server.stop();
    // Nothing secret on standard error, and no warning in the default mode.
    assert!(!errors.contains("correct horse"), "{errors}");
    assert!(!errors.contains(&db.url), "{errors}");
    assert!(!errors.contains("warning"), "{errors}");
}

#[test]
fn serve_dev_mode_and_configuration() {
    let db = TestDb::new();
    let out = db.forgebase(&["migrate", "--doctypes", DOCTYPES]);
    assert_eq!(code(&out), Some(0), "{}", stderr(&out));
    let out = db.forgebase_with_stdin(
        &["user", "create", "root", "--role", "Administrator"],
        Some("correct horse battery\n"),
    );
    assert_eq!(code(&out), Some(0), "{}", stderr(&out));

    // Dev mode: no `Secure` on the cookie, and a warning at startup. The allowed hosts come
    // from the environment.
    let server = Server::start(
        &db,
        &[
            ("FORGEBASE_DEV_INSECURE_COOKIES", "1"),
            ("FORGEBASE_ALLOWED_HOSTS", "Erp.Local, localhost"),
        ],
    );
    let (status, head, body) = server.request(
        "POST",
        "/api/v1/login",
        "erp.local:8000",
        &["Content-Type: application/json"],
        r#"{"username":"root","password":"correct horse battery"}"#,
    );
    assert_eq!(status, 200, "{body}");
    let cookie = head
        .lines()
        .find_map(|l| l.strip_prefix("set-cookie: "))
        .expect("Set-Cookie");
    assert!(
        cookie.ends_with("; HttpOnly; SameSite=Lax; Path=/; Max-Age=604800"),
        "{cookie}"
    );
    // 127.0.0.1 is not in this list.
    let (status, _, _) = server.request("GET", "/api/v1/login", "127.0.0.1", &[], "");
    assert_eq!(status, 400);
    let errors = server.stop();
    assert!(
        errors.contains("FORGEBASE_DEV_INSECURE_COOKIES=1"),
        "{errors}"
    );

    // An unusable host list or address ends the command with exit code 1 and no secret.
    for (name, value) in [
        ("FORGEBASE_ALLOWED_HOSTS", " , "),
        ("FORGEBASE_ALLOWED_HOSTS", "localhost:8000"),
        ("FORGEBASE_LISTEN", "not an address"),
    ] {
        let output = Command::new(env!("CARGO_BIN_EXE_forgebase"))
            .args(["serve", "--doctypes", DOCTYPES])
            .env("FORGEBASE_DATABASE_URL", &db.url)
            .env(name, value)
            .stdin(Stdio::null())
            .output()
            .expect("run");
        assert_eq!(code(&output), Some(1), "{name}");
        assert!(
            stderr(&output).starts_with("error: "),
            "{}",
            stderr(&output)
        );
        assert!(!stderr(&output).contains(&db.url), "{}", stderr(&output));
    }
    // `serve` needs the DocType directory like `migrate`.
    let output = Command::new(env!("CARGO_BIN_EXE_forgebase"))
        .arg("serve")
        .env("FORGEBASE_DATABASE_URL", &db.url)
        .output()
        .expect("run");
    assert_eq!(code(&output), Some(2));
}

#[test]
fn serve_keeps_running_when_its_standard_output_is_closed() {
    use std::io::{Read, Write};

    let db = TestDb::new();
    let out = db.forgebase(&["migrate", "--doctypes", DOCTYPES]);
    assert_eq!(code(&out), Some(0), "{}", stderr(&out));
    // A free port, so the address is known without reading the "listening on" line.
    let port = std::net::TcpListener::bind("127.0.0.1:0")
        .expect("bind")
        .local_addr()
        .expect("address")
        .port();
    let address = format!("127.0.0.1:{port}");
    let mut child = Command::new(env!("CARGO_BIN_EXE_forgebase"))
        .args(["serve", "--doctypes", DOCTYPES])
        .env("FORGEBASE_DATABASE_URL", &db.url)
        .env("FORGEBASE_LISTEN", &address)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .expect("start forgebase serve");
    // Close the read end at once: the server's first write to standard output fails. (`null()`
    // would not do: writes to it succeed.)
    drop(child.stdout.take());
    // Wait until the server listens, then ask once: a server that stopped over the failed write
    // resets the connection or never listens.
    let started = std::time::Instant::now();
    let mut stream = loop {
        match std::net::TcpStream::connect(&address) {
            Ok(stream) => break stream,
            Err(error) => {
                assert!(
                    started.elapsed() < std::time::Duration::from_secs(30),
                    "the server never listened: {error}"
                );
                std::thread::sleep(std::time::Duration::from_millis(20));
            }
        }
    };
    stream
        .write_all(
            format!("GET /api/v1/resource/customer HTTP/1.1\r\nHost: {address}\r\nConnection: close\r\n\r\n")
                .as_bytes(),
        )
        .expect("send");
    let mut reply = String::new();
    let read = stream.read_to_string(&mut reply);
    let alive = child.try_wait().expect("wait").is_none();
    let _ = child.kill();
    let _ = child.wait();
    read.unwrap_or_else(|e| panic!("the server did not answer: {e}"));
    assert!(reply.starts_with("HTTP/1.1 401"), "{reply}");
    assert!(alive, "the server exited");
}

#[test]
fn serve_serves_the_desk_only_when_configured() {
    let db = TestDb::new();
    let out = db.forgebase(&["migrate", "--doctypes", DOCTYPES]);
    assert_eq!(code(&out), Some(0), "{}", stderr(&out));

    let desk = std::env::temp_dir().join(format!("forgebase_test_desk_{}", std::process::id()));
    std::fs::create_dir_all(desk.join("assets")).expect("desk directory");
    std::fs::write(desk.join("index.html"), "<title>desk</title>").expect("index");
    std::fs::write(desk.join("assets/app.js"), "1").expect("asset");

    // Not configured: no desk.
    let server = Server::start(&db, &[]);
    let host = server.address.clone();
    let (status, _, _) = server.request("GET", "/desk/", &host, &[], "");
    assert_eq!(status, 404);
    server.stop();

    // Configured: the index and the asset, with the security headers.
    let server = Server::start(
        &db,
        &[("FORGEBASE_DESK_DIR", desk.to_str().expect("UTF-8"))],
    );
    let host = server.address.clone();
    let (status, head, body) = server.request("GET", "/desk/", &host, &[], "");
    assert_eq!(status, 200, "{body}");
    assert_eq!(body, "<title>desk</title>");
    assert!(
        head.to_ascii_lowercase()
            .contains("content-security-policy: default-src 'self'")
    );
    let (status, _, body) = server.request("GET", "/desk/assets/app.js", &host, &[], "");
    assert_eq!((status, body.as_str()), (200, "1"));
    server.stop();

    // A directory without index.html ends the command with exit code 1.
    let output = Command::new(env!("CARGO_BIN_EXE_forgebase"))
        .args(["serve", "--doctypes", DOCTYPES])
        .env("FORGEBASE_DATABASE_URL", &db.url)
        .env("FORGEBASE_DESK_DIR", desk.join("assets"))
        .stdin(Stdio::null())
        .output()
        .expect("run");
    assert_eq!(code(&output), Some(1));
    assert!(
        stderr(&output).contains("FORGEBASE_DESK_DIR"),
        "{}",
        stderr(&output)
    );
    assert!(!stderr(&output).contains(&db.url), "{}", stderr(&output));
    let _ = std::fs::remove_dir_all(&desk);
}

#[test]
fn the_worker_of_a_binary_without_app_jobs_runs_the_built_in_jobs_and_stops_on_sigterm() {
    use std::io::{BufRead, BufReader};

    let db = TestDb::new();
    // The stock binary has no apps, but the framework's own clean-up jobs: the worker starts.
    let migrated = db.forgebase(&["migrate", "--doctypes", DOCTYPES]);
    assert_eq!(code(&migrated), Some(0), "{}", stderr(&migrated));
    assert_eq!(
        db.scalar("SELECT string_agg(name || ' ' || spec, ', ' ORDER BY name) FROM fb_schedule")
            .as_deref(),
        Some(
            "forgebase.purge_jobs daily 03:00, forgebase.purge_login_failures every 60m, forgebase.purge_sessions every 60m"
        )
    );
    let mut child = Command::new(env!("CARGO_BIN_EXE_forgebase"))
        .args(["worker", "--doctypes", DOCTYPES])
        .env("FORGEBASE_DATABASE_URL", &db.url)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("start the worker");
    drain(child.stdout.take().expect("stdout"));
    // Wait for the real precondition: the worker says it started.
    let mut lines = BufReader::new(child.stderr.take().expect("stderr"));
    let mut log = String::new();
    loop {
        let mut line = String::new();
        let read = lines.read_line(&mut line).expect("read the worker log");
        log.push_str(&line);
        assert!(read > 0, "the worker ended before it started: {log}");
        if line.contains("started ") {
            break;
        }
    }
    assert!(log.contains("4 job type(s)"), "{log}");
    let killed = Command::new("kill")
        .args(["-TERM", &child.id().to_string()])
        .status()
        .expect("kill");
    assert!(killed.success());
    let status = child.wait().expect("worker exit");
    let mut rest = String::new();
    std::io::Read::read_to_string(&mut lines, &mut rest).expect("read the rest of the log");
    assert_eq!(status.code(), Some(0), "{log}{rest}");
    assert!(rest.contains("stopped"), "{rest}");
    // A job name is checked against the binary's jobs: here there are none.
    let output = db.forgebase(&["job", "enqueue", "crm.welcome"]);
    assert_eq!(code(&output), Some(1));
    // (an app's job name is unknown to this binary)
    assert!(
        stderr(&output).starts_with("error: "),
        "{}",
        stderr(&output)
    );
    // Usage errors are exit code 2.
    assert_eq!(
        code(&db.forgebase(&["worker", "--doctypes", DOCTYPES, "--concurrency", "0"])),
        Some(2)
    );
    assert_eq!(
        code(&db.forgebase(&["worker", "--doctypes", DOCTYPES, "--concurrency", "65"])),
        Some(2)
    );
}

#[test]
fn doc_transition_takes_a_workflow_transition_as_the_system_context() {
    let db = TestDb::new();
    let migrate = db.forgebase(&["migrate", "--doctypes", WORKFLOW_DOCTYPES]);
    assert_eq!(code(&migrate), Some(0), "{}", stderr(&migrate));
    let run = |args: &[&str]| {
        let mut full = args.to_vec();
        full.extend_from_slice(&["--doctypes", WORKFLOW_DOCTYPES]);
        db.forgebase(&full)
    };
    let inserted = run(&["doc", "insert", "purchase", "--data", r#"{"title":"p"}"#]);
    assert_eq!(code(&inserted), Some(0), "{}", stderr(&inserted));
    let purchase = json(&inserted);
    let name = purchase["name"].as_str().expect("name").to_owned();
    assert_eq!(purchase["workflow_state"], "Draft");
    assert_eq!(purchase["docstatus"], 0);

    // The roles are not checked: the actor has none, and Approve names a role nobody has.
    let sent = run(&[
        "doc",
        "transition",
        "purchase",
        &name,
        "Send",
        "--version",
        "1",
        "--actor",
        "ann",
    ]);
    assert_eq!(code(&sent), Some(0), "{}", stderr(&sent));
    let sent = json(&sent);
    assert_eq!(
        (
            &sent["workflow_state"],
            &sent["version"],
            &sent["modified_by"]
        ),
        (&Json::from("Pending"), &Json::from(2), &Json::from("ann"))
    );
    // `doc get` prints the state too.
    let got = json(&run(&["doc", "get", "purchase", &name]));
    assert_eq!(got["workflow_state"], "Pending");

    // Errors and exit codes are those of `doc submit`: 1 with the message on standard error.
    let stale = run(&[
        "doc",
        "transition",
        "purchase",
        &name,
        "Approve",
        "--version",
        "9",
    ]);
    assert_eq!(code(&stale), Some(1));
    assert!(
        stderr(&stale).contains("current version 2"),
        "{}",
        stderr(&stale)
    );
    let wrong = run(&[
        "doc",
        "transition",
        "purchase",
        &name,
        "Cancel",
        "--version",
        "2",
    ]);
    assert_eq!(code(&wrong), Some(1));
    assert!(
        stderr(&wrong).contains("is in state `Pending`: no transition `Cancel` from it"),
        "{}",
        stderr(&wrong)
    );
    // Without --version it is a usage error (exit 2).
    let missing = run(&["doc", "transition", "purchase", &name, "Approve"]);
    assert_eq!(code(&missing), Some(2));

    // Plain submit and cancel are refused on a DocType with a workflow.
    for step in ["submit", "cancel"] {
        let refused = run(&["doc", step, "purchase", &name, "--version", "2"]);
        assert_eq!(code(&refused), Some(1), "{step}");
        assert!(
            stderr(&refused).contains("has a workflow; use a transition"),
            "{}",
            stderr(&refused)
        );
    }
    // Approve submits the document, Cancel cancels it.
    let approved = json(&run(&[
        "doc",
        "transition",
        "purchase",
        &name,
        "Approve",
        "--version",
        "2",
    ]));
    assert_eq!(
        (&approved["workflow_state"], &approved["docstatus"]),
        (&Json::from("Approved"), &Json::from(1))
    );
    let cancelled = json(&run(&[
        "doc",
        "transition",
        "purchase",
        &name,
        "Cancel",
        "--version",
        "3",
    ]));
    assert_eq!(
        (&cancelled["workflow_state"], &cancelled["docstatus"]),
        (&Json::from("Cancelled"), &Json::from(2))
    );
    // The log has every transition with its actor.
    assert_eq!(
        db.scalar(
            "SELECT string_agg(action || ':' || actor, ',' ORDER BY id) FROM fb_workflow_log"
        )
        .as_deref(),
        Some("Send:ann,Approve:cli,Cancel:cli")
    );

    // A DocType without a workflow: `transition` says so.
    let other = db.forgebase(&["migrate", "--doctypes", DOCTYPES]);
    assert_eq!(code(&other), Some(0), "{}", stderr(&other));
    let none = db.forgebase(&[
        "doc",
        "transition",
        "customer",
        "x",
        "Send",
        "--version",
        "1",
        "--doctypes",
        DOCTYPES,
    ]);
    assert_eq!(code(&none), Some(1));
}

#[test]
fn migrate_gives_documents_their_state_and_warns_about_a_state_that_is_gone() {
    let db = TestDb::new();
    // The DocType without its workflow: a draft, a submitted and a cancelled document.
    let dir = std::env::temp_dir().join(format!("forgebase-test-workflow-{}", std::process::id()));
    std::fs::create_dir_all(&dir).expect("create dir");
    let text =
        std::fs::read_to_string(format!("{WORKFLOW_DOCTYPES}/purchase.toml")).expect("fixture");
    let without = text
        .split("[workflow]")
        .next()
        .expect("before the workflow");
    std::fs::write(dir.join("purchase.toml"), without).expect("write");
    let before = dir.to_str().expect("UTF-8 path").to_owned();
    let result = std::panic::catch_unwind(|| {
        let migrate = db.forgebase(&["migrate", "--doctypes", &before]);
        assert_eq!(code(&migrate), Some(0), "{}", stderr(&migrate));
        for _ in 0..3 {
            let inserted = db.forgebase(&[
                "doc",
                "insert",
                "purchase",
                "--data",
                r#"{"title":"p"}"#,
                "--doctypes",
                &before,
            ]);
            assert_eq!(code(&inserted), Some(0), "{}", stderr(&inserted));
        }
        db.exec("UPDATE doc_purchase SET docstatus = 1 WHERE name = (SELECT name FROM doc_purchase ORDER BY created_at, id LIMIT 1 OFFSET 1)");
        db.exec("UPDATE doc_purchase SET docstatus = 2 WHERE name = (SELECT name FROM doc_purchase ORDER BY created_at, id LIMIT 1 OFFSET 2)");

        // Adding the workflow: the column is added and the documents get a state per docstatus.
        let migrate = db.forgebase(&["migrate", "--doctypes", WORKFLOW_DOCTYPES]);
        assert_eq!(code(&migrate), Some(0), "{}", stderr(&migrate));
        assert!(
            stdout(&migrate).contains("workflow_state"),
            "{}",
            stdout(&migrate)
        );
        assert!(stderr(&migrate).is_empty(), "{}", stderr(&migrate));
        assert_eq!(
            db.scalar("SELECT string_agg(docstatus::text || '=' || workflow_state, ',' ORDER BY docstatus) FROM doc_purchase")
                .as_deref(),
            Some("0=Draft,1=Approved,2=Cancelled")
        );
        // Again: no change, no warning.
        let again = db.forgebase(&["migrate", "--doctypes", WORKFLOW_DOCTYPES]);
        assert_eq!(code(&again), Some(0), "{}", stderr(&again));
        assert!(stderr(&again).is_empty(), "{}", stderr(&again));

        // A state the workflow no longer has: one warning line on standard error, exit 0.
        db.exec("UPDATE doc_purchase SET workflow_state = 'Gone' WHERE docstatus > 0");
        let warned = db.forgebase(&["migrate", "--doctypes", WORKFLOW_DOCTYPES]);
        assert_eq!(code(&warned), Some(0), "{}", stderr(&warned));
        assert_eq!(
            stderr(&warned),
            "warning: site `default`: DocType `purchase`: 2 documents are in a workflow state that no longer exists; no transition applies to them\n"
        );
    });
    let _ = std::fs::remove_dir_all(&dir);
    if let Err(panic) = result {
        std::panic::resume_unwind(panic);
    }
}

// ---- webhooks (M9.2, ADR 0020) ---------------------------------------------------------------

/// The secret an `add` or `rotate-secret` printed (the 64 hex characters after `secret: ` or
/// `: `), and how many times it is in the text.
fn printed_secret(text: &str) -> String {
    let secret = text
        .lines()
        .find_map(|l| {
            l.strip_prefix("secret: ").or_else(|| {
                l.split_once("webhook ")
                    .and_then(|(_, r)| r.split_once(": "))
                    .map(|(_, s)| s)
            })
        })
        .unwrap_or_else(|| panic!("no secret printed in: {text}"))
        .to_owned();
    assert_eq!(secret.len(), 64, "{text}");
    assert!(secret.bytes().all(|b| b.is_ascii_hexdigit()), "{text}");
    secret
}

fn webhook_db() -> TestDb {
    let db = TestDb::new();
    let migrated = db.forgebase(&["migrate", "--doctypes", DOCTYPES]);
    assert_eq!(code(&migrated), Some(0), "{}", stderr(&migrated));
    db
}

fn add_webhook(db: &TestDb, name: &str, url: &str, flags: &[&str]) -> Output {
    let mut args = vec![
        "webhook",
        "add",
        name,
        "--doctype",
        "customer",
        "--events",
        "insert,update",
        "--url",
        url,
        "--doctypes",
        DOCTYPES,
    ];
    args.extend_from_slice(flags);
    db.forgebase(&args)
}

#[test]
fn webhooks_are_added_listed_disabled_enabled_and_removed() {
    let db = webhook_db();
    let added = add_webhook(
        &db,
        "orders",
        "https://hooks.example.com:8443/services/T0/B0/TOPSECRET?token=TOPSECRET",
        &[],
    );
    assert_eq!(code(&added), Some(0), "{}", stderr(&added));
    let out = stdout(&added);
    assert!(out.starts_with("added webhook orders\n"), "{out}");
    let secret = printed_secret(&out);
    // Printed once, and it is what the database holds.
    assert_eq!(out.matches(&secret).count(), 1, "{out}");
    assert_eq!(
        db.scalar("SELECT secret FROM fb_webhook WHERE name = 'orders'")
            .as_deref(),
        Some(secret.as_str())
    );
    assert!(stderr(&added).is_empty(), "{}", stderr(&added));

    // `list`: the host only, the flags, never the secret or the path.
    let listed = db.forgebase(&["webhook", "list"]);
    assert_eq!(code(&listed), Some(0), "{}", stderr(&listed));
    let table = stdout(&listed);
    let header: Vec<&str> = table
        .lines()
        .next()
        .expect("header")
        .split_whitespace()
        .collect();
    assert_eq!(
        header,
        [
            "NAME",
            "DOCTYPE",
            "EVENTS",
            "URL",
            "ENABLED",
            "ALLOW_PRIVATE",
            "INSECURE_HTTP"
        ]
    );
    let row: Vec<&str> = table
        .lines()
        .nth(1)
        .expect("row")
        .split_whitespace()
        .collect();
    assert_eq!(
        row,
        [
            "orders",
            "customer",
            "insert,update",
            "https://hooks.example.com:8443",
            "yes",
            "no",
            "no"
        ]
    );
    for forbidden in [secret.as_str(), "TOPSECRET", "services", "token"] {
        assert!(!table.contains(forbidden), "{forbidden} in {table}");
        assert!(!stderr(&listed).contains(forbidden));
    }

    // Disable, enable, remove.
    let disabled = db.forgebase(&["webhook", "disable", "orders"]);
    assert_eq!(code(&disabled), Some(0), "{}", stderr(&disabled));
    let after_disable = stdout(&db.forgebase(&["webhook", "list"]));
    let row: Vec<&str> = after_disable
        .lines()
        .nth(1)
        .expect("row")
        .split_whitespace()
        .collect();
    assert_eq!(row[4], "no", "disabled: {after_disable}");
    assert_eq!(
        db.scalar("SELECT enabled::text FROM fb_webhook").as_deref(),
        Some("false")
    );
    let enabled = db.forgebase(&["webhook", "enable", "orders"]);
    assert_eq!(code(&enabled), Some(0), "{}", stderr(&enabled));
    assert_eq!(
        db.scalar("SELECT enabled::text FROM fb_webhook").as_deref(),
        Some("true")
    );
    let removed = db.forgebase(&["webhook", "remove", "orders"]);
    assert_eq!(code(&removed), Some(0), "{}", stderr(&removed));
    assert_eq!(
        stdout(&db.forgebase(&["webhook", "list"])).lines().count(),
        1,
        "only the header"
    );

    // A webhook that is not there.
    for command in ["disable", "enable", "remove", "rotate-secret", "test"] {
        let missing = db.forgebase(&["webhook", command, "orders"]);
        assert_eq!(code(&missing), Some(1), "{command}");
        assert!(
            stderr(&missing).contains("no webhook named `orders`"),
            "{command}: {}",
            stderr(&missing)
        );
    }

    // The same name twice is refused.
    assert_eq!(
        code(&add_webhook(&db, "dup", "https://example.com/x", &[])),
        Some(0)
    );
    let again = add_webhook(&db, "dup", "https://example.com/y", &[]);
    assert_eq!(code(&again), Some(1));
    assert!(
        stderr(&again).contains("exists already"),
        "{}",
        stderr(&again)
    );
}

#[test]
fn a_webhook_that_is_not_valid_is_refused_and_nothing_is_stored() {
    let db = webhook_db();
    let url = "https://example.com/hook-path";
    let with = |name: &str, doctype: &str, events: &str, url: &str, flags: &[&str]| {
        let mut args = vec![
            "webhook",
            "add",
            name,
            "--doctype",
            doctype,
            "--events",
            events,
            "--url",
            url,
            "--doctypes",
            DOCTYPES,
        ];
        args.extend_from_slice(flags);
        db.forgebase(&args)
    };
    let cases: Vec<(Output, &str)> = vec![
        (
            with("w", "nobody", "insert", url, &[]),
            "unknown DocType `nobody`",
        ),
        // A child DocType has no documents of its own.
        (
            with("w", "sales_order_item", "insert", url, &[]),
            "child DocType",
        ),
        (
            with(
                "w",
                "customer",
                "insert",
                "http://example.com/hook-path",
                &[],
            ),
            "--insecure-http",
        ),
        (
            with(
                "w",
                "customer",
                "insert",
                "https://user:pw@example.com/hook-path",
                &[],
            ),
            "user info",
        ),
        (
            with(
                "w",
                "customer",
                "insert",
                "https://example.com@evil.test/hook-path",
                &[],
            ),
            "user info",
        ),
        (
            with(
                "w",
                "customer",
                "insert",
                "ftp://example.com/hook-path",
                &[],
            ),
            "https",
        ),
        (
            with("w", "customer", "insert", "example.com/hook-path", &[]),
            "absolute URL",
        ),
        (
            with("w", "customer", "insert", "not a url", &[]),
            "valid absolute URL",
        ),
        (with("w", "customer", "", url, &[]), "event"),
        (
            with("w", "customer", "insert,save", url, &[]),
            "unknown event `save`",
        ),
        (
            with("Bad Name", "customer", "insert", url, &[]),
            "must match",
        ),
    ];
    for (output, expected) in cases {
        assert_eq!(code(&output), Some(1), "{expected}: {}", stderr(&output));
        assert!(stdout(&output).is_empty(), "{}", stdout(&output));
        let message = stderr(&output);
        assert!(message.contains(expected), "{expected}: {message}");
        // The URL is never repeated: it may hold credentials or a token.
        for forbidden in ["hook-path", "pw@", "evil.test"] {
            assert!(!message.contains(forbidden), "{forbidden} in {message}");
        }
    }
    assert_eq!(
        db.scalar("SELECT count(*)::text FROM fb_webhook")
            .as_deref(),
        Some("0")
    );

    // `http://` is allowed with the explicit development flag, and the list shows it.
    let dev = with(
        "dev",
        "customer",
        "insert",
        "http://localhost:8080/hook",
        &["--insecure-http"],
    );
    assert_eq!(code(&dev), Some(0), "{}", stderr(&dev));
    assert!(
        stdout(&dev).contains("--insecure-http"),
        "a warning: {}",
        stdout(&dev)
    );
    let table = stdout(&db.forgebase(&["webhook", "list"]));
    let row: Vec<&str> = table
        .lines()
        .nth(1)
        .expect("row")
        .split_whitespace()
        .collect();
    assert_eq!(
        row,
        [
            "dev",
            "customer",
            "insert",
            "http://localhost:8080",
            "yes",
            "no",
            "yes"
        ],
        "{table}"
    );
}

#[test]
fn a_secret_from_standard_input_is_used_and_not_printed_and_rotation_prints_once() {
    let db = webhook_db();
    let mut args = vec![
        "webhook",
        "add",
        "w",
        "--doctype",
        "customer",
        "--events",
        "insert",
        "--url",
        "https://example.com/x",
        "--doctypes",
        DOCTYPES,
        "--secret-stdin",
    ];
    let added = db.forgebase_with_stdin(&args, Some("my own secret value\n"));
    assert_eq!(code(&added), Some(0), "{}", stderr(&added));
    assert!(
        !stdout(&added).contains("my own secret value"),
        "{}",
        stdout(&added)
    );
    assert_eq!(
        db.scalar("SELECT secret FROM fb_webhook").as_deref(),
        Some("my own secret value")
    );
    // An empty secret is refused.
    args[2] = "w2";
    let empty = db.forgebase_with_stdin(&args, Some("\n"));
    assert_eq!(code(&empty), Some(1));
    assert!(
        stderr(&empty).contains("the secret is empty"),
        "{}",
        stderr(&empty)
    );

    // A secret of less than 16 bytes is refused, by `add` and by `rotate-secret`, and changes nothing.
    args[2] = "w3";
    let short = db.forgebase_with_stdin(&args, Some("fifteen-bytes!!\n"));
    assert_eq!(code(&short), Some(1));
    assert!(
        stderr(&short).contains("at least 16 bytes"),
        "{}",
        stderr(&short)
    );
    assert_eq!(
        db.scalar("SELECT count(*)::text FROM fb_webhook")
            .as_deref(),
        Some("1")
    );
    let short = db.forgebase_with_stdin(
        &["webhook", "rotate-secret", "w", "--secret-stdin"],
        Some("too short\n"),
    );
    assert_eq!(code(&short), Some(1));
    assert!(
        stderr(&short).contains("at least 16 bytes"),
        "{}",
        stderr(&short)
    );
    assert_eq!(
        db.scalar("SELECT secret FROM fb_webhook").as_deref(),
        Some("my own secret value")
    );
    let sixteen = db.forgebase_with_stdin(
        &["webhook", "rotate-secret", "w", "--secret-stdin"],
        Some("exactly-16-bytes\n"),
    );
    assert_eq!(code(&sixteen), Some(0), "{}", stderr(&sixteen));
    assert_eq!(
        db.scalar("SELECT secret FROM fb_webhook").as_deref(),
        Some("exactly-16-bytes")
    );

    let rotated = db.forgebase(&["webhook", "rotate-secret", "w"]);
    assert_eq!(code(&rotated), Some(0), "{}", stderr(&rotated));
    let new = printed_secret(&stdout(&rotated));
    assert_eq!(stdout(&rotated).matches(&new).count(), 1);
    assert_eq!(
        db.scalar("SELECT secret FROM fb_webhook").as_deref(),
        Some(new.as_str())
    );
    assert!(!stdout(&rotated).contains("exactly-16-bytes"));
}

#[test]
fn webhook_test_sends_one_signed_delivery_and_exits_by_the_answer() {
    use common::{Receiver, Reply, expected_signature};
    let db = webhook_db();
    let receiver = Receiver::http(Reply::Status(200));
    let url = receiver.url("/hook-path?token=TOPSECRET");
    let added = add_webhook(&db, "w", &url, &["--allow-private", "--insecure-http"]);
    assert_eq!(code(&added), Some(0), "{}", stderr(&added));
    let secret = printed_secret(&stdout(&added));

    let ok = db.forgebase(&["webhook", "test", "w"]);
    assert_eq!(code(&ok), Some(0), "{}", stderr(&ok));
    let text = stdout(&ok);
    assert!(
        text.contains("HTTP 200")
            && text.contains(&format!("http://127.0.0.1:{}", receiver.addr.port())),
        "{text}"
    );
    for forbidden in [secret.as_str(), "TOPSECRET", "hook-path"] {
        assert!(
            !text.contains(forbidden) && !stderr(&ok).contains(forbidden),
            "{forbidden}"
        );
    }
    let requests = receiver.requests();
    assert_eq!(requests.len(), 1);
    let request = &requests[0];
    assert_eq!(request.header("x-forgebase-event"), Some("test"));
    let delivery = request.header("x-forgebase-delivery").expect("delivery");
    assert!(delivery.starts_with("test-"), "{delivery}");
    let timestamp = request.header("x-forgebase-timestamp").expect("timestamp");
    assert_eq!(
        request.header("x-forgebase-signature"),
        Some(expected_signature(&secret, timestamp, &request.body).as_str())
    );
    let body = request.json();
    assert_eq!(body["event"], "test");
    assert_eq!(body["doctype"], "customer");
    assert_eq!(body["delivery"], delivery);
    assert!(body["data"].is_null());

    // A disabled webhook can be tested.
    assert_eq!(code(&db.forgebase(&["webhook", "disable", "w"])), Some(0));
    assert_eq!(code(&db.forgebase(&["webhook", "test", "w"])), Some(0));

    // Not a 2xx answer: exit 1, the status on standard output.
    for (reply, status) in [
        (Reply::Status(500), "500"),
        (Reply::Status(404), "404"),
        (Reply::Redirect(302, receiver.url("/other")), "302"),
    ] {
        receiver.set_reply(reply);
        let failed = db.forgebase(&["webhook", "test", "w"]);
        assert_eq!(code(&failed), Some(1), "{status}");
        assert!(
            stdout(&failed).contains(&format!("HTTP {status}")),
            "{}",
            stdout(&failed)
        );
        assert!(
            stderr(&failed).contains("not accepted"),
            "{}",
            stderr(&failed)
        );
        for forbidden in [secret.as_str(), "TOPSECRET", "hook-path"] {
            assert!(!stdout(&failed).contains(forbidden) && !stderr(&failed).contains(forbidden));
        }
    }
}

#[test]
fn webhook_test_refuses_a_private_address_unless_the_webhook_allows_it() {
    use common::{Receiver, Reply};
    let db = webhook_db();
    let receiver = Receiver::http(Reply::Status(200));
    let url = receiver.url("/hook-path?token=TOPSECRET");
    // Plain http is allowed by the flag; loopback is not, since --allow-private is missing.
    let added = add_webhook(&db, "w", &url, &["--insecure-http"]);
    assert_eq!(code(&added), Some(0), "{}", stderr(&added));
    let secret = printed_secret(&stdout(&added));
    let refused = db.forgebase(&["webhook", "test", "w"]);
    assert_eq!(code(&refused), Some(1));
    let message = stderr(&refused);
    assert!(message.contains("--allow-private"), "{message}");
    for forbidden in [secret.as_str(), "TOPSECRET", "hook-path"] {
        assert!(
            !message.contains(forbidden) && !stdout(&refused).contains(forbidden),
            "{forbidden}"
        );
    }
    assert!(receiver.requests().is_empty(), "nothing was sent");
}

#[test]
fn a_document_written_by_the_cli_enqueues_the_delivery() {
    let db = webhook_db();
    assert_eq!(
        code(&add_webhook(&db, "w", "https://example.com/x", &[])),
        Some(0)
    );
    let inserted = db.forgebase(&[
        "doc",
        "insert",
        "customer",
        "--doctypes",
        DOCTYPES,
        "--data",
        r#"{"email":"a@example.com","full_name":"A"}"#,
    ]);
    assert_eq!(code(&inserted), Some(0), "{}", stderr(&inserted));
    assert_eq!(
        db.scalar("SELECT name || '/' || (payload->>'event') || '/' || (payload->'data'->>'email') || '/' || enqueued_by FROM fb_job")
            .as_deref(),
        Some("forgebase.deliver_webhook/insert/a@example.com/cli")
    );
    // The job is visible to `job list`, and its payload (the document, not the secret) to `job show`.
    let listed = db.forgebase(&["job", "list"]);
    assert!(
        stdout(&listed).contains("forgebase.deliver_webhook"),
        "{}",
        stdout(&listed)
    );
}

// ---------------------------------------------------------------------------------------------
// `data export` and `data import` (M9.3, ADR 0021)
// ---------------------------------------------------------------------------------------------

/// A scratch directory for the files of one test, removed when dropped.
struct Scratch(std::path::PathBuf);

impl Scratch {
    fn new() -> Self {
        static COUNTER: AtomicU32 = AtomicU32::new(0);
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map_or(0, |d| d.subsec_nanos());
        let dir = std::env::temp_dir().join(format!(
            "fb_cli_data_{}_{nanos}_{}",
            std::process::id(),
            COUNTER.fetch_add(1, Ordering::Relaxed)
        ));
        std::fs::create_dir_all(&dir).expect("scratch directory");
        Self(dir)
    }

    fn file(&self, name: &str, content: &str) -> String {
        let path = self.0.join(name);
        std::fs::write(&path, content).expect("write file");
        path.to_string_lossy().into_owned()
    }

    fn path(&self, name: &str) -> String {
        self.0.join(name).to_string_lossy().into_owned()
    }
}

impl Drop for Scratch {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

fn insert_customer(db: &TestDb, email: &str, name: &str, points: i64) {
    let data = serde_json::json!({"email": email, "full_name": name, "loyalty_points": points})
        .to_string();
    let out = db.forgebase(&[
        "doc",
        "insert",
        "customer",
        "--doctypes",
        DOCTYPES,
        "--data",
        &data,
    ]);
    assert_eq!(code(&out), Some(0), "{}", stderr(&out));
}

#[test]
fn data_export_writes_csv_and_json_to_stdout_or_a_new_file() {
    let db = migrated_db();
    insert_customer(&db, "a@x.y", "Ann, \"A\"", 30);
    insert_customer(&db, "b@x.y", "=Bob", -5);
    insert_customer(&db, "c@x.y", "Cy", 20);
    let export = |extra: &[&str]| {
        let mut args = vec!["data", "export", "customer", "--doctypes", DOCTYPES];
        args.extend_from_slice(extra);
        db.forgebase(&args)
    };

    let out = export(&[
        "--fields",
        "name,full_name,loyalty_points",
        "--sort",
        "loyalty_points:desc",
        "--filter",
        r#"[{"field":"loyalty_points","op":">","value":0}]"#,
    ]);
    assert_eq!(code(&out), Some(0), "{}", stderr(&out));
    assert_eq!(
        stdout(&out),
        "name,full_name,loyalty_points\r\na@x.y,\"Ann, \"\"A\"\"\",30\r\nc@x.y,Cy,20\r\n"
    );
    // The formula mitigation applies to text, not to the number -5.
    let out = export(&["--fields", "full_name,loyalty_points", "--sort", "name"]);
    assert!(stdout(&out).contains("'=Bob,-5\r\n"), "{}", stdout(&out));
    // JSON: an array of documents.
    let out = export(&["--format", "json", "--sort", "name"]);
    assert_eq!(code(&out), Some(0), "{}", stderr(&out));
    let documents = json(&out);
    assert_eq!(documents.as_array().map(Vec::len), Some(3));
    assert_eq!(documents[0]["name"], "a@x.y");
    assert_eq!(documents[0]["full_name"], "Ann, \"A\"");

    // --out writes a new file and never overwrites; a bad option writes nothing.
    let scratch = Scratch::new();
    let path = scratch.path("customers.csv");
    let out = export(&["--out", &path, "--fields", "name"]);
    assert_eq!(code(&out), Some(0), "{}", stderr(&out));
    assert!(stdout(&out).contains("wrote 3 row(s)"), "{}", stdout(&out));
    assert_eq!(
        std::fs::read_to_string(&path).unwrap(),
        "name\r\na@x.y\r\nb@x.y\r\nc@x.y\r\n"
    );
    let again = export(&["--out", &path]);
    assert_eq!(code(&again), Some(1));
    assert!(
        stderr(&again).contains("cannot create"),
        "{}",
        stderr(&again)
    );
    assert_eq!(std::fs::read_to_string(&path).unwrap().lines().count(), 4);
    for bad in [
        &["--fields", "nope"][..],
        &["--fields", "name,name"],
        &["--filter", "notjson"],
        &["--sort", "nope"],
    ] {
        let out = export(bad);
        assert_eq!(code(&out), Some(1), "{bad:?}");
        assert_eq!(stdout(&out), "", "{bad:?}");
    }
    // The limit refuses before anything is written, and --limit-override raises it.
    let out = export(&["--limit-override", "2"]);
    assert_eq!(code(&out), Some(1));
    assert_eq!(stdout(&out), "");
    assert!(
        stderr(&out).contains("3 rows, over the limit of 2"),
        "{}",
        stderr(&out)
    );
    assert_eq!(code(&export(&["--limit-override", "3"])), Some(0));
}

#[test]
fn data_import_dry_run_failure_exit_code_and_success() {
    let db = migrated_db();
    let scratch = Scratch::new();
    let import = |file: &str, extra: &[&str]| {
        let mut args = vec!["data", "import", "customer", file, "--doctypes", DOCTYPES];
        args.extend_from_slice(extra);
        db.forgebase(&args)
    };
    let count = || {
        db.scalar("SELECT count(*)::text FROM doc_customer")
            .unwrap()
    };
    let good = scratch.file(
        "good.csv",
        "email,full_name,loyalty_points\r\na@x.y,Ann,3\r\nb@x.y,Bob,\r\n",
    );
    let bad = scratch.file(
        "bad.csv",
        "email,full_name,loyalty_points\r\na@x.y,Ann,3\r\nb@x.y,,x\r\nc@x.y,Cy,1\r\n",
    );

    // A dry run keeps nothing, exit 0.
    let out = import(&good, &["--format", "csv", "--dry-run"]);
    assert_eq!(code(&out), Some(0), "{}", stderr(&out));
    let summary = json(&out);
    assert_eq!(summary["dry_run"], true);
    assert_eq!(
        (summary["rows"].as_u64(), summary["inserted"].as_u64()),
        (Some(2), Some(2))
    );
    assert_eq!(count(), "0");

    // A failing file: the summary with the row errors on stdout, exit 1, nothing imported.
    let out = import(&bad, &["--format", "csv"]);
    assert_eq!(code(&out), Some(1));
    let summary = json(&out);
    assert_eq!(summary["error_count"], 1);
    assert_eq!(summary["errors"][0]["row"], 2);
    assert_eq!(summary["errors"][0]["name"], "b@x.y");
    assert_eq!(summary["errors"][0]["line"], 3);
    assert!(
        stderr(&out).contains("nothing was imported"),
        "{}",
        stderr(&out)
    );
    assert_eq!(count(), "0");
    // A dry run of a failing file also exits 1.
    assert_eq!(
        code(&import(&bad, &["--format", "csv", "--dry-run"])),
        Some(1)
    );

    // The real import, recorded as --actor.
    let out = import(&good, &["--format", "csv", "--actor", "importer"]);
    assert_eq!(code(&out), Some(0), "{}", stderr(&out));
    assert_eq!(json(&out)["inserted"], 2);
    assert_eq!(count(), "2");
    assert_eq!(
        db.scalar("SELECT owner FROM doc_customer WHERE name = 'a@x.y'")
            .as_deref(),
        Some("importer")
    );

    // An upsert updates by name (the empty cell is null) and inserts the rest.
    let upsert = scratch.file(
        "upsert.csv",
        "name,full_name,loyalty_points\r\na@x.y,Ann B,\r\nz@x.y,Zed,9\r\n",
    );
    let out = import(&upsert, &["--format", "csv", "--upsert"]);
    assert_eq!(code(&out), Some(1), "z@x.y has no email: {}", stdout(&out));
    let upsert = scratch.file(
        "upsert2.csv",
        "name,email,full_name,loyalty_points\r\na@x.y,a@x.y,Ann B,\r\nz@x.y,z@x.y,Zed,9\r\n",
    );
    let out = import(&upsert, &["--format", "csv", "--upsert"]);
    assert_eq!(code(&out), Some(0), "{}", stderr(&out));
    let summary = json(&out);
    assert_eq!(
        (summary["updated"].as_u64(), summary["inserted"].as_u64()),
        (Some(1), Some(1))
    );
    assert_eq!(
        db.scalar("SELECT full_name || '/' || coalesce(loyalty_points::text, 'null') FROM doc_customer WHERE name = 'a@x.y'")
            .as_deref(),
        Some("Ann B/null")
    );

    // A JSON file; a file the format does not fit and a missing file are refused before the
    // database is touched.
    let json_file = scratch.file("docs.json", r#"[{"email":"j@x.y","full_name":"Jay"}]"#);
    assert_eq!(code(&import(&json_file, &["--format", "json"])), Some(0));
    assert_eq!(count(), "4");
    let out = import(&json_file, &["--format", "csv"]);
    assert_eq!(code(&out), Some(1));
    assert!(
        stderr(&out).contains("the file cannot be imported"),
        "{}",
        stderr(&out)
    );
    let out = import(&scratch.path("missing.csv"), &["--format", "csv"]);
    assert_eq!(code(&out), Some(1));
    assert!(stderr(&out).contains("cannot read"), "{}", stderr(&out));
    // --format is required.
    assert_eq!(code(&import(&good, &[])), Some(2));
    assert_eq!(count(), "4");
}

// ---- customize field (M10.1, ADR 0023) -------------------------------------------------------------

#[test]
fn customize_field_adds_lists_disables_and_enables() {
    let db = TestDb::new();
    let migrated = db.forgebase(&["migrate", "--doctypes", DOCTYPES]);
    assert_eq!(code(&migrated), Some(0), "{}", stderr(&migrated));
    let scratch = Scratch::new();
    let file = scratch.file(
        "vip.toml",
        "name = \"custom_vip\"\nlabel = \"VIP\"\ntype = \"check\"\n",
    );
    let add = |field: &str, stdin: Option<&str>| {
        db.forgebase_with_stdin(
            &[
                "customize",
                "field",
                "add",
                "--doctype",
                "customer",
                "--field",
                field,
                "--doctypes",
                DOCTYPES,
            ],
            stdin,
        )
    };

    // From a file: the column exists at once.
    let added = add(&file, None);
    assert_eq!(code(&added), Some(0), "{}", stderr(&added));
    assert!(
        stdout(&added).starts_with("added custom field customer.custom_vip (check)"),
        "{}",
        stdout(&added)
    );
    assert_eq!(
        db.scalar(
            "SELECT data_type FROM information_schema.columns \
             WHERE table_name = 'doc_customer' AND column_name = 'custom_vip'"
        )
        .as_deref(),
        Some("boolean")
    );
    assert_eq!(
        db.scalar("SELECT version::text FROM fb_meta_version")
            .as_deref(),
        Some("1")
    );

    // From standard input, with a type parameter and `unique`.
    let from_stdin = add(
        "-",
        Some("name = \"custom_rank\"\ntype = \"int\"\nunique = true\n"),
    );
    assert_eq!(code(&from_stdin), Some(0), "{}", stderr(&from_stdin));

    // `list`: a table with every field, then one DocType's.
    let list = db.forgebase(&["customize", "field", "list"]);
    assert_eq!(code(&list), Some(0), "{}", stderr(&list));
    let text = stdout(&list);
    let lines: Vec<&str> = text.lines().collect();
    assert_eq!(lines.len(), 3, "{text}");
    assert!(
        lines[0].starts_with("DOCTYPE") && lines[0].contains("ENABLED"),
        "{text}"
    );
    // In the order they were added.
    assert!(
        lines[1].contains("custom_vip")
            && lines[1].contains("VIP")
            && lines[1].contains("yes")
            && lines[1].trim_end().ends_with("cli"),
        "{text}"
    );
    assert!(
        lines[2].contains("custom_rank") && lines[2].contains("Custom Rank"),
        "{text}"
    );
    let none = db.forgebase(&["customize", "field", "list", "--doctype", "customer_group"]);
    assert_eq!(stdout(&none).lines().count(), 1, "{}", stdout(&none));

    // The commands that read documents show the field.
    let data = r#"{"email": "a@x.test", "full_name": "A"}"#;
    assert_eq!(
        code(&db.forgebase(&[
            "doc",
            "insert",
            "customer",
            "--data",
            data,
            "--doctypes",
            DOCTYPES
        ])),
        Some(0)
    );
    db.exec("UPDATE doc_customer SET custom_vip = true, custom_rank = 3");
    let get = db.forgebase(&["doc", "get", "customer", "a@x.test", "--doctypes", DOCTYPES]);
    assert_eq!(code(&get), Some(0), "{}", stderr(&get));
    let doc: Json = serde_json::from_str(&stdout(&get)).expect("JSON");
    assert_eq!(doc["custom_vip"], true, "{doc}");
    assert_eq!(doc["custom_rank"], 3, "{doc}");

    // Disable: hidden, column and data kept.
    let disabled = db.forgebase(&[
        "customize",
        "field",
        "disable",
        "--doctype",
        "customer",
        "custom_vip",
    ]);
    assert_eq!(code(&disabled), Some(0), "{}", stderr(&disabled));
    assert!(
        stdout(&disabled).contains("its column and data are kept"),
        "{}",
        stdout(&disabled)
    );
    let get = db.forgebase(&["doc", "get", "customer", "a@x.test", "--doctypes", DOCTYPES]);
    let doc: Json = serde_json::from_str(&stdout(&get)).expect("JSON");
    assert!(
        doc.get("custom_vip").is_none() && doc["custom_rank"] == 3,
        "{doc}"
    );
    assert_eq!(
        db.scalar("SELECT custom_vip::text FROM doc_customer")
            .as_deref(),
        Some("true")
    );
    let list = db.forgebase(&["customize", "field", "list", "--doctype", "customer"]);
    assert!(
        stdout(&list)
            .lines()
            .any(|l| l.contains("custom_vip") && l.split_whitespace().any(|w| w == "no")),
        "{}",
        stdout(&list)
    );

    // Enable: shown again with its data.
    let enabled = db.forgebase(&[
        "customize",
        "field",
        "enable",
        "--doctype",
        "customer",
        "custom_vip",
    ]);
    assert_eq!(code(&enabled), Some(0), "{}", stderr(&enabled));
    let get = db.forgebase(&["doc", "get", "customer", "a@x.test", "--doctypes", DOCTYPES]);
    let doc: Json = serde_json::from_str(&stdout(&get)).expect("JSON");
    assert_eq!(doc["custom_vip"], true, "{doc}");
    assert_eq!(
        db.scalar("SELECT version::text FROM fb_meta_version")
            .as_deref(),
        Some("4")
    );
}

#[test]
fn customize_field_refusals_change_nothing() {
    let db = TestDb::new();
    let migrated = db.forgebase(&["migrate", "--doctypes", DOCTYPES]);
    assert_eq!(code(&migrated), Some(0), "{}", stderr(&migrated));
    let add = |doctype: &str, text: &str| {
        db.forgebase_with_stdin(
            &[
                "customize",
                "field",
                "add",
                "--doctype",
                doctype,
                "--field",
                "-",
                "--doctypes",
                DOCTYPES,
            ],
            Some(text),
        )
    };
    for (doctype, text, expected) in [
        (
            "customer",
            "name = \"vip\"\ntype = \"check\"\n",
            "must start with `custom_`",
        ),
        (
            "customer",
            "name = \"custom_a\"\ntype = \"check\"\nrequired = true\n",
            "cannot be required",
        ),
        (
            "customer",
            "name = \"custom_a\"\ntype = \"check\"\nread_only = true\n",
            "cannot be read_only",
        ),
        (
            "customer",
            "name = \"custom_a\"\ntype = \"table\"\ntarget = \"sales_order_item\"\n",
            "cannot add Table fields",
        ),
        (
            "customer",
            "name = \"custom_a\"\ntype = \"check\"\ncolour = 1\n",
            "cannot parse the definition",
        ),
        ("customer", "not toml at all", "cannot parse the definition"),
        (
            "sales_order_item",
            "name = \"custom_a\"\ntype = \"check\"\n",
            "child DocType",
        ),
        (
            "ghost",
            "name = \"custom_a\"\ntype = \"check\"\n",
            "unknown doctype `ghost`",
        ),
        (
            "customer",
            "name = \"email\"\ntype = \"check\"\n",
            "must start with `custom_`",
        ),
    ] {
        let out = add(doctype, text);
        assert_eq!(code(&out), Some(1), "{text}: {}", stderr(&out));
        assert!(stderr(&out).contains(expected), "{text}: {}", stderr(&out));
    }
    // A file that is not there is refused before the database is used.
    let out = db.forgebase(&[
        "customize",
        "field",
        "add",
        "--doctype",
        "customer",
        "--field",
        "/no/such/file.toml",
        "--doctypes",
        DOCTYPES,
    ]);
    assert_eq!(code(&out), Some(1));
    assert!(stderr(&out).contains("cannot read"), "{}", stderr(&out));
    // A duplicate, and a field that is not there.
    assert_eq!(
        code(&add("customer", "name = \"custom_a\"\ntype = \"check\"\n")),
        Some(0)
    );
    let again = add("customer", "name = \"custom_a\"\ntype = \"int\"\n");
    assert_eq!(code(&again), Some(1));
    assert!(
        stderr(&again).contains("already exists"),
        "{}",
        stderr(&again)
    );
    let missing = db.forgebase(&[
        "customize",
        "field",
        "disable",
        "--doctype",
        "customer",
        "custom_nope",
    ]);
    assert_eq!(code(&missing), Some(1));
    assert!(
        stderr(&missing).contains("not found"),
        "{}",
        stderr(&missing)
    );
    // Only the one good field was written.
    assert_eq!(
        db.scalar("SELECT count(*)::text FROM fb_custom_field")
            .as_deref(),
        Some("1")
    );
    assert_eq!(
        db.scalar("SELECT version::text FROM fb_meta_version")
            .as_deref(),
        Some("1")
    );
}

#[test]
fn the_data_and_doc_commands_see_enabled_custom_fields_and_refuse_disabled_ones() {
    let db = TestDb::new();
    let migrated = db.forgebase(&["migrate", "--doctypes", DOCTYPES]);
    assert_eq!(code(&migrated), Some(0), "{}", stderr(&migrated));
    let added = db.forgebase_with_stdin(
        &[
            "customize",
            "field",
            "add",
            "--doctype",
            "customer",
            "--field",
            "-",
            "--doctypes",
            DOCTYPES,
        ],
        Some("name = \"custom_vip\"\ntype = \"check\"\n"),
    );
    assert_eq!(code(&added), Some(0), "{}", stderr(&added));
    let scratch = Scratch::new();
    let run = |args: &[&str]| {
        let mut all = args.to_vec();
        all.extend(["--doctypes", DOCTYPES]);
        db.forgebase(&all)
    };

    // doc insert and update take the field, and doc get shows it.
    let out = run(&[
        "doc",
        "insert",
        "customer",
        "--data",
        r#"{"email":"a@x.test","full_name":"A","custom_vip":true}"#,
    ]);
    assert_eq!(code(&out), Some(0), "{}", stderr(&out));
    let doc: Json = serde_json::from_str(&stdout(&out)).expect("JSON");
    assert_eq!(doc["custom_vip"], true);
    let version = doc["version"].to_string();
    let out = run(&[
        "doc",
        "update",
        "customer",
        "a@x.test",
        "--version",
        &version,
        "--data",
        r#"{"custom_vip":false}"#,
    ]);
    assert_eq!(code(&out), Some(0), "{}", stderr(&out));
    let out = run(&[
        "doc",
        "list",
        "customer",
        "--filter",
        r#"[{"field":"custom_vip","op":"=","value":false}]"#,
    ]);
    assert_eq!(code(&out), Some(0), "{}", stderr(&out));
    assert_eq!(
        serde_json::from_str::<Json>(&stdout(&out))
            .expect("JSON")
            .as_array()
            .map(Vec::len),
        Some(1)
    );

    // A CSV export has the column, --fields and --filter work on it, and an import sets it.
    let out = run(&["data", "export", "customer", "--format", "csv"]);
    assert_eq!(code(&out), Some(0), "{}", stderr(&out));
    assert!(
        stdout(&out)
            .lines()
            .next()
            .expect("header")
            .contains("custom_vip"),
        "{}",
        stdout(&out)
    );
    let out = run(&[
        "data",
        "export",
        "customer",
        "--format",
        "csv",
        "--fields",
        "name,custom_vip",
        "--filter",
        r#"[{"field":"custom_vip","op":"=","value":false}]"#,
    ]);
    assert_eq!(code(&out), Some(0), "{}", stderr(&out));
    assert_eq!(
        stdout(&out).replace("\r\n", "\n"),
        "name,custom_vip\na@x.test,false\n"
    );
    let csv = scratch.file("in.csv", "email,full_name,custom_vip\nb@x.test,B,true\n");
    let out = run(&["data", "import", "customer", &csv, "--format", "csv"]);
    assert_eq!(code(&out), Some(0), "{}", stderr(&out));
    assert_eq!(
        db.scalar("SELECT custom_vip::text FROM doc_customer WHERE name = 'b@x.test'")
            .as_deref(),
        Some("true")
    );

    // Disabled: unknown in input, import, --fields and --filter; the column and data stay.
    let off = db.forgebase(&[
        "customize",
        "field",
        "disable",
        "--doctype",
        "customer",
        "custom_vip",
    ]);
    assert_eq!(code(&off), Some(0), "{}", stderr(&off));
    for args in [
        vec![
            "doc",
            "insert",
            "customer",
            "--data",
            r#"{"email":"c@x.test","full_name":"C","custom_vip":true}"#,
        ],
        vec![
            "doc",
            "list",
            "customer",
            "--filter",
            r#"[{"field":"custom_vip","op":"=","value":true}]"#,
        ],
        vec!["data", "export", "customer", "--fields", "custom_vip"],
        vec![
            "data",
            "export",
            "customer",
            "--filter",
            r#"[{"field":"custom_vip","op":"=","value":true}]"#,
        ],
        vec![
            "data", "import", "customer", &csv, "--format", "csv", "--upsert",
        ],
    ] {
        let out = run(&args);
        assert_eq!(code(&out), Some(1), "{args:?}: {}", stdout(&out));
        assert!(
            stderr(&out).contains("custom_vip"),
            "{args:?}: {}",
            stderr(&out)
        );
    }
    let out = run(&["data", "export", "customer", "--format", "csv"]);
    assert!(!stdout(&out).contains("custom_vip"), "{}", stdout(&out));
    assert_eq!(
        db.scalar("SELECT count(*)::text FROM doc_customer WHERE custom_vip")
            .as_deref(),
        Some("1")
    );
}
