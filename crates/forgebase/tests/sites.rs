//! Sites and the config file (M7.1, ADR 0014): the built `forgebase` binary against two real
//! PostgreSQL databases on one server, one per site.
//!
//! Every test creates its own throwaway `fb_test_sites_*` databases and fails loudly (never
//! skips) when `FORGEBASE_TEST_DATABASE_URL` is not set; see CONTRIBUTING.md.

use std::io::{BufRead, BufReader, Read, Write};
use std::path::PathBuf;
use std::process::{Child, Command, Output, Stdio};
use std::sync::atomic::{AtomicU32, Ordering};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use forgebase_db::{connect, quote_ident};

const ENV_VAR: &str = "FORGEBASE_TEST_DATABASE_URL";
const DOCTYPES: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/tests/fixtures/quickstart");
const A_URL: &str = "FB_TEST_SITE_A_URL";
const B_URL: &str = "FB_TEST_SITE_B_URL";
const PASSWORD_A: &str = "password of site a";
const PASSWORD_B: &str = "password of site b";

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

fn run_sql(url: &str, sql: String) {
    let url = url.to_owned();
    block_on(async move {
        let pool = connect(&url).await.expect("connect");
        let client = pool.get().await.expect("connection");
        client.batch_execute(&sql).await.expect("execute");
        pool.close();
    });
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
            "fb_test_sites_{:x}_{:x}_{}",
            std::process::id(),
            nanos,
            COUNTER.fetch_add(1, Ordering::Relaxed)
        );
        run_sql(
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

    /// The first column of the single result row, as text (`None` for SQL NULL).
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
            let _ = std::panic::catch_unwind(|| run_sql(&url, sql));
        })
        .join();
    }
}

/// Two sites, `a` (host `a.localhost`) and `b` (host `b.localhost`), each with its own database,
/// and a config file that lists them.
struct Pair {
    a: TestDb,
    b: TestDb,
    dir: PathBuf,
}

impl Pair {
    fn new() -> Self {
        static COUNTER: AtomicU32 = AtomicU32::new(0);
        let dir = std::env::temp_dir().join(format!(
            "fb_sites_{}_{}",
            std::process::id(),
            COUNTER.fetch_add(1, Ordering::Relaxed)
        ));
        std::fs::create_dir_all(&dir).expect("temporary directory");
        let pair = Self {
            a: TestDb::new(),
            b: TestDb::new(),
            dir,
        };
        std::fs::write(
            pair.config(),
            format!(
                "[[sites]]\nname = \"a\"\nhosts = [\"a.localhost\"]\ndatabase_url_env = \"{A_URL}\"\n\n\
                 [[sites]]\nname = \"b\"\nhosts = [\"b.localhost\", \"B2.localhost\"]\n\
                 database_url_env = \"{B_URL}\"\npool_size = 4\n"
            ),
        )
        .expect("write the config");
        pair
    }

    fn config(&self) -> PathBuf {
        self.dir.join("forgebase.toml")
    }

    fn command(&self, args: &[&str]) -> Command {
        let mut command = Command::new(env!("CARGO_BIN_EXE_forgebase"));
        command
            .arg("--config")
            .arg(self.config())
            .args(args)
            .env_remove("FORGEBASE_DATABASE_URL")
            .env_remove("FORGEBASE_CONFIG")
            .env_remove("FORGEBASE_ALLOWED_HOSTS")
            .env(A_URL, &self.a.url)
            .env(B_URL, &self.b.url)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        command
    }

    fn run(&self, args: &[&str]) -> Output {
        self.command(args).output().expect("run forgebase")
    }

    fn run_stdin(&self, args: &[&str], stdin: &str) -> Output {
        let mut command = self.command(args);
        command.stdin(Stdio::piped());
        let mut child = command.spawn().expect("start forgebase");
        child
            .stdin
            .take()
            .expect("stdin")
            .write_all(stdin.as_bytes())
            .expect("write stdin");
        child.wait_with_output().expect("run forgebase")
    }

