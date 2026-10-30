//! A binary with apps, end to end (M5.1): `Forgebase::new().app(..).run()` with real command-line
//! arguments against a real PostgreSQL.
//!
//! This file has its own `main` (`harness = false` in `Cargo.toml`). Started normally it is the
//! test driver: it creates a throwaway database and runs *itself* again as the application
//! binary (selected by the environment variable `FORGEBASE_TEST_BINARY`), once per command.

use std::io::Write;
use std::process::{Command, ExitCode, Output, Stdio};

use forgebase::testing::TestDb;
use forgebase::{
    App, DocTypeFile, DocTypeHooks, Document, Forgebase, HookContext, HookError, JobContext,
    JobError, JobFuture, JobHandler, MigrationFile, Param, Report, Value,
};

const BINARY_VAR: &str = "FORGEBASE_TEST_BINARY";

const CUSTOMER: &str = "name = \"customer\"\nlabel = \"Customer\"\nnaming = { field = \"code\" }\n\
    [[fields]]\nname = \"code\"\ntype = \"data\"\nrequired = true\nunique = true\n\
    [[fields]]\nname = \"title\"\ntype = \"data\"\n";

const SEED: MigrationFile = MigrationFile::new(
    1,
    "seed",
    "migrations/0001_seed.sql",
    "CREATE TABLE crm_seed (entry text); INSERT INTO crm_seed VALUES ('seeded')",
);

/// Rejects a customer titled `banned`.
struct NoBanned;

impl DocTypeHooks for NoBanned {
    fn validate(&self, _ctx: &HookContext<'_>, doc: &mut Document) -> Result<(), HookError> {
        if doc.values.get("title") == Some(&Value::Text("banned".to_owned())) {
            return Err(HookError::new("a banned customer is not allowed"));
        }
        Ok(())
    }
}

/// `crm.touch` inserts a customer with the `code` of its payload; `crm.fail` never works.
struct Touch;

impl JobHandler for Touch {
    fn run<'a>(&'a self, ctx: &'a mut JobContext<'_>) -> JobFuture<'a> {
        Box::pin(async move {
            let code = ctx.payload()["code"].as_str().unwrap_or("none").to_owned();
            let customer = Document {
                values: [("code".to_owned(), Value::Text(code))].into(),
                ..Document::default()
            };
            ctx.documents().insert("customer", customer).await?;
            Ok(())
        })
    }
}

struct Fail;

impl JobHandler for Fail {
    fn run<'a>(&'a self, _: &'a mut JobContext<'_>) -> JobFuture<'a> {
        Box::pin(async { Err(JobError::permanent("this job never works")) })
    }
}

fn crm() -> App {
    App::new("crm")
        .job("touch", Touch)
        .job("fail", Fail)
        .version("1.2.3")
        .doctypes(const { &[DocTypeFile::new("doctypes/customer.toml", CUSTOMER)] })
        .migrations(&[SEED])
}

/// `crm` with a report on its customers. The bad one declares one column where the SQL returns
/// two.
fn crm_with_report(valid: bool) -> App {
    let mut report = Report::sql(
        "SELECT name, title FROM doc_customer WHERE ($1::text IS NULL OR name = $1);\n",
    )
    .roles(&["Manager"])
    .param(Param::link("customer", "customer"))
    .column(forgebase::Column::link("customer", "customer"));
    if valid {
        report = report.column(forgebase::Column::data("title"));
    }
    crm().report("totals", report)
}

fn sales() -> App {
    App::new("sales")
        .depends_on(&["crm"])
        .hooks("customer", NoBanned)
}

fn main() -> ExitCode {
    match std::env::var(BINARY_VAR).as_deref() {
        Ok("good") => Forgebase::new().app(crm()).app(sales()).run(),
        // `sales` before `crm`: a startup error.
        Ok("reports_ok") => Forgebase::new().app(crm_with_report(true)).run(),
        Ok("reports_bad") => Forgebase::new().app(crm_with_report(false)).run(),
        Ok("broken") => Forgebase::new().app(sales()).app(crm()).run(),
        _ => {
            drive();
            println!("test app_binary ... ok");
            ExitCode::SUCCESS
        }
    }
}

