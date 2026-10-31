//! TLS to PostgreSQL (M9.0, ADR 0018) against a real PostgreSQL 16 with TLS and a throwaway CA.
//!
//! Start the server with `.github/ci/pg-tls.sh up DIR` (see CONTRIBUTING.md); it prints the
//! variables these tests read: `FORGEBASE_TEST_TLS_URL`, `FORGEBASE_TEST_TLS_CA` and
//! `FORGEBASE_TEST_TLS_WRONG_CA`. When none of them is set a test prints a notice and passes,
//! except when `CI=true`: then it fails, so CI never skips these tests without saying so.

use forgebase_core::meta::Registry;
use forgebase_db::{
    Pool, SyncOptions, connect, create_database, inspect_database, migrate, pg_environment,
    quote_ident,
};

struct Tls {
    url: String,
    ca: String,
    wrong_ca: String,
}

fn tls() -> Option<Tls> {
    let get = |name: &str| std::env::var(name).ok().filter(|v| !v.is_empty());
    let vars = (
        get("FORGEBASE_TEST_TLS_URL"),
        get("FORGEBASE_TEST_TLS_CA"),
        get("FORGEBASE_TEST_TLS_WRONG_CA"),
    );
    match vars {
        (Some(url), Some(ca), Some(wrong_ca)) => Some(Tls { url, ca, wrong_ca }),
        _ if std::env::var("CI").is_ok_and(|v| v == "true") => panic!(
            "CI=true but FORGEBASE_TEST_TLS_URL, FORGEBASE_TEST_TLS_CA and \
             FORGEBASE_TEST_TLS_WRONG_CA are not all set: run .github/ci/pg-tls.sh up DIR first. \
             These tests are never skipped in CI."
        ),
        _ => {
            eprintln!(
                "SKIPPED: FORGEBASE_TEST_TLS_URL, FORGEBASE_TEST_TLS_CA and \
                 FORGEBASE_TEST_TLS_WRONG_CA are not set (.github/ci/pg-tls.sh up DIR)"
            );
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

impl Tls {
    /// The server URL with these query parameters.
    fn url(&self, query: &str) -> String {
        format!("{}?{query}", self.url)
    }

    fn verified(&self, mode: &str) -> String {
        self.url(&format!("sslmode={mode}&sslrootcert={}", encode(&self.ca)))
    }

    fn with_wrong_ca(&self) -> String {
        self.url(&format!(
            "sslmode=require&sslrootcert={}",
            encode(&self.wrong_ca)
        ))
    }
}

async fn uses_ssl(pool: &Pool) -> bool {
    let client = pool.get().await.expect("a connection");
    client
        .query_one(
            "SELECT ssl FROM pg_stat_ssl WHERE pid = pg_backend_pid()",
            &[],
        )
        .await
        .expect("pg_stat_ssl")
        .get(0)
}

/// The error of a connect that must fail: its text, and a check that it is about TLS.
async fn tls_failure(url: &str) -> String {
    let message = connect(url)
        .await
        .expect_err("must not connect")
        .to_string();
    assert!(message.contains("TLS"), "{message}");
    assert!(
        !message.contains("password") && !message.contains("28P01"),
        "{message}"
    );
    message
}

#[tokio::test]
async fn require_and_the_verify_modes_connect_over_tls_with_the_right_ca() {
    let Some(tls) = tls() else { return };
    for mode in ["require", "verify-ca", "verify-full"] {
        let pool = connect(&tls.verified(mode)).await.expect(mode);
        assert!(uses_ssl(&pool).await, "{mode}");
    }
}

#[tokio::test]
async fn the_pool_works_over_tls_with_several_connections() {
    let Some(tls) = tls() else { return };
    let pool = connect(&tls.verified("require")).await.expect("pool");
    let mut held = Vec::new();
    for _ in 0..4 {
        held.push(pool.get().await.expect("checkout"));
    }
    for client in &held {
        let ssl: bool = client
            .query_one(
                "SELECT ssl FROM pg_stat_ssl WHERE pid = pg_backend_pid()",
                &[],
            )
            .await
            .expect("query")
            .get(0);
        assert!(ssl);
    }
    drop(held);
    assert!(uses_ssl(&pool).await);
}

#[tokio::test]
async fn a_wrong_ca_fails_with_a_verification_error() {
    let Some(tls) = tls() else { return };
    let message = tls_failure(&tls.with_wrong_ca()).await;
    assert!(message.contains("UnknownIssuer"), "{message}");
}

#[tokio::test]
async fn a_host_name_that_is_not_in_the_certificate_fails() {
    let Some(tls) = tls() else { return };
    // The name `wrong.example` goes into the verification and `hostaddr` gives the address.
    let url = tls.url.replacen("localhost", "wrong.example", 1).replacen(
        "/postgres",
        "/postgres?hostaddr=127.0.0.1&",
        1,
    ) + &format!("sslmode=require&sslrootcert={}", encode(&tls.ca));
    let message = tls_failure(&url).await;
    assert!(message.contains("not valid for name"), "{message}");
}

#[tokio::test]
async fn an_ip_host_is_verified_against_the_ip_names_of_the_certificate() {
    let Some(tls) = tls() else { return };
    let url = tls
        .verified("require")
        .replacen("localhost", "127.0.0.1", 1);
    let pool = connect(&url)
        .await
        .expect("127.0.0.1 is in the certificate");
    assert!(uses_ssl(&pool).await);
}

#[tokio::test]
async fn disable_and_no_sslmode_connect_without_tls() {
    let Some(tls) = tls() else { return };
    for url in [tls.url("sslmode=disable"), tls.url.clone()] {
        let pool = connect(&url).await.expect("plain connection");
        assert!(!uses_ssl(&pool).await);
    }
}

#[tokio::test]
async fn prefer_unknown_modes_and_client_certificates_are_refused() {
    let Some(tls) = tls() else { return };
    for (query, expected) in [
        ("sslmode=prefer", "`sslmode=prefer` is not supported"),
        ("sslmode=allow", "`sslmode=prefer` is not supported"),
        ("sslmode=bogus", "not supported"),
        ("sslmode=require&sslcert=/x.pem", "`sslcert`"),
        ("sslmode=require&sslkey=/x.pem", "`sslkey`"),
    ] {
        let message = connect(&tls.url(query)).await.expect_err(query).to_string();
        assert!(message.contains(expected), "{query}: {message}");
        assert!(!message.contains("forgebase:forgebase"), "{message}");
    }
    assert!(
        create_database(&tls.url("sslmode=prefer"), None)
            .await
            .is_err()
    );
    assert!(inspect_database(&tls.url("sslmode=prefer")).await.is_err());
}

#[tokio::test]
async fn a_missing_root_file_is_a_clear_error_without_the_url() {
    let Some(tls) = tls() else { return };
    let message = connect(&tls.url("sslmode=require&sslrootcert=/nonexistent/ca.pem"))
        .await
        .expect_err("missing file")
        .to_string();
    assert!(message.contains("/nonexistent/ca.pem"), "{message}");
    assert!(!message.contains("forgebase:forgebase"), "{message}");
}

/// The system roots do not contain the throwaway CA, so `sslrootcert=system` (and the default)
/// must fail with a verification error against this server.
#[tokio::test]
async fn the_system_roots_do_not_trust_the_throwaway_ca() {
    let Some(tls) = tls() else { return };
    for query in ["sslmode=require", "sslmode=require&sslrootcert=system"] {
        let message = connect(&tls.url(query)).await.expect_err(query).to_string();
        // An image without CA certificates fails earlier, with its own clear message.
        assert!(
            message.contains("UnknownIssuer") || message.contains("trust store"),
            "{message}"
        );
    }
}

/// A database dropped (with `FORCE`) over TLS when the value goes away, also after a panic.
struct Scratch {
    url: String,
    name: String,
}

impl Drop for Scratch {
    fn drop(&mut self) {
        let (url, name) = (self.url.clone(), self.name.clone());
        let _ = std::thread::spawn(move || {
            let runtime = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .expect("runtime");
            runtime.block_on(async {
                if let Ok(pool) = connect(&url).await
                    && let Ok(client) = pool.get().await
                {
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

#[tokio::test]
async fn create_database_inspect_and_migrate_work_over_tls() {
    let Some(tls) = tls() else { return };
    let name = format!("fb_test_tls_{}", std::process::id());
    let query = format!("sslmode=require&sslrootcert={}", encode(&tls.ca));
    let maintenance_url = tls.url(&query);
    let _scratch = Scratch {
        url: maintenance_url.clone(),
        name: name.clone(),
    };
    let url = format!(
        "{}?{query}",
        tls.url.replacen("/postgres", &format!("/{name}"), 1)
    );

    // The maintenance connection of `create_database` uses TLS: with the wrong CA it fails.
    let wrong = tls.url.replacen("/postgres", &format!("/{name}"), 1);
    let wrong = format!(
        "{wrong}?sslmode=require&sslrootcert={}",
        encode(&tls.wrong_ca)
    );
    let message = create_database(&wrong, None)
        .await
        .expect_err("wrong CA")
        .to_string();
    assert!(
        message.contains("TLS") && message.contains("UnknownIssuer"),
        "{message}"
    );

    assert!(create_database(&url, None).await.expect("create"));
    assert!(!create_database(&url, None).await.expect("exists"));

    let info = inspect_database(&url).await.expect("inspect");
    assert_eq!(info.server_major(), 16);
    assert!(info.is_empty);

    let pool = connect(&url).await.expect("pool");
    assert!(uses_ssl(&pool).await);
    let report = migrate(&pool, &Registry::new(), &SyncOptions::default())
        .await
        .expect("migrate over TLS");
    assert!(!report.migrations.is_empty());
    drop(report);
    let client = pool.get().await.expect("connection");
    let count: i64 = client
        .query_one("SELECT count(*) FROM fb_migration", &[])
        .await
        .expect("fb_migration")
        .get(0);
    assert!(count > 0);
    drop(client);
    pool.close();
}

#[test]
fn the_backup_tools_get_the_matching_tls_environment() {
    // No server needed: the environment is a function of the URL.
    let env = pg_environment("postgres://u:pw@db.example.com/shop?sslmode=require").expect("env");
    assert!(env.contains(&("PGSSLMODE", "verify-full".to_owned())));
    assert!(env.contains(&("PGSSLROOTCERT", "system".to_owned())));
    let env = pg_environment("postgres://u@h/shop?sslmode=verify-ca&sslrootcert=/tmp/a%20b.pem")
        .expect("env");
    assert!(env.contains(&("PGSSLMODE", "verify-full".to_owned())));
    assert!(env.contains(&("PGSSLROOTCERT", "/tmp/a b.pem".to_owned())));
    let env = pg_environment("postgres://u@h/shop").expect("env");
    assert!(env.contains(&("PGSSLMODE", "disable".to_owned())));
    assert!(env.iter().all(|(name, _)| *name != "PGSSLROOTCERT"));
}

/// Neon's connection strings carry `channel_binding=require`; SCRAM channel binding needs the
/// connector to pass the certificate hash (`tokio-postgres-rustls` does).
#[tokio::test]
async fn scram_channel_binding_works_over_tls() {
    let Some(tls) = tls() else { return };
    let url = format!("{}&channel_binding=require", tls.verified("require"));
    let pool = connect(&url).await.expect("channel_binding=require");
    assert!(uses_ssl(&pool).await);
}

/// The role `qmark` (password `p?w`, a `?`) is accepted over TLS only (`pg-tls.sh`). A raw `?` in
/// the password hid `sslmode` from the parser once, and connected without TLS: now the raw form is
/// refused before any connection, and the encoded one (`p%3Fw`) connects over TLS.
#[tokio::test]
async fn a_question_mark_in_the_password_must_be_encoded_and_then_keeps_tls() {
    let Some(tls) = tls() else { return };
    let user = |url: String, password: &str| {
        url.replacen("forgebase:forgebase", &format!("qmark:{password}"), 1)
    };
    for url in [
        user(tls.verified("require"), "p%3Fw"),
        // The key written with a percent-encoded letter.
        user(
            tls.url(&format!(
                "%73slmode=require&sslrootcert={}",
                encode(&tls.ca)
            )),
            "p%3Fw",
        ),
    ] {
        let pool = connect(&url).await.expect("a TLS connection");
        assert!(uses_ssl(&pool).await);
    }
    // The raw `?` is refused with a clear message, not connected without TLS.
    for query in ["sslmode=require", "sslmode=disable"] {
        let message = connect(&user(tls.url(query), "p?w"))
            .await
            .expect_err("raw ?")
            .to_string();
        assert!(message.contains("before its first `@`"), "{message}");
    }
    // Without TLS the server rejects the role: the role really is TLS only.
    assert!(
        connect(&user(tls.url("sslmode=disable"), "p%3Fw"))
            .await
            .is_err()
    );
    // An encoded TLS option is refused by name, not ignored.
    let message = connect(&user(tls.url("sslmode=require&%73slcert=/x.pem"), "p%3Fw"))
        .await
        .expect_err("sslcert")
        .to_string();
    assert!(message.contains("`sslcert`"), "{message}");
}