    /// Migrates both sites and gives each an Administrator `root` with its own password.
    fn set_up(&self) {
        for (site, password) in [("a", PASSWORD_A), ("b", PASSWORD_B)] {
            let out = self.run(&["migrate", "--site", site, "--doctypes", DOCTYPES]);
            assert_eq!(code(&out), Some(0), "{}", stderr(&out));
            let out = self.run_stdin(
                &[
                    "user",
                    "create",
                    "root",
                    "--role",
                    "Administrator",
                    "--site",
                    site,
                ],
                &format!("{password}\n"),
            );
            assert_eq!(code(&out), Some(0), "{}", stderr(&out));
        }
    }
}

impl Drop for Pair {
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

#[test]
fn site_is_required_on_every_database_command_but_serve_with_two_sites() {
    let pair = Pair::new();
    for args in [
        &["migrate", "--doctypes", DOCTYPES][..],
        &["doc", "get", "customer", "x", "--doctypes", DOCTYPES],
        &["user", "disable", "root"],
        &["token", "revoke", "key"],
        &["job", "list"],
        &["worker", "--doctypes", DOCTYPES],
    ] {
        let output = pair.run(args);
        assert_eq!(code(&output), Some(2), "{args:?}: {}", stderr(&output));
        let message = stderr(&output);
        assert!(
            message.contains("--site NAME") && message.contains("a, b"),
            "{args:?}: {message}"
        );
        assert!(stdout(&output).is_empty());
    }
    // An unknown site lists the known ones.
    let output = pair.run(&["migrate", "--site", "nope", "--doctypes", DOCTYPES]);
    assert_eq!(code(&output), Some(2));
    assert!(
        stderr(&output).contains("unknown site `nope`") && stderr(&output).contains("a, b"),
        "{}",
        stderr(&output)
    );
    // Nothing was touched.
    for db in [&pair.a, &pair.b] {
        assert_eq!(db.scalar("SELECT to_regclass('fb_migration')::text"), None);
    }
}

#[test]
fn one_site_is_the_default_and_no_config_means_the_implicit_default_site() {
    let pair = Pair::new();
    let single = pair.dir.join("single.toml");
    std::fs::write(
        &single,
        format!("[[sites]]\nname = \"only\"\nhosts = [\"x.localhost\"]\ndatabase_url_env = \"{A_URL}\"\n"),
    )
    .expect("write");
    let output = Command::new(env!("CARGO_BIN_EXE_forgebase"))
        .args(["migrate", "--doctypes", DOCTYPES, "--config"])
        .arg(&single)
        .env(A_URL, &pair.a.url)
        .env_remove("FORGEBASE_DATABASE_URL")
        .output()
        .expect("run");
    assert_eq!(code(&output), Some(0), "{}", stderr(&output));
    assert_eq!(
        pair.a.scalar("SELECT to_regclass('fb_migration')::text"),
        Some("fb_migration".to_owned())
    );
    assert_eq!(
        pair.b.scalar("SELECT to_regclass('fb_migration')::text"),
        None
    );

    // Without a config file: the implicit site `default`, from FORGEBASE_DATABASE_URL.
    for site in [&[][..], &["--site", "default"]] {
        let output = Command::new(env!("CARGO_BIN_EXE_forgebase"))
            .args(["migrate", "--doctypes", DOCTYPES, "--dry-run"])
            .args(site)
            .env("FORGEBASE_DATABASE_URL", &pair.b.url)
            .env_remove("FORGEBASE_CONFIG")
            .output()
            .expect("run");
        assert_eq!(code(&output), Some(0), "{site:?}: {}", stderr(&output));
    }
    let output = Command::new(env!("CARGO_BIN_EXE_forgebase"))
        .args(["migrate", "--doctypes", DOCTYPES, "--site", "a"])
        .env("FORGEBASE_DATABASE_URL", &pair.b.url)
        .env_remove("FORGEBASE_CONFIG")
        .output()
        .expect("run");
    assert_eq!(code(&output), Some(2));
    assert!(
        stderr(&output).contains("only site is `default`"),
        "{}",
        stderr(&output)
    );
}

#[test]
fn the_config_comes_from_the_option_or_the_environment_and_the_option_wins() {
    let pair = Pair::new();
    // Only through the environment variable.
    let output = Command::new(env!("CARGO_BIN_EXE_forgebase"))
        .args([
            "migrate",
            "--site",
            "b",
            "--doctypes",
            DOCTYPES,
            "--dry-run",
        ])
        .env("FORGEBASE_CONFIG", pair.config())
        .env(B_URL, &pair.b.url)
        .env_remove("FORGEBASE_DATABASE_URL")
        .output()
        .expect("run");
    assert_eq!(code(&output), Some(0), "{}", stderr(&output));
    // A wrong path in the environment does not matter when the option is given.
    let output = Command::new(env!("CARGO_BIN_EXE_forgebase"))
        .arg("--config")
        .arg(pair.config())
        .args([
            "migrate",
            "--site",
            "b",
            "--doctypes",
            DOCTYPES,
            "--dry-run",
        ])
        .env("FORGEBASE_CONFIG", pair.dir.join("missing.toml"))
        .env(B_URL, &pair.b.url)
        .output()
        .expect("run");
    assert_eq!(code(&output), Some(0), "{}", stderr(&output));
    // A file that cannot be read is exit code 1, naming the path.
    let output = Command::new(env!("CARGO_BIN_EXE_forgebase"))
        .args(["migrate", "--doctypes", DOCTYPES, "--config"])
        .arg(pair.dir.join("missing.toml"))
        .output()
        .expect("run");
    assert_eq!(code(&output), Some(1));
    assert!(
        stderr(&output).contains("missing.toml"),
        "{}",
        stderr(&output)
    );
    // The current directory is never searched: a `forgebase.toml` there changes nothing.
    let output = Command::new(env!("CARGO_BIN_EXE_forgebase"))
        .current_dir(&pair.dir)
        .args(["migrate", "--site", "a", "--doctypes", DOCTYPES])
        .env("FORGEBASE_DATABASE_URL", &pair.b.url)
        .env_remove("FORGEBASE_CONFIG")
        .output()
        .expect("run");
    assert_eq!(code(&output), Some(2), "{}", stderr(&output));
}

#[test]
fn an_invalid_config_reports_every_problem_and_exits_1() {
    let pair = Pair::new();
    let bad = pair.dir.join("bad.toml");
    std::fs::write(
        &bad,
        "[[sites]]\nname = \"Bad\"\nhosts = [\"a.test:80\"]\ndatabase_url_env = \"X Y\"\n\n\
         [[sites]]\nname = \"ok\"\nhosts = [\"a.test:80\"]\ndatabase_url_env = \"OK_URL\"\npool_size = 0\n",
    )
    .expect("write");
    let output = Command::new(env!("CARGO_BIN_EXE_forgebase"))
        .args(["job", "list", "--site", "ok", "--config"])
        .arg(&bad)
        .env("FORGEBASE_DATABASE_URL", &pair.a.url)
        .output()
        .expect("run");
    assert_eq!(code(&output), Some(1));
    let message = stderr(&output);
    for expected in [
        "site `Bad`: the name must be",
        "site `Bad`: host \"a.test:80\"",
        "site `Bad`: `database_url_env` must be",
        "site `ok`: host \"a.test:80\"",
        "site `ok`: `pool_size` is 0",
    ] {
        assert!(message.contains(expected), "{expected}: {message}");
    }
    assert!(!message.contains(&pair.a.url), "{message}");
}

#[test]
fn an_unset_url_variable_is_an_error_naming_the_site_and_never_a_value() {
    let pair = Pair::new();
    let output = pair
        .command(&["job", "list", "--site", "b"])
        .env_remove(B_URL)
        .output()
        .expect("run");
    assert_eq!(code(&output), Some(1));
    let message = stderr(&output);
    assert!(
        message.contains("site `b`") && message.contains(B_URL),
        "{message}"
    );
    // The URL of the other site is in the environment, and is not printed.
    assert!(
        !message.contains(&pair.a.url) && !message.contains("forgebase:forgebase"),
        "{message}"
    );
    // An empty value is the same error.
    let output = pair
        .command(&["job", "list", "--site", "b"])
        .env(B_URL, "")
        .output()
        .expect("run");
    assert_eq!(code(&output), Some(1));
    assert!(stderr(&output).contains("site `b`"), "{}", stderr(&output));
}

#[test]
fn commands_touch_only_the_database_of_their_site() {
    let pair = Pair::new();
    let output = pair.run(&["migrate", "--site", "a", "--doctypes", DOCTYPES]);
    assert_eq!(code(&output), Some(0), "{}", stderr(&output));
    let migrations = "SELECT count(*)::text FROM fb_migration";
    assert_eq!(pair.a.scalar(migrations).as_deref(), Some("12"));
    assert_eq!(
        pair.b.scalar("SELECT to_regclass('fb_migration')::text"),
        None
    );
    assert_eq!(
        pair.b.scalar("SELECT to_regclass('doc_customer')::text"),
        None
    );
    let output = pair.run(&["migrate", "--site", "b", "--doctypes", DOCTYPES]);
    assert_eq!(code(&output), Some(0), "{}", stderr(&output));
    assert_eq!(pair.b.scalar(migrations).as_deref(), Some("12"));

    let customer = r#"{"email":"ann@example.com","full_name":"Ann"}"#;
    let output = pair.run(&[
        "doc",
        "insert",
        "customer",
        "--doctypes",
        DOCTYPES,
        "--site",
        "a",
        "--data",
        customer,
    ]);
    assert_eq!(code(&output), Some(0), "{}", stderr(&output));
    let count = "SELECT count(*)::text FROM doc_customer";
    assert_eq!(pair.a.scalar(count).as_deref(), Some("1"));
    assert_eq!(pair.b.scalar(count).as_deref(), Some("0"));
    // Read back on each site.
    let get = |site: &str| {
        pair.run(&[
            "doc",
            "get",
            "customer",
            "ann@example.com",
            "--doctypes",
            DOCTYPES,
            "--site",
            site,
        ])
    };
    assert_eq!(code(&get("a")), Some(0));
    assert_eq!(code(&get("b")), Some(1));
    // Users and tokens too: `ann` exists on A only.
    let output = pair.run_stdin(
        &["user", "create", "ann", "--site", "a"],
        "a password 123\n",
    );
    assert_eq!(code(&output), Some(0), "{}", stderr(&output));
    let output = pair.run(&["token", "create", "ann", "--site", "a"]);
    assert_eq!(code(&output), Some(0), "{}", stderr(&output));
    let output = pair.run(&["token", "create", "ann", "--site", "b"]);
    assert_eq!(code(&output), Some(1), "no user `ann` on site b");
    // And jobs: a job enqueued on A is not in the queue of B.
    let output = pair.run(&["job", "enqueue", "forgebase.purge_sessions", "--site", "a"]);
    assert_eq!(code(&output), Some(0), "{}", stderr(&output));
    assert_eq!(
        pair.a
            .scalar("SELECT count(*)::text FROM fb_job")
            .as_deref(),
        Some("1")
    );
    assert_eq!(
        pair.b
            .scalar("SELECT count(*)::text FROM fb_job")
            .as_deref(),
        Some("0")
    );
}

/// Reads a child's standard output to its end in the background, so a line it prints can never
/// block it or meet a closed pipe.
fn drain<R: std::io::Read + Send + 'static>(mut pipe: R) {
    std::thread::spawn(move || {
        let _ = std::io::copy(&mut pipe, &mut std::io::sink());
    });
}