/// Reads a child's standard output to its end in the background, so a line it prints can never
/// block it or meet a closed pipe.
fn drain<R: std::io::Read + Send + 'static>(mut pipe: R) {
    std::thread::spawn(move || {
        let _ = std::io::copy(&mut pipe, &mut std::io::sink());
    });
}

struct Run<'a> {
    mode: &'a str,
    url: &'a str,
}

impl Run<'_> {
    /// Starts the binary and leaves it running (the `worker`).
    fn spawn(&self, args: &[&str]) -> std::process::Child {
        Command::new(std::env::current_exe().expect("current exe"))
            .args(args)
            .env(BINARY_VAR, self.mode)
            .env("FORGEBASE_DATABASE_URL", self.url)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .expect("start the app binary")
    }

    fn go(&self, args: &[&str], stdin: Option<&str>) -> Output {
        let mut child = Command::new(std::env::current_exe().expect("current exe"))
            .args(args)
            .env(BINARY_VAR, self.mode)
            .env("FORGEBASE_DATABASE_URL", self.url)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .expect("start the app binary");
        {
            let mut pipe = child.stdin.take().expect("stdin");
            if let Some(text) = stdin {
                pipe.write_all(text.as_bytes()).expect("write stdin");
            }
        }
        child.wait_with_output().expect("run")
    }
}

fn text(bytes: &[u8]) -> String {
    String::from_utf8_lossy(bytes).into_owned()
}

#[track_caller]
fn succeeds(output: &Output) -> String {
    assert_eq!(
        output.status.code(),
        Some(0),
        "stderr: {}",
        text(&output.stderr)
    );
    text(&output.stdout)
}

