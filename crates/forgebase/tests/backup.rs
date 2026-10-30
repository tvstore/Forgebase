//! `site backup` and `site restore` (M7.3, ADR 0014): the built `forgebase` binary against real
//! PostgreSQL databases and the real `pg_dump` and `pg_restore` of the machine.
//!
//! The PostgreSQL client tools (version 16 or newer, on `PATH`) are required: a test that cannot
//! find them fails with the tool's name (it never skips). Every test makes its own throwaway
//! `fb_test_backup_*` databases and needs `FORGEBASE_TEST_DATABASE_URL`; see CONTRIBUTING.md.

use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};
use std::sync::atomic::{AtomicU32, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

use forgebase_db::{connect, quote_ident};

const ENV_VAR: &str = "FORGEBASE_TEST_DATABASE_URL";
const DOCTYPES: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/tests/fixtures/quickstart");
const SRC_URL: &str = "FB_TEST_BACKUP_SRC_URL";
const DST_URL: &str = "FB_TEST_BACKUP_DST_URL";
const PASSWORD: &str = "a password for ann";

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

fn run_sql(url: &str, sql: String) {
    let url = url.to_owned();
    block_on(async move {
        let pool = connect(&url).await.expect("connect");
        let client = pool.get().await.expect("connection");
        client.batch_execute(&sql).await.expect("execute");
        pool.close();
    });
}

/// A database name no test run uses twice.
fn unique_name() -> String {
    static COUNTER: AtomicU32 = AtomicU32::new(0);
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.subsec_nanos());
    format!(
        "fb_test_backup_{:x}_{:x}_{}",
        std::process::id(),
        nanos,
        COUNTER.fetch_add(1, Ordering::Relaxed)
    )
}

/// The URL of a database that may or may not exist yet; it is dropped with the value.
struct Db {
    admin_url: String,
    name: String,
    url: String,
}

impl Db {
    /// Only the name: the database is made by `site create` or by `created`.
    fn named() -> Self {
        let admin_url = admin_url();
        let name = unique_name();
        Self {
            url: with_database(&admin_url, &name),
            admin_url,
            name,
        }
    }

    /// A database that exists and is empty.
    fn created() -> Self {
        let db = Self::named();
        run_sql(
            &db.admin_url,
            format!("CREATE DATABASE {}", quote_ident(&db.name)),
        );
        db
    }

