//! `site create`, `site backup` and `site restore` over TLS (M9.0, ADR 0018): the built
//! `forgebase` binary and the real `pg_dump` and `pg_restore` against the PostgreSQL with TLS of
//! `.github/ci/pg-tls.sh`. The site URLs say `sslmode=require` and `sslrootcert=<CA file>`, so the
//! tools get `PGSSLMODE=verify-full` and `PGSSLROOTCERT`.
//!
//! The variables are `FORGEBASE_TEST_TLS_URL` and `FORGEBASE_TEST_TLS_CA` (see CONTRIBUTING.md).
//! When they are not set the test prints a notice and passes, except when `CI=true`: then it
//! fails. The PostgreSQL 16 client tools are needed like in `backup.rs` (`FORGEBASE_PG_BIN`).

use std::path::PathBuf;
use std::process::{Command, Output, Stdio};

use forgebase_db::{connect, quote_ident};

const DOCTYPES: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/tests/fixtures/quickstart");

fn settings() -> Option<(String, String, String)> {
    let get = |name: &str| std::env::var(name).ok().filter(|v| !v.is_empty());
    match (
        get("FORGEBASE_TEST_TLS_URL"),
        get("FORGEBASE_TEST_TLS_CA"),
        get("FORGEBASE_TEST_TLS_WRONG_CA"),
    ) {
        (Some(url), Some(ca), Some(wrong)) => Some((url, ca, wrong)),
        _ if std::env::var("CI").is_ok_and(|v| v == "true") => panic!(
            "CI=true but FORGEBASE_TEST_TLS_URL, _CA and _WRONG_CA are not all set: run \
             .github/ci/pg-tls.sh up DIR first. These tests are never skipped in CI."
        ),
        _ => {
            eprintln!("SKIPPED: FORGEBASE_TEST_TLS_* are not set");
            None
        }
    }
}

fn encode(path: &str) -> String {
    path.bytes()
        .map(|b| match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'/' | b'.' | b'-' | b'_' => {
                (b as char).to_string()
            }
            _ => format!("%{b:02X}"),
        })
        .collect()
}

fn block_on<T>(future: impl std::future::Future<Output = T>) -> T {
    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("tokio runtime")
        .block_on(future)
}

/// Two database names and a directory, removed (the databases dropped over TLS) at the end.
struct World {
    maintenance_url: String,
    names: [String; 2],
    dir: PathBuf,
}

impl Drop for World {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.dir);
        let (url, names) = (self.maintenance_url.clone(), self.names.clone());
        let _ = std::thread::spawn(move || {
            block_on(async {
                let Ok(pool) = connect(&url).await else {
                    return;
                };
                let Ok(client) = pool.get().await else { return };
                for name in names {
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

fn stderr(output: &Output) -> String {
    String::from_utf8_lossy(&output.stderr).into_owned()
}

#[test]
fn site_create_backup_and_restore_work_over_tls() {
    let Some((admin_url, ca, wrong_ca)) = settings() else {
        return;
    };
    // The role `tls_only` is accepted over TLS only (see .github/ci/pg-tls.sh), so the dump and the
    // restore below prove that pg_dump and pg_restore used TLS.
    let url = admin_url.replacen("forgebase:forgebase", "tls_only:tls-only", 1);
    let id = std::process::id();
    let names = [
        format!("fb_test_tls_src_{id}"),
        format!("fb_test_tls_dst_{id}"),
    ];
    let query = format!("sslmode=require&sslrootcert={}", encode(&ca));
    let site_url = |name: &str| {
        format!(
            "{}?{query}",
            url.replacen("/postgres", &format!("/{name}"), 1)
        )
    };
    let dir = std::env::temp_dir().join(format!("fb_backup_tls_{id}"));
    std::fs::create_dir_all(&dir).expect("temporary directory");
    let world = World {
        maintenance_url: format!("{admin_url}?{query}"),
        names,
        dir: dir.clone(),
    };
    std::fs::write(
        dir.join("forgebase.toml"),
        "[[sites]]\nname = \"src\"\nhosts = [\"src.localhost\"]\ndatabase_url_env = \"FB_TEST_TLS_SRC_URL\"\n\n\
         [[sites]]\nname = \"dst\"\nhosts = [\"dst.localhost\"]\ndatabase_url_env = \"FB_TEST_TLS_DST_URL\"\n",
    )
    .expect("config");

    let run = |args: &[&str]| -> Output {
        Command::new(env!("CARGO_BIN_EXE_forgebase"))
            .arg("--config")
            .arg(dir.join("forgebase.toml"))
            .args(args)
            .env_remove("FORGEBASE_DATABASE_URL")
            .env_remove("FORGEBASE_CONFIG")
            .env("FB_TEST_TLS_SRC_URL", site_url(&world.names[0]))
            .env("FB_TEST_TLS_DST_URL", site_url(&world.names[1]))
            .stdin(Stdio::null())
            .output()
            .expect("run forgebase")
    };
    let ok = |args: &[&str]| -> String {
        let output = run(args);
        assert_eq!(
            output.status.code(),
            Some(0),
            "{args:?}: {}",
            stderr(&output)
        );
        String::from_utf8_lossy(&output.stdout).into_owned()
    };

    // `site create` connects to the maintenance database over TLS, then migrates over TLS.
    ok(&["site", "create", "src", "--doctypes", DOCTYPES]);
    ok(&[
        "doc",
        "insert",
        "customer",
        "--site",
        "src",
        "--doctypes",
        DOCTYPES,
        "--data",
        r#"{"email":"a@example.com","full_name":"Ann"}"#,
    ]);
    let before = ok(&[
        "doc",
        "list",
        "customer",
        "--site",
        "src",
        "--doctypes",
        DOCTYPES,
    ]);
    assert!(before.contains("a@example.com"), "{before}");

    // pg_dump and pg_restore verify the server with PGSSLMODE=verify-full and PGSSLROOTCERT.
    // pg_dump with the environment of a wrong-CA URL fails on the certificate, not on the login.
    let bin = std::env::var_os("FORGEBASE_PG_BIN").map(PathBuf::from);
    let pg_dump = bin.map_or_else(|| PathBuf::from("pg_dump"), |d| d.join("pg_dump"));
    let wrong_url = format!(
        "{}?sslmode=require&sslrootcert={}",
        url.replacen("/postgres", &format!("/{}", world.names[0]), 1),
        encode(&wrong_ca)
    );
    let mut dump = Command::new(pg_dump);
    dump.env_clear()
        .args(["--format=custom", "--file=/dev/null"]);
    for (name, value) in forgebase_db::pg_environment(&wrong_url).expect("environment") {
        dump.env(name, value);
    }
    let output = dump.output().expect("run pg_dump");
    assert!(!output.status.success());
    let message = stderr(&output);
    assert!(message.contains("certificate"), "{message}");

    let file = dir.join("src.dump");
    let file_arg = file.to_str().expect("utf-8 path");
    let out = ok(&["site", "backup", "src", "--out", file_arg]);
    assert!(out.starts_with("backed up site `src`"), "{out}");
    assert!(!out.contains("forgebase:forgebase"));
    ok(&["site", "create", "dst", "--empty"]);
    ok(&["site", "restore", "dst", "--from", file_arg]);
    let after = ok(&[
        "doc",
        "list",
        "customer",
        "--site",
        "dst",
        "--doctypes",
        DOCTYPES,
    ]);
    assert_eq!(after, before);
}