fn drive() {
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("runtime");
    let db = runtime.block_on(TestDb::new());
    let good = Run {
        mode: "good",
        url: db.url(),
    };

    // A dry run on an empty database lists the app migration, and keeps nothing.
    let out = succeeds(&good.go(&["migrate", "--dry-run"], None));
    assert!(out.contains("framework migrations: 9 pending"), "{out}");
    assert!(
        out.contains("app migrations: 1 pending\n  crm 0001 seed\n"),
        "{out}"
    );
    assert!(!out.contains("applied"), "{out}");

    // `migrate` needs no --doctypes: the apps bring their DocTypes.
    let out = succeeds(&good.go(&["migrate"], None));
    assert!(out.contains("schema sync: applied"), "{out}");
    assert!(
        out.contains("app migrations: 1 applied\n  crm 0001 seed\n"),
        "{out}"
    );
    let rows = runtime.block_on(db.rows("SELECT name, version FROM fb_app ORDER BY name"));
    assert_eq!(
        rows,
        vec![
            vec!["crm".to_owned(), "1.2.3".into()],
            vec!["sales".to_owned(), "NULL".into()]
        ]
    );
    let out = succeeds(&good.go(&["migrate"], None));
    assert!(out.contains("app migrations: up to date\n"), "{out}");

    // Documents: the hook of `sales` (registered on crm's DocType) runs for `doc insert`.
    let rejected = good.go(
        &[
            "doc",
            "insert",
            "customer",
            "--data",
            r#"{"code":"c-1","title":"banned"}"#,
        ],
        None,
    );
    assert_eq!(rejected.status.code(), Some(1));
    let message = text(&rejected.stderr);
    assert!(
        message.starts_with("error: ") && message.contains("a banned customer is not allowed"),
        "{message}"
    );
    let out = succeeds(&good.go(
        &[
            "doc",
            "insert",
            "customer",
            "--data",
            r#"{"code":"c-2","title":"fine"}"#,
        ],
        None,
    ));
    assert!(out.contains("\"name\": \"c-2\""), "{out}");
    let out = succeeds(&good.go(&["doc", "get", "customer", "c-2"], None));
    assert!(out.contains("\"title\": \"fine\""), "{out}");

    // `--doctypes DIR` is one more app, `local`, that may link to every app's DocTypes.
    let dir = std::env::temp_dir().join(format!("forgebase-app-binary-{}", std::process::id()));
    std::fs::create_dir_all(&dir).expect("dir");
    std::fs::write(
        dir.join("note.toml"),
        "name = \"note\"\nlabel = \"Note\"\n[[fields]]\nname = \"customer\"\ntype = \"link\"\ntarget = \"customer\"\n",
    )
    .expect("write");
    let dir_arg = dir.to_str().expect("utf-8 path");
    let out = succeeds(&good.go(&["migrate", "--doctypes", dir_arg], None));
    assert!(out.contains("schema sync: applied"), "{out}");
    let out = succeeds(&good.go(
        &[
            "doc",
            "insert",
            "note",
            "--doctypes",
            dir_arg,
            "--data",
            r#"{"customer":"c-2"}"#,
        ],
        None,
    ));
    assert!(out.contains("\"customer\": \"c-2\""), "{out}");
    // A command that does not read DocTypes works too; an invalid file is a startup error.
    succeeds(&good.go(&["user", "create", "ann"], Some("a long password\n")));
    std::fs::write(dir.join("broken.toml"), "name = ").expect("write");
    let out = good.go(&["migrate", "--doctypes", dir_arg], None);
    std::fs::remove_dir_all(&dir).expect("cleanup");
    assert_eq!(out.status.code(), Some(1));
    assert!(
        text(&out.stderr).starts_with("error: cannot load DocType files:\n  - ")
            && text(&out.stderr).contains("broken.toml"),
        "{}",
        text(&out.stderr)
    );

    jobs(&good, &runtime, &db);

    // Apps in the wrong order: exit code 1 and the problem, for every command, before any
    // database work (the URL here does not even point to a server).
    let broken = Run {
        mode: "broken",
        url: "postgres://unused:unused@127.0.0.1:1/unused",
    };
    for args in [&["migrate"][..], &["user", "create", "ann"], &["serve"]] {
        let out = broken.go(args, Some("a long password\n"));
        assert_eq!(out.status.code(), Some(1), "{args:?}");
        assert!(
            text(&out.stderr).starts_with(
                "error: invalid app setup:\n  - app `sales` depends on `crm`, which is registered after it;"
            ),
            "{args:?}: {}",
            text(&out.stderr)
        );
        assert!(out.stdout.is_empty());
    }

    runtime.block_on(db.cleanup());
    reports(&runtime);
}

/// The check of the reports' SQL at `migrate` (M8.4): after the migrations, never in a dry run,
/// and a mismatch is exit code 1 with the migrations kept.
fn reports(runtime: &tokio::runtime::Runtime) {
    let db = runtime.block_on(TestDb::new());
    let ok = Run {
        mode: "reports_ok",
        url: db.url(),
    };
    // A dry run keeps nothing, so the tables the SQL reads may not exist: it checks nothing.
    let out = succeeds(&ok.go(&["migrate", "--dry-run"], None));
    assert!(!out.contains("reports:"), "{out}");
    let out = succeeds(&ok.go(&["migrate"], None));
    assert!(out.ends_with("reports: 1 checked\n"), "{out}");
    runtime.block_on(db.cleanup());

    let db = runtime.block_on(TestDb::new());
    let bad = Run {
        mode: "reports_bad",
        url: db.url(),
    };
    let out = bad.go(&["migrate"], None);
    assert_eq!(out.status.code(), Some(1));
    // What was applied is reported, and it stays applied.
    let stdout = text(&out.stdout);
    assert!(
        stdout.contains("framework migrations: 9 applied")
            && stdout.contains("app migrations: 1 applied\n  crm 0001 seed\n"),
        "{stdout}"
    );
    let stderr = text(&out.stderr);
    assert!(
        stderr.starts_with(
            "error: the migrations were applied, but 1 report(s) do not fit their SQL:\n"
        ) && stderr.contains("report `crm.totals`: the SQL returns 2 column(s) but 1 are declared")
            && stderr.contains("fix the report and run migrate again"),
        "{stderr}"
    );
    assert!(
        !stderr.contains("doc_customer"),
        "the SQL is not printed: {stderr}"
    );
    let rows = runtime.block_on(db.rows("SELECT name FROM fb_app"));
    assert_eq!(rows, vec![vec!["crm".to_owned()]]);
    let again = bad.go(&["migrate"], None);
    assert_eq!(again.status.code(), Some(1));
    assert!(
        text(&again.stdout).contains("app migrations: up to date"),
        "{}",
        text(&again.stdout)
    );
    runtime.block_on(db.cleanup());
}