    fn scalar(&self, sql: &str) -> Option<String> {
        let (url, sql) = (self.url.clone(), sql.to_owned());
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

impl Drop for Db {
    fn drop(&mut self) {
        let (url, sql) = (
            self.admin_url.clone(),
            format!(
                "DROP DATABASE IF EXISTS {} WITH (FORCE)",
                quote_ident(&self.name)
            ),
        );
        let _ = std::thread::spawn(move || {
            let _ = std::panic::catch_unwind(|| run_sql(&url, sql));
        })
        .join();
    }
}

/// Two sites, `src` and `dst`, a config file and a directory for the backup files.
struct World {
    src: Db,
    dst: Db,
    dir: PathBuf,
}

impl World {
    /// `dst` does not exist yet.
    fn new() -> Self {
        static COUNTER: AtomicU32 = AtomicU32::new(0);
        let dir = std::env::temp_dir().join(format!(
            "fb_backup_{}_{}",
            std::process::id(),
            COUNTER.fetch_add(1, Ordering::Relaxed)
        ));
        let (src, dst) = (Db::created(), Db::named());
        std::fs::create_dir_all(&dir).expect("temporary directory");
        std::fs::write(
            dir.join("forgebase.toml"),
            format!(
                "[[sites]]\nname = \"src\"\nhosts = [\"src.localhost\"]\ndatabase_url_env = \"{SRC_URL}\"\n\n\
                 [[sites]]\nname = \"dst\"\nhosts = [\"dst.localhost\"]\ndatabase_url_env = \"{DST_URL}\"\n"
            ),
        )
        .expect("write the config");
        Self { src, dst, dir }
    }

    fn file(&self, name: &str) -> PathBuf {
        self.dir.join(name)
    }

    fn command(&self, args: &[&str]) -> Command {
        let mut command = Command::new(env!("CARGO_BIN_EXE_forgebase"));
        command
            .arg("--config")
            .arg(self.dir.join("forgebase.toml"))
            .args(args)
            .env_remove("FORGEBASE_DATABASE_URL")
            .env_remove("FORGEBASE_CONFIG")
            .env_remove("FORGEBASE_PG_BIN")
            .env(SRC_URL, &self.src.url)
            .env(DST_URL, &self.dst.url)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        command
    }

    fn run(&self, args: &[&str]) -> Output {
        self.command(args).output().expect("run forgebase")
    }

    fn run_with_bin(&self, args: &[&str], bin: &Path) -> Output {
        self.command(args)
            .env("FORGEBASE_PG_BIN", bin)
            .output()
            .expect("run forgebase")
    }

    fn ok(&self, args: &[&str]) -> String {
        let output = self.run(args);
        assert_eq!(code(&output), Some(0), "{args:?}: {}", stderr(&output));
        stdout(&output)
    }

    /// Migrates `src`, gives it a user and a few documents.
    fn fill_src(&self) {
        self.ok(&["migrate", "--site", "src", "--doctypes", DOCTYPES]);
        let mut command = self.command(&[
            "user",
            "create",
            "ann",
            "--role",
            "Sales User",
            "--site",
            "src",
        ]);
        command.stdin(Stdio::piped());
        let mut child = command.spawn().expect("start");
        child
            .stdin
            .take()
            .expect("stdin")
            .write_all(format!("{PASSWORD}\n").as_bytes())
            .expect("write");
        let out = child.wait_with_output().expect("run");
        assert_eq!(code(&out), Some(0), "{}", stderr(&out));
        for data in [
            r#"{"email":"a@example.com","full_name":"Ann","credit_limit":"1234.50","tier":"gold","joined_on":"2024-03-15"}"#,
            r#"{"email":"b@example.com","full_name":"Bob","notes":"café ☕","loyalty_points":7}"#,
        ] {
            self.ok(&[
                "doc",
                "insert",
                "customer",
                "--site",
                "src",
                "--doctypes",
                DOCTYPES,
                "--data",
                data,
            ]);
        }
        self.ok(&[
            "doc",
            "insert",
            "sales_order",
            "--site",
            "src",
            "--doctypes",
            DOCTYPES,
            "--data",
            r#"{"customer":"a@example.com","order_date":"2024-04-01","items":[{"description":"Pen","qty":3,"rate":"1.25"},{"description":"Ink","qty":1,"rate":"9.00"}]}"#,
        ]);
    }

    /// Every document of the DocTypes, as `doc list` and `doc get` print them, for `site`.
    fn documents(&self, site: &str) -> String {
        let mut all = String::new();
        for doctype in ["customer", "sales_order"] {
            let list = self.ok(&[
                "doc",
                "list",
                doctype,
                "--site",
                site,
                "--doctypes",
                DOCTYPES,
                "--limit",
                "500",
            ]);
            let docs: serde_json::Value = serde_json::from_str(&list).expect("json");
            for doc in docs.as_array().expect("array") {
                let name = doc["name"].as_str().expect("name");
                all.push_str(&self.ok(&[
                    "doc",
                    "get",
                    doctype,
                    name,
                    "--site",
                    site,
                    "--doctypes",
                    DOCTYPES,
                ]));
            }
            all.push_str(&list);
        }
        all
    }
}

impl Drop for World {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.dir);
    }
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

/// The command failed with exit code 1; returns its error text.
#[track_caller]
fn fails(output: &Output) -> String {
    assert_eq!(code(output), Some(1), "stdout: {}", stdout(output));
    stderr(output)
}

#[test]
fn a_backup_restored_into_an_empty_database_gives_the_same_documents() {
    let world = World::new();
    world.fill_src();
    let before = world.documents("src");
    assert!(before.contains("a@example.com") && before.contains("Pen"));

    let file = world.file("src.dump");
    let file_arg = file.to_str().expect("utf-8 path");
    let out = world.ok(&["site", "backup", "src", "--out", file_arg]);
    assert!(
        out.starts_with(&format!("backed up site `src` to {file_arg} ("))
            && out.ends_with(" bytes)\n"),
        "{out}"
    );
    assert!(!out.contains("forgebase:forgebase"));
    assert!(std::fs::metadata(&file).expect("file").len() > 0);
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mode = std::fs::metadata(&file).expect("meta").permissions().mode();
        assert_eq!(mode & 0o777, 0o600, "a backup holds password hashes");
    }

