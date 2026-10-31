//! Apps per site (M7.2, ADR 0014): a binary with two apps, `base` and `extra` (which depends on
//! `base`), run end to end with a config file of sites that install different apps, against real
//! PostgreSQL databases.
//!
//! This file has its own `main` (`harness = false` in `Cargo.toml`), like `app_binary`. Started
//! normally it is the test driver: it runs the scenarios one after the other and starts itself
//! again as the application binary (selected by the environment variable `FORGEBASE_TEST_BINARY`)
//! for every command. Every scenario makes its own throwaway databases; the driver fails loudly
//! (never skips) when `FORGEBASE_TEST_DATABASE_URL` is not set.

use std::io::{BufRead, BufReader, Read, Write};
use std::path::PathBuf;
use std::process::{Child, Command, ExitCode, Output, Stdio};
use std::sync::atomic::{AtomicU32, Ordering};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use forgebase::testing::TestDb;
use forgebase::{
    App, DocTypeFile, Document, Forgebase, JobContext, JobError, JobFuture, JobHandler, Method,
    MethodContext, MethodFuture, MethodHandler, MigrationFile, Schedule, Value,
};
use forgebase_db::{connect, quote_ident};

const BINARY_VAR: &str = "FORGEBASE_TEST_BINARY";
const ENV_VAR: &str = "FORGEBASE_TEST_DATABASE_URL";
const A_URL: &str = "FB_TEST_APPS_A_URL";
const B_URL: &str = "FB_TEST_APPS_B_URL";
const NEW_URL: &str = "FB_TEST_APPS_NEW_URL";
const PASSWORD: &str = "a password for root";

const ITEM: &str = "name = \"item\"\nlabel = \"Item\"\nnaming = { field = \"code\" }\n\
    [[fields]]\nname = \"code\"\ntype = \"data\"\nrequired = true\nunique = true\n";
const EXTRA_ITEM: &str = "name = \"extra_item\"\nlabel = \"Extra item\"\nnaming = { field = \"code\" }\n\
    [[fields]]\nname = \"code\"\ntype = \"data\"\nrequired = true\nunique = true\n";

const EXTRA_SEED: MigrationFile = MigrationFile::new(
    1,
    "seed",
    "migrations/0001_seed.sql",
    "CREATE TABLE extra_seed (entry text)",
);

struct Noop;

impl JobHandler for Noop {
    fn run<'a>(&'a self, _: &'a mut JobContext<'_>) -> JobFuture<'a> {
        Box::pin(async { Ok::<(), JobError>(()) })
    }
}

/// A method that answers with its own full name (M9.1): `base.hello` and `extra.hello`.
struct Hello;

impl MethodHandler for Hello {
    fn call<'a>(&'a self, ctx: &'a mut MethodContext<'_>) -> MethodFuture<'a> {
        Box::pin(async move { Ok(forgebase::serde_json::json!({ "method": ctx.name() })) })
    }
}

/// `extra.stamp` inserts an `extra_item` with the `code` of its payload.
struct Stamp;

impl JobHandler for Stamp {
    fn run<'a>(&'a self, ctx: &'a mut JobContext<'_>) -> JobFuture<'a> {
        Box::pin(async move {
            let code = ctx.payload()["code"].as_str().unwrap_or("none").to_owned();
            let item = Document {
                values: [("code".to_owned(), Value::Text(code))].into(),
                ..Document::default()
            };
            ctx.documents().insert("extra_item", item).await?;
            Ok(())
        })
    }
}

fn base() -> App {
    App::new("base")
        .version("1.0.0")
        .doctypes(const { &[DocTypeFile::new("doctypes/item.toml", ITEM)] })
        .job("tick", Noop)
        .schedule("tick", Schedule::daily_at(4, 0))
        .method("hello", Method::new(Hello).any_user())
}

fn extra() -> App {
    App::new("extra")
        .depends_on(&["base"])
        .doctypes(const { &[DocTypeFile::new("doctypes/extra_item.toml", EXTRA_ITEM)] })
        .migrations(&[EXTRA_SEED])
        .job("stamp", Stamp)
        .job("mark", Noop)
        .schedule("mark", Schedule::every_minutes(60))
        .method("hello", Method::new(Hello).any_user())
}

fn main() -> ExitCode {
    match std::env::var(BINARY_VAR).as_deref() {
        Ok("app") => Forgebase::new().app(base()).app(extra()).run(),
        _ => {
            let scenarios: [(&str, fn()); 8] = [
                ("config_validation_and_site_list", config_and_site_list),
                (
                    "migrate_installs_only_the_apps_of_the_site",
                    migrate_per_site,
                ),
                ("commands_know_only_the_apps_of_the_site", cli_per_site),
                ("http_shows_only_the_apps_of_the_site", http_per_site),
                ("a_worker_runs_only_the_jobs_of_its_site", worker_per_site),
                ("a_removed_app_keeps_its_data", removed_app),
                ("site_create", site_create),
                ("m7_1_follow_ups", follow_ups),
            ];
            for (name, scenario) in scenarios {
                scenario();
                println!("scenario {name} ... ok");
            }
            println!("test site_apps ... ok");
            ExitCode::SUCCESS
        }
    }
}