/// A running `forgebase serve`, killed when dropped so a failing test leaves no process behind.
struct Server {
    child: Child,
    address: String,
    lines: Vec<String>,
}

impl Server {
    /// Starts `serve` (with the arguments after `serve`) on an ephemeral port and waits for its
    /// "listening on" line. `extra` are more environment variables.
    fn start(pair: &Pair, args: &[&str], extra: &[(&str, &str)]) -> Server {
        let mut full = vec!["serve", "--doctypes", DOCTYPES];
        full.extend_from_slice(args);
        let mut command = pair.command(&full);
        command.env("FORGEBASE_LISTEN", "127.0.0.1:0");
        for (name, value) in extra {
            command.env(name, value);
        }
        let mut child = command.spawn().expect("start forgebase serve");
        let mut reader = BufReader::new(child.stdout.take().expect("stdout"));
        let mut line = String::new();
        reader
            .read_line(&mut line)
            .expect("read the listening line");
        let address = line
            .trim()
            .strip_prefix("listening on http://")
            .unwrap_or_else(|| panic!("unexpected first line {line:?}"))
            .to_owned();
        // The lines after it, one per site, are printed before the server accepts requests; the
        // desk line is not configured here. Read as many as there are sites to serve.
        let sites = if args.contains(&"--site") { 1 } else { 2 };
        let lines = (0..sites)
            .map(|_| {
                let mut line = String::new();
                reader.read_line(&mut line).expect("read a site line");
                line.trim().to_owned()
            })
            .collect();
        // Anything printed later is read too, until the server stops.
        drain(reader);
        Server {
            child,
            address,
            lines,
        }
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

    /// Logs in as `root`; returns the status and the `name=value` of the cookie, if any.
    fn login(&self, host: &str, password: &str) -> (u16, Option<String>) {
        let (status, head, _) = self.request(
            "POST",
            "/api/v1/login",
            host,
            &["Content-Type: application/json"],
            &format!(r#"{{"username":"root","password":"{password}"}}"#),
        );
        let cookie = head
            .lines()
            .find_map(|l| l.strip_prefix("set-cookie: "))
            .map(|c| c.split(';').next().expect("pair").to_owned());
        (status, cookie)
    }

    /// Stops the server and returns what it wrote to standard error.
    fn stop(mut self) -> String {
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
fn serve_with_a_config_routes_by_host_and_keeps_the_sites_apart() {
    let pair = Pair::new();
    pair.set_up();
    // FORGEBASE_ALLOWED_HOSTS is ignored with a config file, with one warning.
    let server = Server::start(&pair, &[], &[("FORGEBASE_ALLOWED_HOSTS", "other.example")]);
    assert_eq!(
        server.lines,
        ["site a: a.localhost", "site b: b.localhost, b2.localhost"]
    );
    let get = |host: &str, cookie: Option<&str>| {
        let header = cookie.map(|c| format!("Cookie: {c}"));
        let headers: Vec<&str> = header.iter().map(String::as_str).collect();
        server.request("GET", "/api/v1/resource/customer", host, &headers, "")
    };

    // Each site has its own password for the same username.
    let (status, cookie_a) = server.login("a.localhost:8000", PASSWORD_A);
    assert_eq!(status, 200);
    let cookie_a = cookie_a.expect("cookie");
    assert_eq!(server.login("b.localhost", PASSWORD_A).0, 401);
    assert_eq!(server.login("a.localhost", PASSWORD_B).0, 401);
    let (status, cookie_b) = server.login("B2.localhost", PASSWORD_B);
    assert_eq!(status, 200);
    let cookie_b = cookie_b.expect("cookie");

    // A's session works on A and is refused on B, and the other way round.
    assert_eq!(get("a.localhost", Some(&cookie_a)).0, 200);
    assert_eq!(get("b.localhost", Some(&cookie_a)).0, 401);
    assert_eq!(get("b.localhost", Some(&cookie_b)).0, 200);
    assert_eq!(get("a.localhost", Some(&cookie_b)).0, 401);

    // A host that no site owns is a 404 with the envelope; a malformed one is a 400.
    let (status, head, body) = get("c.localhost", Some(&cookie_a));
    assert_eq!(status, 404, "{body}");
    assert!(body.contains("\"not_found\""), "{body}");
    assert!(head.contains("cache-control: no-store"), "{head}");
    assert_eq!(get("a.localhost:80@evil.example", None).0, 400);
    // The forwarded host is ignored.
    let (status, _, _) = server.request(
        "GET",
        "/api/v1/resource/customer",
        "c.localhost",
        &[
            "X-Forwarded-Host: a.localhost",
            &format!("Cookie: {cookie_a}"),
        ],
        "",
    );
    assert_eq!(status, 404);

    let errors = server.stop();
    assert_eq!(
        errors.matches("FORGEBASE_ALLOWED_HOSTS is ignored").count(),
        1,
        "{errors}"
    );
    assert!(
        !errors.contains(&pair.a.url) && !errors.contains(&pair.b.url),
        "{errors}"
    );
    assert!(
        !errors.contains(PASSWORD_A) && !errors.contains(PASSWORD_B),
        "{errors}"
    );
}

#[test]
fn serve_with_site_serves_only_that_site() {
    let pair = Pair::new();
    pair.set_up();
    let server = Server::start(&pair, &["--site", "b"], &[]);
    assert_eq!(server.lines, ["site b: b.localhost, b2.localhost"]);
    assert_eq!(server.login("b.localhost", PASSWORD_B).0, 200);
    // The host of the other site is unknown here: its database was never opened.
    assert_eq!(server.login("a.localhost", PASSWORD_A).0, 404);
    let errors = server.stop();
    assert!(!errors.contains("warning"), "{errors}");

    // A site whose variable is unset stops `serve` before it listens.
    let output = pair
        .command(&["serve", "--doctypes", DOCTYPES])
        .env_remove(A_URL)
        .output()
        .expect("run");
    assert_eq!(code(&output), Some(1));
    assert!(
        stderr(&output).contains("site `a`") && stdout(&output).is_empty(),
        "{}",
        stderr(&output)
    );
}

#[test]
fn a_worker_claims_only_from_the_database_of_its_site() {
    let pair = Pair::new();
    pair.set_up();
    let mut ids = Vec::new();
    for site in ["a", "b"] {
        let output = pair.run(&["job", "enqueue", "forgebase.purge_sessions", "--site", site]);
        assert_eq!(code(&output), Some(0), "{}", stderr(&output));
        let id = stdout(&output)
            .trim()
            .strip_prefix("enqueued job ")
            .expect("id")
            .to_owned();
        ids.push(id);
    }
    let status =
        |db: &TestDb, id: &str| db.scalar(&format!("SELECT status FROM fb_job WHERE id = '{id}'"));
    assert_eq!(status(&pair.a, &ids[0]).as_deref(), Some("queued"));
    assert_eq!(status(&pair.b, &ids[1]).as_deref(), Some("queued"));

    let mut child = pair
        .command(&["worker", "--site", "a", "--doctypes", DOCTYPES])
        .spawn()
        .expect("start the worker");
    drain(child.stdout.take().expect("stdout"));
    // Poll the real condition: A's job is done.
    let started = Instant::now();
    while status(&pair.a, &ids[0]).as_deref() != Some("done") {
        assert!(
            started.elapsed() < Duration::from_secs(30),
            "the worker did not run the job of site a"
        );
        std::thread::sleep(Duration::from_millis(100));
    }
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
    let exit = child.wait().expect("worker exit");
    assert_eq!(exit.code(), Some(0), "{log}");
    // B's job was never claimed: it is still queued, with no attempt.
    assert_eq!(status(&pair.b, &ids[1]).as_deref(), Some("queued"));
    assert_eq!(
        pair.b
            .scalar(&format!(
                "SELECT attempts::text FROM fb_job WHERE id = '{}'",
                ids[1]
            ))
            .as_deref(),
        Some("0")
    );
    assert!(!log.contains(&ids[1]), "{log}");
}

#[test]
fn a_webhook_of_one_site_enqueues_nothing_on_the_other() {
    // M9.2: a webhook is a row of the site's own database, so it belongs to that site alone.
    let pair = Pair::new();
    pair.set_up();
    let added = pair.run(&[
        "webhook",
        "add",
        "orders",
        "--doctype",
        "customer",
        "--events",
        "insert",
        "--url",
        "https://example.com/x",
        "--site",
        "a",
        "--doctypes",
        DOCTYPES,
    ]);
    assert_eq!(code(&added), Some(0), "{}", stderr(&added));
    // The other site has no such webhook.
    let listed_b = pair.run(&["webhook", "list", "--site", "b"]);
    assert_eq!(code(&listed_b), Some(0), "{}", stderr(&listed_b));
    assert_eq!(
        stdout(&listed_b).lines().count(),
        1,
        "only the header: {}",
        stdout(&listed_b)
    );
    assert!(stdout(&pair.run(&["webhook", "list", "--site", "a"])).contains("orders"));

    for site in ["a", "b"] {
        let inserted = pair.run(&[
            "doc",
            "insert",
            "customer",
            "--site",
            site,
            "--doctypes",
            DOCTYPES,
            "--data",
            r#"{"email":"x@example.com","full_name":"X"}"#,
        ]);
        assert_eq!(code(&inserted), Some(0), "{}", stderr(&inserted));
    }
    let jobs = "SELECT count(*)::text FROM fb_job WHERE name = 'forgebase.deliver_webhook'";
    assert_eq!(pair.a.scalar(jobs).as_deref(), Some("1"));
    assert_eq!(pair.b.scalar(jobs).as_deref(), Some("0"));
}