    // The new, empty database of the restore (made without migrating).
    let out = world.ok(&["site", "create", "dst", "--empty"]);
    assert!(
        out.starts_with("created the database of site `dst`\n") && out.contains("site restore dst"),
        "{out}"
    );
    assert_eq!(
        world.dst.scalar("SELECT to_regclass('fb_migration')::text"),
        None,
        "--empty does not migrate"
    );
    let output = world.run(&["site", "restore", "dst", "--from", file_arg]);
    assert_eq!(code(&output), Some(0), "{}", stderr(&output));
    let out = stdout(&output);
    assert!(
        out.contains("run `forgebase migrate --site dst` if this binary is newer than the backup"),
        "{out}"
    );
    assert!(!out.contains("forgebase:forgebase"));

    // The same documents, with their child rows, and the same users; migrate has nothing to do.
    assert_eq!(world.documents("dst"), before);
    for sql in [
        "SELECT count(*)::text FROM fb_user",
        "SELECT string_agg(username || ':' || password_hash, ',' ORDER BY username) FROM fb_user",
        "SELECT count(*)::text FROM fb_migration",
        "SELECT count(*)::text FROM doc_sales_order_item",
    ] {
        let (a, b) = (world.src.scalar(sql), world.dst.scalar(sql));
        assert!(a.is_some() && a == b, "{sql}: {a:?} against {b:?}");
    }
    let report = world.ok(&["migrate", "--site", "dst", "--doctypes", DOCTYPES]);
    assert!(
        report.contains("framework migrations: up to date"),
        "{report}"
    );
    // The restored site is usable: a new document goes in.
    world.ok(&[
        "doc",
        "insert",
        "customer",
        "--site",
        "dst",
        "--doctypes",
        DOCTYPES,
        "--data",
        r#"{"email":"c@example.com","full_name":"Cy"}"#,
    ]);
}

#[test]
fn restore_refuses_a_database_that_is_not_empty_and_changes_nothing() {
    let world = World::new();
    world.fill_src();
    let file = world.file("src.dump");
    let file_arg = file.to_str().expect("utf-8 path");
    world.ok(&["site", "backup", "src", "--out", file_arg]);

    // `site create` without --empty migrates, so its database is not empty.
    world.ok(&["site", "create", "dst", "--doctypes", DOCTYPES]);
    let tables = "SELECT count(*)::text FROM pg_class WHERE relnamespace = 'public'::regnamespace";
    let before = world.dst.scalar(tables);
    let message = fails(&world.run(&["site", "restore", "dst", "--from", file_arg]));
    assert_eq!(
        message,
        "error: the database of site `dst` is not empty; restore only into an empty database \
         (create one with `forgebase site create dst --empty`)\n"
    );
    assert_eq!(world.dst.scalar(tables), before);
    // The source database itself is not empty either.
    let message = fails(&world.run(&["site", "restore", "src", "--from", file_arg]));
    assert!(message.contains("is not empty"), "{message}");

    // A database that does not exist is told apart, with the way out.
    let missing = Db::named();
    let output = world
        .command(&["site", "restore", "dst", "--from", file_arg])
        .env(DST_URL, &missing.url)
        .output()
        .expect("run");
    let message = fails(&output);
    assert!(
        message.contains("does not exist") && message.contains("site create dst --empty"),
        "{message}"
    );
    assert!(!message.contains("forgebase:forgebase"), "{message}");

    // A file that is not a backup, and a path that is not a file.
    let junk = world.file("junk.dump");
    std::fs::write(&junk, "this is not a dump").expect("write");
    let empty = Db::created();
    let restore = |from: &Path| {
        world
            .command(&[
                "site",
                "restore",
                "dst",
                "--from",
                from.to_str().expect("path"),
            ])
            .env(DST_URL, &empty.url)
            .output()
            .expect("run")
    };
    let message = fails(&restore(&junk));
    assert!(
        message.contains("is not a backup that pg_restore can read"),
        "{message}"
    );
    let message = fails(&restore(&world.dir));
    assert!(message.contains("is not a file"), "{message}");
    assert_eq!(
        empty.scalar(
            "SELECT count(*)::text FROM pg_class WHERE relnamespace = 'public'::regnamespace"
        ),
        Some("0".to_owned())
    );
}