// ---------------------------------------------------------------------------------------------
// Helpers

fn text(bytes: &[u8]) -> String {
    String::from_utf8_lossy(bytes).into_owned()
}

fn stdout(output: &Output) -> String {
    text(&output.stdout)
}

fn stderr(output: &Output) -> String {
    text(&output.stderr)
}

#[track_caller]
fn succeeds(output: &Output) -> String {
    assert_eq!(output.status.code(), Some(0), "stderr: {}", stderr(output));
    stdout(output)
}

#[track_caller]
fn fails(output: &Output, code: i32) -> String {
    assert_eq!(
        output.status.code(),
        Some(code),
        "stdout: {}\nstderr: {}",
        stdout(output),
        stderr(output)
    );
    stderr(output)
}

fn block_on<T>(future: impl std::future::Future<Output = T>) -> T {
    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("runtime")
        .block_on(future)
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

fn database_of(url: &str) -> &str {
    url.rsplit('/').next().expect("database name")
}

/// One text value (or `None` for NULL) of the first row.
fn scalar(db: &TestDb, sql: &str) -> Option<String> {
    let rows = block_on_rows(db, sql);
    rows.first()
        .and_then(|row| row.first())
        .and_then(|v| (v != "NULL").then(|| v.clone()))
}

fn block_on_rows(db: &TestDb, sql: &str) -> Vec<Vec<String>> {
    // `TestDb` owns a pool bound to the runtime that created it, so queries run on the
    // scenario's runtime; see `World`.
    WORLD_RUNTIME.with(|runtime| runtime.block_on(db.rows(sql)))
}

thread_local! {
    static WORLD_RUNTIME: tokio::runtime::Runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("runtime");
}

fn new_db() -> TestDb {
    WORLD_RUNTIME.with(|runtime| runtime.block_on(TestDb::new()))
}

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

/// Two databases and a config file with two sites: `a` installs `a_apps` and `b` installs
/// `b_apps` (a TOML array text, or `None` for the key to be absent).
struct World {
    a: TestDb,
    b: TestDb,
    dir: PathBuf,
}

impl World {
    fn new(a_apps: Option<&str>, b_apps: Option<&str>) -> Self {
        static COUNTER: AtomicU32 = AtomicU32::new(0);
        let dir = std::env::temp_dir().join(format!(
            "fb_site_apps_{}_{}",
            std::process::id(),
            COUNTER.fetch_add(1, Ordering::Relaxed)
        ));
        std::fs::create_dir_all(&dir).expect("temporary directory");
        let world = Self {
            a: new_db(),
            b: new_db(),
            dir,
        };
        world.config(a_apps, b_apps);
        world
    }

    fn config_path(&self) -> PathBuf {
        self.dir.join("forgebase.toml")
    }

    fn config(&self, a_apps: Option<&str>, b_apps: Option<&str>) {
        let apps = |list: Option<&str>| list.map_or(String::new(), |l| format!("apps = {l}\n"));
        std::fs::write(
            self.config_path(),
            format!(
                "[[sites]]\nname = \"a\"\nhosts = [\"a.localhost\"]\ndatabase_url_env = \"{A_URL}\"\n{}\n\
                 [[sites]]\nname = \"b\"\nhosts = [\"b.localhost\"]\ndatabase_url_env = \"{B_URL}\"\n{}",
                apps(a_apps),
                apps(b_apps)
            ),
        )
        .expect("write the config");
    }

    fn command(&self, args: &[&str]) -> Command {
        command_with(
            &self.config_path(),
            args,
            &[(A_URL, self.a.url()), (B_URL, self.b.url())],
        )
    }

    fn run(&self, args: &[&str]) -> Output {
        self.command(args).output().expect("run the app binary")
    }

    fn run_stdin(&self, args: &[&str], stdin: &str) -> Output {
        let mut command = self.command(args);
        command.stdin(Stdio::piped());
        let mut child = command.spawn().expect("start the app binary");
        child
            .stdin
            .take()
            .expect("stdin")
            .write_all(stdin.as_bytes())
            .expect("write stdin");
        child.wait_with_output().expect("run")
    }

    /// Migrates both sites and creates the Administrator `root` on each.
    fn set_up(&self) {
        for site in ["a", "b"] {
            succeeds(&self.run(&["migrate", "--site", site]));
            succeeds(&self.run_stdin(
                &[
                    "user",
                    "create",
                    "root",
                    "--role",
                    "Administrator",
                    "--site",
                    site,
                ],
                &format!("{PASSWORD}\n"),
            ));
        }
    }
}

impl Drop for World {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.dir);
    }
}

fn command_with(config: &std::path::Path, args: &[&str], env: &[(&str, &str)]) -> Command {
    let mut command = Command::new(std::env::current_exe().expect("current exe"));
    command
        .arg("--config")
        .arg(config)
        .args(args)
        .env(BINARY_VAR, "app")
        .env_remove("FORGEBASE_DATABASE_URL")
        .env_remove("FORGEBASE_CONFIG")
        .env_remove("FORGEBASE_ALLOWED_HOSTS")
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    for (name, value) in env {
        command.env(name, value);
    }
    command
}

