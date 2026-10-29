//! A binary with apps, end to end (M5.1): `Forgebase::new().app(..).run()` with real command-line
//! arguments against a real PostgreSQL.
//!
//! This file has its own `main` (`harness = false` in `Cargo.toml`). Started normally it is the
//! test driver: it creates a throwaway database and runs *itself* again as the application
//! binary (selected by the environment variable `FORGEBASE_TEST_BINARY`), once per command.

use std::io::Write;
use std::process::{Command, ExitCode, Output, Stdio};
use std::sync::Arc;

use forgebase::testing::TestDb;
use forgebase::{
    App, DocTypeFile, DocTypeHooks, Document, Forgebase, HookContext, HookError, MigrationFile,
    Value,
};

const BINARY_VAR: &str = "FORGEBASE_TEST_BINARY";

const CUSTOMER: &str = "name = \"customer\"\nlabel = \"Customer\"\nnaming = { field = \"code\" }\n\
    [[fields]]\nname = \"code\"\ntype = \"data\"\nrequired = true\nunique = true\n\
    [[fields]]\nname = \"title\"\ntype = \"data\"\n";

const SEED: MigrationFile = MigrationFile {
    version: 1,
    name: "seed",
    path: "migrations/0001_seed.sql",
    sql: "CREATE TABLE crm_seed (entry text); INSERT INTO crm_seed VALUES ('seeded')",
};

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

fn crm() -> App {
    App::new("crm")
        .version("1.2.3")
        .doctypes(&[DocTypeFile {
            path: "doctypes/customer.toml",
            toml: CUSTOMER,
        }])
        .migrations(&[SEED])
}

fn sales() -> App {
    App::new("sales")
        .depends_on(&["crm"])
        .hooks("customer", Arc::new(NoBanned))
}

fn main() -> ExitCode {
    match std::env::var(BINARY_VAR).as_deref() {
        Ok("good") => Forgebase::new().app(crm()).app(sales()).run(),
        // `sales` before `crm`: a startup error.
        Ok("broken") => Forgebase::new().app(sales()).app(crm()).run(),
        _ => {
            drive();
            println!("test app_binary ... ok");
            ExitCode::SUCCESS
        }
    }
}

struct Run<'a> {
    mode: &'a str,
    url: &'a str,
}

impl Run<'_> {
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
    assert!(out.contains("framework migrations: 4 pending"), "{out}");
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
}