#[test]
fn a_backup_never_overwrites_a_file() {
    let world = World::new();
    world.fill_src();
    let file = world.file("keep.dump");
    std::fs::write(&file, "precious").expect("write");
    let message = fails(&world.run(&[
        "site",
        "backup",
        "src",
        "--out",
        file.to_str().expect("path"),
    ]));
    assert_eq!(
        message,
        format!("error: refusing to overwrite {}\n", file.display())
    );
    assert_eq!(std::fs::read_to_string(&file).expect("read"), "precious");

    // Neither does a second backup to the file of the first.
    let second = world.file("second.dump");
    let arg = second.to_str().expect("path");
    world.ok(&["site", "backup", "src", "--out", arg]);
    let first = std::fs::read(&second).expect("read");
    let message = fails(&world.run(&["site", "backup", "src", "--out", arg]));
    assert!(message.contains("refusing to overwrite"), "{message}");
    assert_eq!(std::fs::read(&second).expect("read"), first);

    // `--site` is not used by `site` commands; an unknown site is a usage error.
    let output = world.run(&[
        "site",
        "backup",
        "nope",
        "--out",
        world.file("n.dump").to_str().expect("path"),
    ]);
    assert_eq!(code(&output), Some(2), "{}", stderr(&output));
    assert!(!world.file("n.dump").exists());
}

#[test]
fn a_missing_client_tool_is_a_clear_error_and_leaves_no_file() {
    let world = World::new();
    world.fill_src();
    let nowhere = world.file("no-bin");
    std::fs::create_dir_all(&nowhere).expect("dir");
    let file = world.file("x.dump");
    let file_arg = file.to_str().expect("path");
    let message =
        fails(&world.run_with_bin(&["site", "backup", "src", "--out", file_arg], &nowhere));
    assert!(
        message.starts_with(
            "error: pg_dump not found; install the PostgreSQL client tools of version >= the server's"
        ) && message.contains("docs/guide/operations.md"),
        "{message}"
    );
    assert!(!file.exists(), "no file is left behind");

    // pg_restore: a real backup first, then the tool is hidden.
    world.ok(&["site", "backup", "src", "--out", file_arg]);
    world.ok(&["site", "create", "dst", "--empty"]);
    let message =
        fails(&world.run_with_bin(&["site", "restore", "dst", "--from", file_arg], &nowhere));
    assert!(
        message.starts_with("error: pg_restore not found;"),
        "{message}"
    );
    // Nothing was restored.
    assert_eq!(
        world.dst.scalar("SELECT to_regclass('fb_migration')::text"),
        None
    );
}

/// A directory with fake `pg_dump` and `pg_restore` scripts that print `version`, and then run
/// `body` (a shell script) for any other arguments.
#[cfg(unix)]
fn fake_tools(dir: &Path, version: &str, body: &str) -> PathBuf {
    use std::os::unix::fs::PermissionsExt;
    let bin = dir.join(format!("fake-bin-{}", unique_name()));
    std::fs::create_dir_all(&bin).expect("dir");
    for tool in ["pg_dump", "pg_restore"] {
        let script = format!(
            "#!/bin/sh\nif [ \"$1\" = \"--version\" ]; then echo '{tool} (PostgreSQL) {version}'; exit 0; fi\n{body}\n"
        );
        let path = bin.join(tool);
        std::fs::write(&path, script).expect("script");
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).expect("chmod");
    }
    bin
}