fn names(db: &TestDb, sql: &str) -> Vec<String> {
    block_on_rows(db, sql)
        .into_iter()
        .map(|mut row| row.remove(0))
        .collect()
}

const SCHEDULES: &str = "SELECT name FROM fb_schedule ORDER BY name COLLATE \"C\"";
const APPS: &str = "SELECT name FROM fb_app ORDER BY name COLLATE \"C\"";
const BUILTIN_SCHEDULES: [&str; 3] = [
    "forgebase.purge_jobs",
    "forgebase.purge_login_failures",
    "forgebase.purge_sessions",
];

fn with_builtin(mut own: Vec<&str>) -> Vec<String> {
    own.extend(BUILTIN_SCHEDULES);
    own.sort_unstable();
    own.into_iter().map(str::to_owned).collect()
}

// ---------------------------------------------------------------------------------------------
// Scenarios

/// The `apps` key is validated with the other config problems, and `site list` prints the
/// sites without touching a database.
fn config_and_site_list() {
    let dir = std::env::temp_dir().join(format!("fb_site_apps_cfg_{}", std::process::id()));
    std::fs::create_dir_all(&dir).expect("dir");
    let config = dir.join("forgebase.toml");
    let site = |apps: &str| {
        format!(
            "[[sites]]\nname = \"a\"\nhosts = [\"a.localhost\"]\ndatabase_url_env = \"{A_URL}\"\n{apps}\n"
        )
    };
    let secret = "postgres://user:hunter2@127.0.0.1:1/never_connected";
    let list = |content: &str| {
        std::fs::write(&config, content).expect("write");
        command_with(&config, &["site", "list"], &[(A_URL, secret)])
            .output()
            .expect("run")
    };

    // Problems, all at once, each naming the site.
    let message = fails(
        &list(&site("apps = [\"extra\", \"nope\", \"base\", \"base\"]")),
        1,
    );
    assert!(
        message.contains("site `a`: unknown app `nope` in `apps`")
            && message.contains("site `a`: app `base` is listed twice in `apps`"),
        "{message}"
    );
    let message = fails(&list(&site("apps = [\"extra\"]")), 1);
    assert!(
        message.contains("site `a`: app `extra` needs `base`, which is not in `apps`"),
        "{message}"
    );
    // A host that could never match a request is refused when the file is loaded.
    let message = fails(&list(&site("").replace("a.localhost", "a/b.localhost")), 1);
    assert!(message.contains("host \"a/b.localhost\""), "{message}");

    // Valid: absent means all, an empty list means none, a closed subset is fine.
    let out = succeeds(&list(&format!(
        "{}\n[[sites]]\nname = \"b\"\nhosts = [\"b.localhost\", \"b2.localhost\"]\ndatabase_url_env = \"{B_URL}\"\napps = []\n\n\
         [[sites]]\nname = \"c\"\nhosts = [\"c.localhost\"]\ndatabase_url_env = \"C_URL\"\napps = [\"extra\", \"base\"]\n",
        site("")
    )));
    let expected = [
        "SITE  HOSTS                      APPS         DATABASE_URL_ENV".to_owned(),
        format!("a     a.localhost                all          {A_URL}"),
        format!("b     b.localhost, b2.localhost  (none)       {B_URL}"),
        "c     c.localhost                extra, base  C_URL".to_owned(),
    ]
    .join("\n")
        + "\n";
    assert_eq!(out, expected);
    assert!(
        !out.contains("hunter2") && !out.contains("never_connected"),
        "{out}"
    );

    // Without a config file: the implicit site, from the environment, no database access.
    let out = succeeds(
        &Command::new(std::env::current_exe().expect("exe"))
            .args(["site", "list"])
            .env(BINARY_VAR, "app")
            .env("FORGEBASE_DATABASE_URL", secret)
            .env_remove("FORGEBASE_CONFIG")
            .env_remove("FORGEBASE_ALLOWED_HOSTS")
            .output()
            .expect("run"),
    );
    assert!(
        out.starts_with("SITE")
            && out.contains("default")
            && out.contains("localhost, 127.0.0.1")
            && out.contains("all")
            && out.contains("FORGEBASE_DATABASE_URL")
            && !out.contains("hunter2"),
        "{out}"
    );
    // `--site` is not used by `site` commands.
    let message = fails(
        &Command::new(std::env::current_exe().expect("exe"))
            .args(["site", "list", "--site", "default"])
            .env(BINARY_VAR, "app")
            .output()
            .expect("run"),
        2,
    );
    assert!(message.contains("do not use --site"), "{message}");
    std::fs::remove_dir_all(&dir).expect("cleanup");
}