/// The `job` commands and the `worker`, with the jobs of `crm`.
fn jobs(good: &Run<'_>, runtime: &tokio::runtime::Runtime, db: &TestDb) {
    // An unknown job is refused; the payload must be JSON.
    let out = good.go(&["job", "enqueue", "crm.nobody"], None);
    assert_eq!(out.status.code(), Some(1));
    assert!(
        text(&out.stderr).starts_with("error: unknown job `crm.nobody`"),
        "{}",
        text(&out.stderr)
    );
    let out = good.go(&["job", "enqueue", "crm.touch", "--payload", "{"], None);
    assert_eq!(out.status.code(), Some(1));
    assert!(
        text(&out.stderr).contains("not valid JSON"),
        "{}",
        text(&out.stderr)
    );
    assert_eq!(count(runtime, db, "SELECT count(*)::text FROM fb_job"), "0");

    // The payload as an argument, from standard input and the default; `--actor` is recorded.
    let first = enqueued(&succeed(
        good,
        &[
            "job",
            "enqueue",
            "crm.touch",
            "--payload",
            r#"{"code":"j-1"}"#,
        ],
        None,
    ));
    let second = enqueued(&succeed(
        good,
        &[
            "job",
            "enqueue",
            "crm.touch",
            "--payload",
            "-",
            "--actor",
            "ann",
        ],
        Some("{\"code\":\"j-2\"}\n"),
    ));
    let dying = enqueued(&succeed(good, &["job", "enqueue", "crm.fail"], None));
    let by = runtime.block_on(db.rows("SELECT enqueued_by FROM fb_job ORDER BY id"));
    assert_eq!(by, [["cli"], ["ann"], ["cli"]]);

    // List: a table, newest first, with filters.
    let out = succeed(good, &["job", "list"], None);
    let lines: Vec<&str> = out.lines().collect();
    assert_eq!(lines.len(), 4, "{out}");
    assert!(
        lines[0].starts_with("ID") && lines[0].contains("LAST_ERROR"),
        "{out}"
    );
    assert!(
        lines[1].contains(&dying) && lines[1].contains("crm.fail") && lines[1].contains("queued"),
        "{out}"
    );
    let out = succeed(
        good,
        &["job", "list", "--name", "crm.touch", "--limit", "1"],
        None,
    );
    assert_eq!(out.lines().count(), 2, "{out}");
    let out = succeed(good, &["job", "list", "--status", "dead"], None);
    assert_eq!(out.lines().count(), 1, "{out}");
    assert_eq!(
        good.go(&["job", "list", "--status", "bogus"], None)
            .status
            .code(),
        Some(2)
    );
    assert_eq!(
        good.go(&["job", "list", "--limit", "0"], None)
            .status
            .code(),
        Some(2)
    );

    // Show: every field as JSON, the payload included.
    let shown: serde_json::Value =
        serde_json::from_str(&succeed(good, &["job", "show", &first], None)).expect("json");
    assert_eq!(shown["id"], first.as_str());
    assert_eq!(shown["payload"]["code"], "j-1");
    assert_eq!(shown["status"], "queued");
    assert!(shown["finished_at"].is_null());
    let out = good.go(
        &["job", "show", "00000000-0000-7000-8000-000000000000"],
        None,
    );
    assert_eq!(out.status.code(), Some(1));
    assert_eq!(
        good.go(&["job", "show", "nonsense"], None).status.code(),
        Some(1)
    );

    // Only a dead job is retried.
    let out = good.go(&["job", "retry", &first], None);
    assert_eq!(out.status.code(), Some(1));
    assert!(
        text(&out.stderr).contains("only a dead job can be retried"),
        "{}",
        text(&out.stderr)
    );

    // The worker runs them, and stops cleanly on SIGTERM. The schedules are removed first: a
    // built-in clean-up job that falls due while this test runs (at the turn of the hour) would
    // add a job to the counts below.
    runtime.block_on(db.exec("DELETE FROM fb_schedule"));
    let mut worker = good.spawn(&["worker", "--concurrency", "2"]);
    drain(worker.stdout.take().expect("stdout"));
    let started = std::time::Instant::now();
    loop {
        let done = count(
            runtime,
            db,
            "SELECT count(*)::text FROM fb_job WHERE status IN ('done', 'dead')",
        );
        if done == "3" {
            break;
        }
        assert!(
            started.elapsed() < std::time::Duration::from_secs(30),
            "the worker did not finish: {done}"
        );
        std::thread::sleep(std::time::Duration::from_millis(100));
    }
    let killed = Command::new("kill")
        .args(["-TERM", &worker.id().to_string()])
        .status()
        .expect("kill");
    assert!(killed.success());
    let out = worker.wait_with_output().expect("worker exit");
    assert_eq!(out.status.code(), Some(0), "stderr: {}", text(&out.stderr));
    let log = text(&out.stderr);
    assert!(
        log.contains("started ") && log.contains("concurrency 2") && log.contains("stopped"),
        "{log}"
    );
    assert!(
        log.contains(&format!("job crm.touch {first} done")),
        "{log}"
    );
    assert!(
        log.contains("dead after attempt 1: this job never works"),
        "{log}"
    );
    assert_eq!(
        runtime
            .block_on(db.rows("SELECT code FROM doc_customer WHERE code LIKE 'j-%' ORDER BY code")),
        [["j-1"], ["j-2"]]
    );
    let out = succeed(good, &["job", "list", "--status", "dead"], None);
    assert!(
        out.contains(&dying) && out.contains("this job never works"),
        "{out}"
    );
    assert!(succeed(good, &["job", "show", &second], None).contains("\"status\": \"done\""));

    // Retry: dead to queued.
    succeed(good, &["job", "retry", &dying], None);
    let shown: serde_json::Value =
        serde_json::from_str(&succeed(good, &["job", "show", &dying], None)).expect("json");
    assert_eq!(
        (shown["status"].as_str(), shown["attempts"].as_i64()),
        (Some("queued"), Some(0))
    );
    assert!(shown["last_error"].is_null() && shown["finished_at"].is_null());
    // And a job of a done status is refused.
    assert_eq!(
        good.go(&["job", "retry", &second], None).status.code(),
        Some(1)
    );
}

fn succeed(run: &Run<'_>, args: &[&str], stdin: Option<&str>) -> String {
    succeeds(&run.go(args, stdin))
}

/// The id in `enqueued job <id>`.
fn enqueued(output: &str) -> String {
    output
        .trim()
        .strip_prefix("enqueued job ")
        .unwrap_or_else(|| panic!("unexpected output: {output}"))
        .to_owned()
}

fn count(runtime: &tokio::runtime::Runtime, db: &TestDb, sql: &str) -> String {
    runtime.block_on(db.rows(sql)).remove(0).remove(0)
}