#[cfg(unix)]
#[test]
fn an_older_client_tool_is_refused_with_both_versions() {
    let world = World::new();
    world.fill_src();
    let bin = fake_tools(&world.dir, "15.4", "exit 0");
    let file = world.file("x.dump");
    let message = fails(&world.run_with_bin(
        &[
            "site",
            "backup",
            "src",
            "--out",
            file.to_str().expect("path"),
        ],
        &bin,
    ));
    assert!(
        message.contains("pg_dump is version 15, older than the server (version 16")
            || message.contains("older than the server (version 17")
            || message.contains("older than the server (version 18"),
        "{message}"
    );
    assert!(!file.exists());
}

#[cfg(unix)]
#[test]
fn the_tool_gets_the_connection_in_its_environment_only_and_a_failed_dump_leaves_no_file() {
    let world = World::new();
    world.fill_src();
    let log = world.file("tool.log");
    // Records its arguments and its PG* environment, writes some of a dump and fails.
    let body = format!(
        "echo \"ARGS: $*\" > '{log}'\nenv | grep '^PG' | sort >> '{log}'\n\
         for last; do :; done\nfile=\"${{last#--file=}}\"\necho partial > \"$file\"\n\
         echo 'something went wrong' >&2\nexit 3",
        log = log.display()
    );
    let bin = fake_tools(&world.dir, "99.0", &body);
    let file = world.file("x.dump");
    let output = world
        .command(&[
            "site",
            "backup",
            "src",
            "--out",
            file.to_str().expect("path"),
        ])
        .env("FORGEBASE_PG_BIN", &bin)
        .env("PGSERVICE", "inherited-service")
        .env("PGPASSFILE", "/inherited/passfile")
        .env("PGOPTIONS", "-c inherited=1")
        .output()
        .expect("run");
    let message = fails(&output);
    // The tool's standard error is passed on with its name; the failure is reported.
    assert!(
        message.contains("pg_dump: something went wrong\n"),
        "{message}"
    );
    assert!(
        message.contains("pg_dump failed; no backup file was written"),
        "{message}"
    );
    assert!(!file.exists(), "the partial file is removed");
    let logged = std::fs::read_to_string(&log).expect("log");
    let (args, env) = logged.split_once('\n').expect("args line");
    // Not on the command line: no URL, user, password, host or port.
    for secret in ["forgebase", "postgres://", "localhost", "5432"] {
        assert!(!args.contains(secret), "{args} contains {secret}");
    }
    assert!(
        args.contains("--format=custom") && args.contains("--no-owner"),
        "{args}"
    );
    // In the environment: the connection, and none of the inherited PG* variables.
    assert!(env.contains("PGPASSWORD=forgebase\n"), "{env}");
    assert!(env.contains("PGUSER=forgebase\n"), "{env}");
    assert!(
        env.contains(&format!("PGDATABASE={}\n", world.src.name)),
        "{env}"
    );
    assert!(env.contains("PGHOST=localhost\n"), "{env}");
    for inherited in ["PGSERVICE", "PGPASSFILE", "PGOPTIONS"] {
        assert!(!env.contains(inherited), "{env}");
    }
    // Neither our output nor our error has the password or the URL.
    assert!(
        !stdout(&output).contains("forgebase:forgebase")
            && !message.contains("forgebase:forgebase")
    );
}

#[cfg(unix)]
#[test]
fn a_backup_made_by_a_newer_pg_dump_is_refused_by_an_older_pg_restore() {
    let world = World::new();
    world.fill_src();
    // A real dump of this server; the fake pg_restore claims version 15.
    let file = world.file("src.dump");
    let file_arg = file.to_str().expect("path");
    world.ok(&["site", "backup", "src", "--out", file_arg]);
    world.ok(&["site", "create", "dst", "--empty"]);
    let bin = fake_tools(&world.dir, "15.4", "exit 0");
    let message = fails(&world.run_with_bin(&["site", "restore", "dst", "--from", file_arg], &bin));
    assert!(
        message.contains("pg_restore is version 15, older than the server"),
        "{message}"
    );
    assert_eq!(
        world.dst.scalar("SELECT to_regclass('fb_migration')::text"),
        None
    );
}