/// `migrate --site` installs the schema, the migrations and the schedules of the site's apps
/// only.
fn migrate_per_site() {
    let world = World::new(Some("[\"base\"]"), Some("[\"base\", \"extra\"]"));
    let out_a = succeeds(&world.run(&["migrate", "--site", "a"]));
    let out_b = succeeds(&world.run(&["migrate", "--site", "b"]));
    assert!(
        !out_a.contains("extra") && out_a.contains("doc_item"),
        "{out_a}"
    );
    assert!(
        out_b.contains("doc_extra_item") && out_b.contains("extra 0001 seed"),
        "{out_b}"
    );

    let table =
        |db: &TestDb, name: &str| scalar(db, &format!("SELECT to_regclass('{name}')::text"));
    assert_eq!(table(&world.a, "doc_item").as_deref(), Some("doc_item"));
    assert_eq!(table(&world.a, "doc_extra_item"), None);
    assert_eq!(table(&world.a, "extra_seed"), None);
    assert_eq!(
        table(&world.b, "doc_extra_item").as_deref(),
        Some("doc_extra_item")
    );
    assert_eq!(table(&world.b, "extra_seed").as_deref(), Some("extra_seed"));

    assert_eq!(names(&world.a, APPS), ["base"]);
    assert_eq!(names(&world.b, APPS), ["base", "extra"]);
    assert_eq!(names(&world.a, SCHEDULES), with_builtin(vec!["base.tick"]));
    assert_eq!(
        names(&world.b, SCHEDULES),
        with_builtin(vec!["base.tick", "extra.mark"])
    );
    // Migrating again changes nothing and warns about nothing.
    let again = world.run(&["migrate", "--site", "b"]);
    succeeds(&again);
    assert!(stderr(&again).is_empty(), "{}", stderr(&again));
}

/// Documents and jobs of an app that is not installed are unknown on that site.
fn cli_per_site() {
    let world = World::new(Some("[\"base\"]"), Some("[\"base\", \"extra\"]"));
    world.set_up();
    let data = r#"{"code":"x-1"}"#;
    let message = fails(
        &world.run(&["doc", "insert", "extra_item", "--site", "a", "--data", data]),
        1,
    );
    assert!(
        message.contains("unknown DocType `extra_item`") && message.contains("item"),
        "{message}"
    );
    assert!(!message.contains("known DocTypes: extra_item"), "{message}");
    let out = succeeds(&world.run(&["doc", "insert", "extra_item", "--site", "b", "--data", data]));
    assert!(out.contains("\"name\": \"x-1\""), "{out}");
    // A DocType of the installed app works on both.
    succeeds(&world.run(&["doc", "insert", "item", "--site", "a", "--data", data]));

    let message = fails(
        &world.run(&["job", "enqueue", "extra.stamp", "--site", "a"]),
        1,
    );
    assert!(message.contains("unknown job `extra.stamp`"), "{message}");
    assert_eq!(
        scalar(&world.a, "SELECT count(*)::text FROM fb_job").as_deref(),
        Some("0")
    );
    succeeds(&world.run(&["job", "enqueue", "extra.stamp", "--site", "b"]));
    // Built-in jobs are in every site.
    succeeds(&world.run(&["job", "enqueue", "forgebase.purge_jobs", "--site", "a"]));
}

/// Reads a child's standard output to its end in the background, so a line it prints can never
/// block it or meet a closed pipe.
fn drain<R: std::io::Read + Send + 'static>(mut pipe: R) {
    std::thread::spawn(move || {
        let _ = std::io::copy(&mut pipe, &mut std::io::sink());
    });
}

/// A running `forgebase serve`, killed when dropped.
struct Server {
    child: Child,
    address: String,
}

impl Server {
    fn start(world: &World) -> Server {
        let mut child = world
            .command(&["serve"])
            .env("FORGEBASE_LISTEN", "127.0.0.1:0")
            .spawn()
            .expect("start serve");
        let mut reader = BufReader::new(child.stdout.take().expect("stdout"));
        let mut line = String::new();
        reader.read_line(&mut line).expect("the listening line");
        let address = line
            .trim()
            .strip_prefix("listening on http://")
            .unwrap_or_else(|| panic!("unexpected first line {line:?}"))
            .to_owned();
        // The site lines follow; keep reading until the server stops.
        drain(reader);
        Server { child, address }
    }

    fn request(
        &self,
        method: &str,
        path: &str,
        host: &str,
        headers: &[&str],
        body: &str,
    ) -> (u16, String) {
        let mut stream = std::net::TcpStream::connect(&self.address).expect("connect");
        let mut request =
            format!("{method} {path} HTTP/1.1\r\nHost: {host}\r\nConnection: close\r\n");
        for header in headers {
            request.push_str(header);
            request.push_str("\r\n");
        }
        request.push_str(&format!("Content-Length: {}\r\n\r\n{body}", body.len()));
        stream.write_all(request.as_bytes()).expect("send");
        let mut reply = String::new();
        stream.read_to_string(&mut reply).expect("read");
        let (head, body) = reply.split_once("\r\n\r\n").expect("head and body");
        let status = head
            .split(' ')
            .nth(1)
            .expect("status")
            .parse()
            .expect("number");
        let cookie = head
            .lines()
            .find_map(|l| l.strip_prefix("set-cookie: "))
            .map(|c| c.split(';').next().expect("pair"))
            .unwrap_or_default();
        // The cookie rides along in the first element of the body for the caller's convenience.
        (status, format!("{cookie}\n{body}"))
    }

    /// Logs in as `root` and returns the `Cookie` and CSRF headers.
    fn cookie(&self, host: &str) -> String {
        let (status, reply) = self.request(
            "POST",
            "/api/v1/login",
            host,
            &["Content-Type: application/json"],
            &format!(r#"{{"username":"root","password":"{PASSWORD}"}}"#),
        );
        assert_eq!(status, 200, "{reply}");
        let (cookie, body) = reply.split_once('\n').expect("cookie and body");
        let json: serde_json::Value = serde_json::from_str(body).expect("json");
        let csrf = json["data"]["csrf_token"].as_str().expect("csrf token");
        // Two header lines in one string: `request` writes each header followed by CRLF.
        format!("Cookie: {cookie}\r\nX-Forgebase-CSRF: {csrf}")
    }
}

impl Drop for Server {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

/// One router, two sites: the meta API, the resources and the desk of a site show only its apps.
fn http_per_site() {
    let world = World::new(Some("[\"base\"]"), Some("[\"base\", \"extra\"]"));
    world.set_up();
    let server = Server::start(&world);
    let (cookie_a, cookie_b) = (server.cookie("a.localhost"), server.cookie("b.localhost"));
    let doctypes = |host: &str, cookie: &str| -> Vec<String> {
        let (status, reply) = server.request("GET", "/api/v1/meta", host, &[cookie], "");
        assert_eq!(status, 200, "{reply}");
        let json: serde_json::Value =
            serde_json::from_str(reply.split_once('\n').expect("body").1).expect("json");
        let mut names: Vec<String> = json["data"]
            .as_array()
            .expect("list")
            .iter()
            .map(|d| d["name"].as_str().expect("name").to_owned())
            .collect();
        names.sort_unstable();
        names
    };
    assert_eq!(doctypes("a.localhost", &cookie_a), ["item"]);
    assert_eq!(doctypes("b.localhost", &cookie_b), ["extra_item", "item"]);

    let on_a = |method: &str, path: &str, body: &str| {
        let headers = [cookie_a.as_str(), "Content-Type: application/json"];
        server
            .request(method, path, "a.localhost", &headers, body)
            .0
    };
    // A DocType of an app that is not installed does not exist on A: meta, list and create.
    assert_eq!(on_a("GET", "/api/v1/meta/extra_item", ""), 404);
    assert_eq!(on_a("GET", "/api/v1/resource/extra_item", ""), 404);
    assert_eq!(
        on_a("POST", "/api/v1/resource/extra_item", r#"{"code":"h-1"}"#),
        404
    );
    // On B it is served; on A the installed DocType still is.
    let on_b = |method: &str, path: &str, body: &str| {
        let headers = [cookie_b.as_str(), "Content-Type: application/json"];
        server
            .request(method, path, "b.localhost", &headers, body)
            .0
    };
    assert_eq!(
        on_b("POST", "/api/v1/resource/extra_item", r#"{"code":"h-1"}"#),
        201
    );
    assert_eq!(on_b("GET", "/api/v1/resource/extra_item", ""), 200);
    assert_eq!(on_a("GET", "/api/v1/resource/item", ""), 200);
    // Nothing was written to A.
    assert_eq!(
        scalar(&world.a, "SELECT to_regclass('doc_extra_item')::text"),
        None
    );
    // The methods (M9.1) of an app that is not installed do not exist either, over a real server.
    let method = |hosted: &dyn Fn(&str, &str, &str) -> u16, name: &str| {
        hosted("POST", &format!("/api/v1/method/{name}"), "{}")
    };
    assert_eq!(method(&on_a, "base.hello"), 200);
    assert_eq!(method(&on_a, "extra.hello"), 404);
    assert_eq!(method(&on_b, "base.hello"), 200);
    assert_eq!(method(&on_b, "extra.hello"), 200);
}

/// Polls `check` until it is true, failing after `what` did not happen within 30 seconds.
fn wait_until(what: &str, mut check: impl FnMut() -> bool) {
    let started = Instant::now();
    while !check() {
        assert!(
            started.elapsed() < Duration::from_secs(30),
            "timed out waiting for: {what}"
        );
        std::thread::sleep(Duration::from_millis(50));
    }
}

fn stop_worker(mut child: Child) -> String {
    let killed = Command::new("kill")
        .args(["-TERM", &child.id().to_string()])
        .status()
        .expect("kill");
    assert!(killed.success());
    let mut log = String::new();
    child
        .stderr
        .take()
        .expect("stderr")
        .read_to_string(&mut log)
        .expect("log");
    let exit = child.wait().expect("exit");
    assert_eq!(exit.code(), Some(0), "{log}");
    log
}

const INSERT_JOB: &str = "INSERT INTO fb_job (id, name, payload, status, enqueued_by) \
    VALUES (gen_random_uuid(), '{name}', '{\"code\":\"from-job\"}', 'queued', 'test')";

/// A worker claims only the jobs of the apps of its site, and the other site's worker runs them.
fn worker_per_site() {
    let world = World::new(Some("[\"base\"]"), Some("[\"base\", \"extra\"]"));
    world.set_up();
    // `extra.stamp` is queued in both databases, directly (`job enqueue` would refuse it on A).
    for db in [&world.a, &world.b] {
        WORLD_RUNTIME.with(|runtime| {
            runtime.block_on(db.exec(&INSERT_JOB.replace("{name}", "extra.stamp")))
        });
    }
    // A built-in job after it, on A: when it is done the worker has been through the queue.
    let purge = "forgebase.purge_sessions";
    WORLD_RUNTIME
        .with(|runtime| runtime.block_on(world.a.exec(&INSERT_JOB.replace("{name}", purge))));
    let status = |db: &TestDb, name: &str| {
        scalar(
            db,
            &format!("SELECT status FROM fb_job WHERE name = '{name}'"),
        )
    };

    // A stale `extra.mark` schedule row, due now, in A's database (the app is not installed on
    // A, so `migrate` never made one: this is what a leftover would look like), and the real one
    // of B made due too, as the control that a due schedule does tick.
    for db in [&world.a, &world.b] {
        let sql = "INSERT INTO fb_schedule (name, spec, next_run_at) \
             VALUES ('extra.mark', 'every 60m', now() - interval '1 hour') \
             ON CONFLICT (name) DO UPDATE SET next_run_at = now() - interval '1 hour'";
        WORLD_RUNTIME.with(|runtime| runtime.block_on(db.exec(sql)));
    }
    let mut worker_a = world
        .command(&["worker", "--site", "a"])
        .spawn()
        .expect("worker a");
    drain(worker_a.stdout.take().expect("stdout"));
    wait_until("site a's worker to run the built-in job", || {
        status(&world.a, purge).as_deref() == Some("done")
    });
    let log = stop_worker(worker_a);
    // The job of the app that is not installed on A stays queued, untouched.
    assert_eq!(status(&world.a, "extra.stamp").as_deref(), Some("queued"));
    assert_eq!(
        scalar(
            &world.a,
            "SELECT attempts::text FROM fb_job WHERE name = 'extra.stamp'"
        )
        .as_deref(),
        Some("0")
    );
    assert!(!log.contains("extra.stamp"), "{log}");
    // The worker of A does not tick the stale `extra.mark` row: nothing was enqueued and the
    // row is untouched.
    assert_eq!(
        scalar(
            &world.a,
            "SELECT count(*)::text FROM fb_job WHERE name = 'extra.mark'"
        )
        .as_deref(),
        Some("0")
    );
    assert_eq!(
        scalar(
            &world.a,
            "SELECT (last_run_at IS NULL)::text FROM fb_schedule WHERE name = 'extra.mark'"
        )
        .as_deref(),
        Some("true")
    );

    // B's worker runs it: the job's handler inserts an `extra_item` of B.
    let mut worker_b = world
        .command(&["worker", "--site", "b"])
        .spawn()
        .expect("worker b");
    drain(worker_b.stdout.take().expect("stdout"));
    wait_until("site b's worker to run extra.stamp", || {
        status(&world.b, "extra.stamp").as_deref() == Some("done")
    });
    stop_worker(worker_b);
    // The control: B's worker did tick its due `extra.mark` schedule.
    assert_eq!(
        scalar(
            &world.b,
            "SELECT (count(*) >= 1)::text FROM fb_job WHERE name = 'extra.mark'"
        )
        .as_deref(),
        Some("true")
    );
    assert_eq!(
        scalar(&world.b, "SELECT count(*)::text FROM doc_extra_item").as_deref(),
        Some("1")
    );
}

/// Dropping an app from a site's list keeps its tables and data; `migrate` warns and removes the
/// app's schedules; the DocType is no longer served.
fn removed_app() {
    let world = World::new(Some("[\"base\"]"), Some("[\"base\", \"extra\"]"));
    world.set_up();
    succeeds(&world.run(&[
        "doc",
        "insert",
        "extra_item",
        "--site",
        "b",
        "--data",
        r#"{"code":"kept"}"#,
    ]));
    assert_eq!(
        names(&world.b, SCHEDULES),
        with_builtin(vec!["base.tick", "extra.mark"])
    );

    world.config(Some("[\"base\"]"), Some("[\"base\"]"));
    let output = world.run(&["migrate", "--site", "b"]);
    let report = succeeds(&output);
    assert_eq!(
        stderr(&output),
        "warning: site `b`: app `extra` is recorded in the database but not installed; its tables and data are kept\n"
    );
    // The schedule of the removed app is deleted (and reported), nothing else is dropped.
    assert!(report.contains("extra.mark"), "{report}");
    assert_eq!(names(&world.b, SCHEDULES), with_builtin(vec!["base.tick"]));
    assert_eq!(names(&world.b, APPS), ["base", "extra"]);
    assert_eq!(
        scalar(&world.b, "SELECT count(*)::text FROM doc_extra_item").as_deref(),
        Some("1")
    );
    assert_eq!(
        scalar(&world.b, "SELECT count(*)::text FROM extra_seed").as_deref(),
        Some("0")
    );
    // The DocType is gone from the site.
    let message = fails(
        &world.run(&["doc", "get", "extra_item", "kept", "--site", "b"]),
        1,
    );
    assert!(
        message.contains("unknown DocType `extra_item`"),
        "{message}"
    );
    let server = Server::start(&world);
    let cookie = server.cookie("b.localhost");
    let (status, _) = server.request(
        "GET",
        "/api/v1/resource/extra_item",
        "b.localhost",
        &[&cookie],
        "",
    );
    assert_eq!(status, 404);
    drop(server);

    // Installing it again finds the data where it was left.
    world.config(Some("[\"base\"]"), Some("[\"base\", \"extra\"]"));
    let output = world.run(&["migrate", "--site", "b"]);
    succeeds(&output);
    assert!(stderr(&output).is_empty(), "{}", stderr(&output));
    assert_eq!(
        names(&world.b, SCHEDULES),
        with_builtin(vec!["base.tick", "extra.mark"])
    );
    let out = succeeds(&world.run(&["doc", "get", "extra_item", "kept", "--site", "b"]));
    assert!(out.contains("\"name\": \"kept\""), "{out}");
}

/// A scratch database name that does not exist, and the URL of the admin database it is made in.
struct Scratch {
    name: String,
    url: String,
    admin: String,
}

impl Scratch {
    fn new() -> Self {
        let admin = admin_url();
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map_or(0, |d| d.subsec_nanos());
        let name = format!("fb_test_create_{:x}_{nanos:x}", std::process::id());
        Self {
            url: with_database(&admin, &name),
            name,
            admin,
        }
    }

    fn exists(&self) -> bool {
        let (admin, name) = (self.admin.clone(), self.name.clone());
        block_on(async move {
            let pool = connect(&admin).await.expect("connect");
            let client = pool.get().await.expect("connection");
            let row = client
                .query_one(
                    "SELECT count(*) FROM pg_database WHERE datname = $1",
                    &[&name],
                )
                .await
                .expect("query");
            pool.close();
            row.get::<_, i64>(0) == 1
        })
    }

    /// The count of `fb_migration` rows of the created database.
    fn migrations(&self) -> i64 {
        let url = self.url.clone();
        block_on(async move {
            let pool = connect(&url).await.expect("connect");
            let client = pool.get().await.expect("connection");
            let row = client
                .query_one("SELECT count(*) FROM fb_migration", &[])
                .await
                .expect("query");
            pool.close();
            row.get::<_, i64>(0)
        })
    }
}

impl Drop for Scratch {
    fn drop(&mut self) {
        let sql = format!(
            "DROP DATABASE IF EXISTS {} WITH (FORCE)",
            quote_ident(&self.name)
        );
        let admin = self.admin.clone();
        let _ = std::thread::spawn(move || {
            let _ = std::panic::catch_unwind(|| {
                block_on(async move {
                    let pool = connect(&admin).await.expect("connect");
                    let client = pool.get().await.expect("connection");
                    let _ = client.batch_execute(&sql).await;
                    pool.close();
                });
            });
        })
        .join();
    }
}

/// `site create` makes the database and migrates it; it refuses an existing database and an
/// unusable name, and never prints the URL.
fn site_create() {
    let scratch = Scratch::new();
    let dir = std::env::temp_dir().join(format!("fb_site_apps_create_{}", std::process::id()));
    std::fs::create_dir_all(&dir).expect("dir");
    let config = dir.join("forgebase.toml");
    std::fs::write(
        &config,
        format!(
            "[[sites]]\nname = \"fresh\"\nhosts = [\"fresh.localhost\"]\ndatabase_url_env = \"{NEW_URL}\"\napps = [\"base\"]\n"
        ),
    )
    .expect("write");
    let create = |url: &str, extra: &[&str]| {
        let mut args = vec!["site", "create", "fresh"];
        args.extend_from_slice(extra);
        command_with(&config, &args, &[(NEW_URL, url)])
            .output()
            .expect("run")
    };

    // Not a site of the config file: a usage error, nothing is created.
    let message = fails(
        &command_with(
            &config,
            &["site", "create", "nope"],
            &[(NEW_URL, &scratch.url)],
        )
        .output()
        .expect("run"),
        2,
    );
    assert!(message.contains("unknown site `nope`"), "{message}");
    assert!(!scratch.exists());

    // Created and migrated; the maintenance database can be named (here: the admin database).
    let maintenance = database_of(&scratch.admin).to_owned();
    let out = succeeds(&create(&scratch.url, &["--maintenance-db", &maintenance]));
    assert!(
        out.starts_with("created the database of site `fresh`\n")
            && out.contains(&format!(
                "framework migrations: {} applied",
                forgebase_db::MIGRATIONS.len()
            ))
            && out.contains("doc_item")
            && !out.contains("doc_extra_item"),
        "{out}"
    );
    assert!(scratch.exists());
    assert_eq!(scratch.migrations(), forgebase_db::MIGRATIONS.len() as i64);

    // A second create is refused, and the database is left alone.
    let output = create(&scratch.url, &[]);
    let message = fails(&output, 1);
    assert_eq!(
        message,
        "error: database for site `fresh` already exists; run `forgebase migrate --site fresh`\n"
    );
    assert!(stdout(&output).is_empty());
    assert_eq!(scratch.migrations(), forgebase_db::MIGRATIONS.len() as i64);

    // An unusable database name in the URL is refused before anything is created; the message
    // has neither the URL nor the password.
    for bad in ["bad%3Bname", "bad%20name", "x%22y"] {
        let url = with_database(&scratch.url, bad);
        let message = fails(&create(&url, &[]), 1);
        assert!(
            message.contains("site `fresh`") && message.contains("must be 1 to 63 characters"),
            "{message}"
        );
        assert!(
            !message.contains("forgebase:forgebase") && !message.contains(&url),
            "{message}"
        );
    }
    // A URL that is not a URL, and a maintenance database that does not exist.
    let message = fails(&create("not a url ::", &[]), 1);
    assert!(
        message.contains("site `fresh`") && !message.contains("not a url"),
        "{message}"
    );
    let fresh = Scratch::new();
    let message = fails(
        &create(
            &fresh.url,
            &["--maintenance-db", "fb_no_such_maintenance_db"],
        ),
        1,
    );
    assert!(message.contains("site `fresh`"), "{message}");
    assert!(!fresh.exists(), "no database was created");
    assert!(!message.contains(&fresh.url), "{message}");
    std::fs::remove_dir_all(&dir).expect("cleanup");
    drop(scratch);
}

/// The follow-ups of the M7.1 review.
fn follow_ups() {
    let world = World::new(None, None);
    // The database of site b points nowhere (port 1 refuses at once).
    let dead = "postgres://nobody:s3cret@127.0.0.1:1/none";
    let run = |args: &[&str], stdin: &str| {
        let mut command = command_with(
            &world.config_path(),
            args,
            &[(A_URL, world.a.url()), (B_URL, dead)],
        );
        command.stdin(Stdio::piped());
        let mut child = command.spawn().expect("start");
        child
            .stdin
            .take()
            .expect("stdin")
            .write_all(stdin.as_bytes())
            .expect("write stdin");
        child.wait_with_output().expect("run")
    };

    // LOW-1: a payload that is not JSON (given as an argument or on standard input) is reported
    // before the database is opened, so the unreachable URL does not matter.
    for (args, stdin) in [
        (
            vec![
                "job",
                "enqueue",
                "base.tick",
                "--site",
                "b",
                "--payload",
                "{",
            ],
            "",
        ),
        (
            vec![
                "job",
                "enqueue",
                "base.tick",
                "--site",
                "b",
                "--payload",
                "-",
            ],
            "{not json",
        ),
    ] {
        let message = fails(&run(&args, stdin), 1);
        assert!(message.contains("not valid JSON"), "{args:?}: {message}");
        assert!(
            !message.contains("s3cret") && !message.contains("connect"),
            "{message}"
        );
    }
    // A valid payload then does reach the database: the error is the connection, with the site.
    let message = fails(&run(&["job", "enqueue", "base.tick", "--site", "b"], ""), 1);
    // LOW-2: connection errors name the site, and never the URL.
    assert!(message.starts_with("error: site `b`: "), "{message}");
    assert!(
        !message.contains("s3cret") && !message.contains(dead),
        "{message}"
    );

    // INFO-3 of M7.2: `serve` composes every site's registry before it opens a pool. Site `a`
    // (all apps) cannot connect either, but site `b` (only `base`) cannot compose the local
    // DocType that links to `extra_item`: that is the error, and no connection is tried.
    let dir = std::env::temp_dir().join(format!("fb_site_apps_compose_{}", std::process::id()));
    std::fs::create_dir_all(&dir).expect("dir");
    let doctypes = dir.join("doctypes");
    std::fs::create_dir_all(&doctypes).expect("dir");
    std::fs::write(
        doctypes.join("local_note.toml"),
        "name = \"local_note\"\nlabel = \"Local note\"\n[[fields]]\nname = \"item\"\ntype = \"link\"\ntarget = \"extra_item\"\n",
    )
    .expect("write");
    let config = dir.join("forgebase.toml");
    std::fs::write(
        &config,
        format!(
            "[[sites]]\nname = \"a\"\nhosts = [\"a.localhost\"]\ndatabase_url_env = \"{A_URL}\"\n\n\
             [[sites]]\nname = \"b\"\nhosts = [\"b.localhost\"]\ndatabase_url_env = \"{B_URL}\"\napps = [\"base\"]\n"
        ),
    )
    .expect("write");
    let output = command_with(
        &config,
        &["serve", "--doctypes", doctypes.to_str().expect("path")],
        &[(A_URL, dead), (B_URL, dead)],
    )
    .output()
    .expect("run");
    let message = fails(&output, 1);
    assert!(
        message.starts_with("error: ")
            && message.contains("site `b`")
            && message.contains("extra_item"),
        "{message}"
    );
    assert!(
        !message.contains("connect") && !message.contains("s3cret"),
        "{message}"
    );
    std::fs::remove_dir_all(&dir).expect("cleanup");

    // `serve` with a site that cannot connect says which one, and listens on nothing.
    let output = run(&["serve"], "");
    let message = fails(&output, 1);
    assert!(message.starts_with("error: site `b`: "), "{message}");
    assert!(
        !message.contains("s3cret") && stdout(&output).is_empty(),
        "{message}"
    );
}
