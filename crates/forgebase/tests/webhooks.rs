//! Outgoing webhooks, end to end (M9.2, ADR 0020): a document operation enqueues a delivery, the
//! worker posts it to an in-test receiver, and the receiver checks the headers, the body and the
//! signature. Real PostgreSQL (`forgebase::testing::TestDb`), real sockets on `127.0.0.1`, real
//! TLS with a throwaway CA made by `openssl`.
//!
//! The webhooks here are rows inserted with SQL, so a test knows the secret and sets the two
//! development flags directly; the `webhook` commands have their own tests in `cli.rs`.
//!
//! Every test fails loudly (never skips) when `FORGEBASE_TEST_DATABASE_URL` is not set.

mod common;

use std::sync::OnceLock;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use common::{Pki, Receiver, Reply, expected_signature};
use forgebase::serde_json::{Value as Json, json};
use forgebase::testing::{TestDb, run_jobs, webhook_test_settings};
use forgebase::{App, DocTypeFile, Document, Forgebase, JobContext, JobFuture, JobHandler, Value};

const NOTE: &str = r#"
name = "note"
label = "Note"
[[fields]]
name = "title"
type = "data"
required = true

[[permissions]]
role = "Agent"
read = true
write = true
create = true
delete = true
"#;

const FILES: &[DocTypeFile] = &[DocTypeFile::new("doctypes/note.toml", NOTE)];

/// Inserts a note titled `from job`, as the documents handle of a job.
struct MakeNote;

impl JobHandler for MakeNote {
    fn run<'a>(&'a self, ctx: &'a mut JobContext<'_>) -> JobFuture<'a> {
        Box::pin(async move {
            let doc = Document {
                values: [("title".to_owned(), Value::Text("from job".to_owned()))].into(),
                ..Document::default()
            };
            ctx.documents().insert("note", doc).await?;
            Ok(())
        })
    }
}

fn binary() -> Forgebase {
    Forgebase::new().app(App::new("shop").doctypes(FILES).job("make_note", MakeNote))
}

/// The test CA. The first call also tells the webhook client to trust it and to give up after
/// 3 seconds, for the whole test process; call it first in every test.
fn pki() -> &'static Pki {
    static PKI: OnceLock<Pki> = OnceLock::new();
    PKI.get_or_init(|| {
        let pki = Pki::new("main");
        webhook_test_settings(&pki.ca_pem, Duration::from_secs(3));
        pki
    })
}

async fn ready() -> TestDb {
    pki();
    let db = TestDb::new().await;
    db.migrate(&binary(), false).await.expect("migrate");
    db
}

const SECRET: &str = "test-secret-do-not-print";

/// A webhook row with `SECRET`. `flags` is `(allow_private, insecure_http)`.
async fn add_hook(db: &TestDb, name: &str, events: &str, url: &str, flags: (bool, bool)) {
    db.exec(&format!(
        "INSERT INTO fb_webhook (name, doctype, events, url, secret, allow_private, insecure_http) \
         VALUES ('{name}', 'note', '{{{events}}}', '{url}', '{SECRET}', {}, {})",
        flags.0, flags.1
    ))
    .await;
}

/// A local plain-HTTP webhook: both development flags on.
async fn add_local(db: &TestDb, name: &str, url: &str) {
    add_hook(db, name, "insert", url, (true, true)).await;
}

async fn insert_note(db: &TestDb, binary: &Forgebase, title: &str) -> Json {
    db.create_user("ann", &["Agent"]).await;
    let ann = db.client(binary).login("ann").await;
    let reply = ann
        .post("/api/v1/resource/note", json!({"title": title}))
        .await;
    assert_eq!(reply.status, 201, "{:?}", reply.body);
    reply.body
}

/// `(status, attempts, last_error)` of the only delivery job.
async fn job(db: &TestDb) -> (String, String, String) {
    let mut rows = db
        .rows(
            "SELECT status, attempts::text, coalesce(last_error, '-') FROM fb_job \
             WHERE name = 'forgebase.deliver_webhook'",
        )
        .await;
    assert_eq!(rows.len(), 1, "{rows:?}");
    let row = rows.remove(0);
    (row[0].clone(), row[1].clone(), row[2].clone())
}

async fn make_due(db: &TestDb) {
    db.exec("UPDATE fb_job SET run_at = now() WHERE status = 'queued'")
        .await;
}

fn now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("after 1970")
        .as_secs()
}

/// What a failed job may show: never the secret, the path or the query of the URL.
fn assert_clean(text: &str) {
    for forbidden in [SECRET, "TOPSECRET", "hook-path", "token=", "sha256="] {
        assert!(!text.contains(forbidden), "`{forbidden}` in: {text}");
    }
}

// ---- the happy path ---------------------------------------------------------------------------

#[tokio::test]
async fn a_committed_document_is_delivered_signed() {
    let db = ready().await;
    let receiver = Receiver::http(Reply::Status(200));
    add_local(&db, "orders", &receiver.url("/hook-path?token=TOPSECRET")).await;
    let binary = binary();

    let created = insert_note(&db, &binary, "hello").await;
    assert!(
        receiver.requests().is_empty(),
        "nothing is sent by the request"
    );
    assert_eq!(job(&db).await.0, "queued");

    let run = run_jobs(&binary, &db).await;
    assert_eq!((run.done(), run.retried(), run.dead()), (1, 0, 0));
    assert_eq!(job(&db).await.0, "done");

    let requests = receiver.requests();
    assert_eq!(requests.len(), 1);
    let request = &requests[0];
    assert_eq!(
        request.request_line,
        "POST /hook-path?token=TOPSECRET HTTP/1.1"
    );
    assert_eq!(request.header("content-type"), Some("application/json"));
    assert!(
        request
            .header("user-agent")
            .is_some_and(|ua| ua.starts_with("Forgebase-Webhook/")),
        "{:?}",
        request.header("user-agent")
    );
    assert_eq!(request.header("x-forgebase-event"), Some("insert"));

    // The delivery id is the id of the job.
    let job_id = db.rows("SELECT id::text FROM fb_job").await[0][0].clone();
    assert_eq!(
        request.header("x-forgebase-delivery"),
        Some(job_id.as_str())
    );

    // The timestamp is the time of sending, in Unix seconds.
    let timestamp = request.header("x-forgebase-timestamp").expect("timestamp");
    let sent: u64 = timestamp.parse().expect("seconds");
    assert!(sent.abs_diff(now()) <= 30, "{sent}");

    // The signature, computed independently of the code under test.
    assert_eq!(
        request.header("x-forgebase-signature"),
        Some(expected_signature(SECRET, timestamp, &request.body).as_str())
    );

    // The body: the event, the document as the API shows it, and the delivery id.
    let body = request.json();
    assert_eq!(body["event"], "insert");
    assert_eq!(body["doctype"], "note");
    assert_eq!(body["name"], created["data"]["name"]);
    assert_eq!(body["delivery"], job_id);
    assert!(
        body["occurred_at"]
            .as_str()
            .is_some_and(|t| t.ends_with('Z'))
    );
    assert_eq!(body["data"], created["data"]);
    assert_eq!(body["data"]["title"], "hello");
    assert_eq!(body["data"]["version"], 1);
    assert_eq!(
        body.as_object()
            .expect("an object")
            .keys()
            .cloned()
            .collect::<Vec<_>>(),
        [
            "data",
            "delivery",
            "doctype",
            "event",
            "name",
            "occurred_at"
        ]
    );
    db.cleanup().await;
}

#[tokio::test]
async fn a_write_by_a_job_is_delivered_too() {
    let db = ready().await;
    let receiver = Receiver::http(Reply::Status(204));
    add_local(&db, "orders", &receiver.url("/x")).await;
    db.exec(
        "INSERT INTO fb_job (id, name, payload, status, enqueued_by) \
         VALUES (gen_random_uuid(), 'shop.make_note', '{}', 'queued', 'test')",
    )
    .await;
    let run = run_jobs(&binary(), &db).await;
    // The job itself, and the delivery it caused (run in the same call).
    assert_eq!((run.done(), run.retried(), run.dead()), (2, 0, 0));
    let requests = receiver.requests();
    assert_eq!(requests.len(), 1);
    assert_eq!(requests[0].json()["data"]["title"], "from job");
    assert_eq!(
        db.rows("SELECT enqueued_by FROM fb_job WHERE name = 'forgebase.deliver_webhook'")
            .await,
        [["job:shop.make_note"]]
    );
    db.cleanup().await;
}

#[tokio::test]
async fn the_update_and_delete_events_carry_their_documents() {
    let db = ready().await;
    let receiver = Receiver::http(Reply::Status(200));
    add_hook(
        &db,
        "all",
        "insert,update,delete",
        &receiver.url("/x"),
        (true, true),
    )
    .await;
    let binary = binary();
    db.create_user("ann", &["Agent"]).await;
    let ann = db.client(&binary).login("ann").await;
    let created = ann
        .post("/api/v1/resource/note", json!({"title": "one"}))
        .await
        .body;
    let name = created["data"]["name"].as_str().expect("name").to_owned();
    let path = format!("/api/v1/resource/note/{name}");
    let patched = ann
        .patch(&path, json!({"version": 1, "data": {"title": "two"}}))
        .await;
    assert_eq!(patched.status, 200, "{:?}", patched.body);
    let deleted = ann.delete(&format!("{path}?version=2")).await;
    assert_eq!(deleted.status, 204, "{:?}", deleted.body);

    let run = run_jobs(&binary, &db).await;
    assert_eq!(run.done(), 3);
    let bodies: Vec<Json> = receiver
        .requests()
        .iter()
        .map(common::Received::json)
        .collect();
    let summary: Vec<(String, String)> = bodies
        .iter()
        .map(|b| {
            (
                b["event"].as_str().unwrap_or_default().to_owned(),
                b["data"]["title"].as_str().unwrap_or_default().to_owned(),
            )
        })
        .collect();
    // Deliveries of different jobs may arrive in any order only if run concurrently; the worker
    // of `run_jobs` runs one job at a time in creation order.
    assert_eq!(
        summary,
        [
            ("insert".to_owned(), "one".to_owned()),
            ("update".to_owned(), "two".to_owned()),
            ("delete".to_owned(), "two".to_owned()),
        ]
    );
    db.cleanup().await;
}

#[tokio::test]
async fn a_huge_response_body_is_discarded() {
    let db = ready().await;
    let receiver = Receiver::http(Reply::Large(300_000));
    add_local(&db, "orders", &receiver.url("/x")).await;
    let binary = binary();
    insert_note(&db, &binary, "x").await;
    let run = run_jobs(&binary, &db).await;
    assert_eq!((run.done(), run.retried(), run.dead()), (1, 0, 0));
    db.cleanup().await;
}

// ---- retries ----------------------------------------------------------------------------------

#[tokio::test]
async fn the_delivery_id_is_the_same_on_a_retry() {
    let db = ready().await;
    let receiver = Receiver::http(Reply::Sequence(vec![
        Reply::Status(500),
        Reply::Status(200),
    ]));
    add_local(&db, "orders", &receiver.url("/hook-path?token=TOPSECRET")).await;
    let binary = binary();
    insert_note(&db, &binary, "again").await;

    let run = run_jobs(&binary, &db).await;
    assert_eq!((run.done(), run.retried(), run.dead()), (0, 1, 0));
    let (status, attempts, error) = job(&db).await;
    assert_eq!((status.as_str(), attempts.as_str()), ("queued", "1"));
    assert!(error.contains("HTTP 500"), "{error}");
    assert_clean(&error);

    make_due(&db).await;
    let run = run_jobs(&binary, &db).await;
    assert_eq!((run.done(), run.retried(), run.dead()), (1, 0, 0));

    let requests = receiver.requests();
    assert_eq!(requests.len(), 2);
    let ids: Vec<_> = requests
        .iter()
        .map(|r| {
            r.header("x-forgebase-delivery")
                .expect("delivery id")
                .to_owned()
        })
        .collect();
    assert_eq!(ids[0], ids[1], "the same delivery id on a retry");
    assert_eq!(
        requests[0].json()["delivery"],
        requests[1].json()["delivery"]
    );
    // Each attempt is signed afresh, with its own timestamp.
    for request in &requests {
        let timestamp = request.header("x-forgebase-timestamp").expect("timestamp");
        assert_eq!(
            request.header("x-forgebase-signature"),
            Some(expected_signature(SECRET, timestamp, &request.body).as_str())
        );
    }
    db.cleanup().await;
}

#[tokio::test]
async fn a_receiver_that_keeps_failing_ends_dead_after_five_attempts() {
    let db = ready().await;
    let receiver = Receiver::http(Reply::Status(500));
    add_local(&db, "orders", &receiver.url("/hook-path?token=TOPSECRET")).await;
    let binary = binary();
    insert_note(&db, &binary, "x").await;
    for attempt in 1..=5 {
        make_due(&db).await;
        let run = run_jobs(&binary, &db).await;
        let expected = if attempt < 5 { (0, 1, 0) } else { (0, 0, 1) };
        assert_eq!(
            (run.done(), run.retried(), run.dead()),
            expected,
            "attempt {attempt}"
        );
    }
    let (status, attempts, error) = job(&db).await;
    assert_eq!((status.as_str(), attempts.as_str()), ("dead", "5"));
    assert!(error.contains("HTTP 500"), "{error}");
    assert_clean(&error);
    assert_eq!(receiver.requests().len(), 5);
    db.cleanup().await;
}

#[tokio::test]
async fn a_receiver_that_never_answers_is_a_retryable_timeout() {
    let db = ready().await;
    let receiver = Receiver::http(Reply::Hang);
    add_local(&db, "orders", &receiver.url("/hook-path")).await;
    let binary = binary();
    insert_note(&db, &binary, "x").await;
    let started = Instant::now();
    let run = run_jobs(&binary, &db).await;
    assert_eq!((run.done(), run.retried(), run.dead()), (0, 1, 0));
    assert!(
        started.elapsed() < Duration::from_secs(15),
        "{:?}",
        started.elapsed()
    );
    let (status, _, error) = job(&db).await;
    assert_eq!(status, "queued");
    assert!(error.contains("did not answer within 3 s"), "{error}");
    assert_clean(&error);
    assert_eq!(
        receiver.requests().len(),
        1,
        "the request arrived; no answer came"
    );
    db.cleanup().await;
}

#[tokio::test]
async fn a_closed_port_is_a_retryable_connection_error() {
    let db = ready().await;
    // A port that was free a moment ago and has no listener.
    let port = {
        let probe = std::net::TcpListener::bind("127.0.0.1:0").expect("bind");
        probe.local_addr().expect("address").port()
    };
    add_local(&db, "orders", &format!("http://127.0.0.1:{port}/hook-path")).await;
    let binary = binary();
    insert_note(&db, &binary, "x").await;
    let run = run_jobs(&binary, &db).await;
    assert_eq!((run.done(), run.retried(), run.dead()), (0, 1, 0));
    let (_, _, error) = job(&db).await;
    assert!(error.contains("cannot connect"), "{error}");
    assert_clean(&error);
    db.cleanup().await;
}

// ---- permanent failures -----------------------------------------------------------------------

#[tokio::test]
async fn a_redirect_is_permanent_and_is_not_followed() {
    let db = ready().await;
    let target = Receiver::http(Reply::Status(200));
    let receiver = Receiver::http(Reply::Redirect(302, target.url("/elsewhere")));
    add_local(&db, "orders", &receiver.url("/hook-path")).await;
    let binary = binary();
    insert_note(&db, &binary, "x").await;
    let run = run_jobs(&binary, &db).await;
    assert_eq!((run.done(), run.retried(), run.dead()), (0, 0, 1));
    let (status, attempts, error) = job(&db).await;
    assert_eq!((status.as_str(), attempts.as_str()), ("dead", "1"));
    assert!(error.contains("redirects are not followed"), "{error}");
    assert!(error.contains("302"), "{error}");
    assert_eq!(receiver.requests().len(), 1);
    assert!(
        target.requests().is_empty(),
        "the redirect was not followed"
    );
    db.cleanup().await;
}

#[tokio::test]
async fn addresses_that_are_not_allowed_are_refused_for_good_and_nothing_is_sent() {
    let db = ready().await;
    let receiver = Receiver::http(Reply::Status(200));
    let port = receiver.addr.port();
    // Both flags on would allow everything: `insecure_http` is on only to use http://, and
    // `allow_private` is off, which is the point.
    let urls = [
        (
            "loopback",
            format!("http://127.0.0.1:{port}/hook-path?token=TOPSECRET"),
        ),
        (
            "a name that resolves to loopback",
            format!("http://localhost:{port}/hook-path"),
        ),
        ("private", "http://10.1.2.3/hook-path".to_owned()),
        (
            "private 192.168",
            "http://192.168.0.10:8080/hook-path".to_owned(),
        ),
        ("link-local", "http://169.254.7.7/hook-path".to_owned()),
        (
            "metadata",
            "http://169.254.169.254/latest/meta-data/hook-path".to_owned(),
        ),
        ("ipv6 loopback", format!("http://[::1]:{port}/hook-path")),
        (
            "ipv4-mapped ipv6",
            format!("http://[::ffff:127.0.0.1]:{port}/hook-path"),
        ),
    ];
    let binary = binary();
    for (what, url) in urls {
        db.exec("DELETE FROM fb_job; DELETE FROM fb_webhook").await;
        add_hook(&db, "w", "insert", &url, (false, true)).await;
        db.exec("INSERT INTO fb_job (id, name, payload, status, enqueued_by) \
                 VALUES (gen_random_uuid(), 'forgebase.deliver_webhook', \
                 '{\"webhook\": \"w\", \"event\": \"insert\", \"doctype\": \"note\", \"name\": \"n\", \"data\": {}, \"occurred_at\": \"2026-01-01T00:00:00Z\"}', \
                 'queued', 'test')").await;
        let run = run_jobs(&binary, &db).await;
        assert_eq!((run.done(), run.retried(), run.dead()), (0, 0, 1), "{what}");
        let (status, attempts, error) = job(&db).await;
        assert_eq!(
            (status.as_str(), attempts.as_str()),
            ("dead", "1"),
            "{what}"
        );
        assert!(error.contains("--allow-private"), "{what}: {error}");
        assert_clean(&error);
    }
    assert!(
        receiver.requests().is_empty(),
        "nothing reached the receiver: {:?}",
        receiver.requests()
    );
    db.cleanup().await;
}

#[tokio::test]
async fn http_without_the_insecure_flag_is_refused_at_send_time() {
    let db = ready().await;
    let receiver = Receiver::http(Reply::Status(200));
    // A row written around the CLI: allow_private on, insecure_http off.
    add_hook(
        &db,
        "orders",
        "insert",
        &receiver.url("/hook-path"),
        (true, false),
    )
    .await;
    let binary = binary();
    insert_note(&db, &binary, "x").await;
    let run = run_jobs(&binary, &db).await;
    assert_eq!((run.done(), run.retried(), run.dead()), (0, 0, 1));
    let (_, _, error) = job(&db).await;
    assert!(error.contains("--insecure-http"), "{error}");
    assert_clean(&error);
    assert!(receiver.requests().is_empty());
    db.cleanup().await;
}

#[tokio::test]
async fn a_url_with_user_info_is_refused_at_send_time() {
    let db = ready().await;
    let receiver = Receiver::http(Reply::Status(200));
    let url = format!(
        "http://user:pw@127.0.0.1:{}/hook-path",
        receiver.addr.port()
    );
    add_hook(&db, "orders", "insert", &url, (true, true)).await;
    let binary = binary();
    insert_note(&db, &binary, "x").await;
    let run = run_jobs(&binary, &db).await;
    assert_eq!((run.done(), run.retried(), run.dead()), (0, 0, 1));
    let (_, _, error) = job(&db).await;
    assert!(error.contains("user info"), "{error}");
    assert!(!error.contains("pw"), "{error}");
    assert!(receiver.requests().is_empty());
    db.cleanup().await;
}

// ---- removed, disabled -------------------------------------------------------------------------

#[tokio::test]
async fn a_webhook_removed_or_disabled_before_delivery_sends_nothing_and_completes() {
    for how in [
        "DELETE FROM fb_webhook",
        "UPDATE fb_webhook SET enabled = false",
    ] {
        let db = ready().await;
        let receiver = Receiver::http(Reply::Status(200));
        add_local(&db, "orders", &receiver.url("/hook-path")).await;
        let binary = binary();
        insert_note(&db, &binary, "x").await;
        assert_eq!(job(&db).await.0, "queued");
        db.exec(how).await;
        let run = run_jobs(&binary, &db).await;
        assert_eq!((run.done(), run.retried(), run.dead()), (1, 0, 0), "{how}");
        assert_eq!(job(&db).await.0, "done", "{how}");
        assert!(receiver.requests().is_empty(), "{how}");
        db.cleanup().await;
    }
}

// ---- TLS -------------------------------------------------------------------------------------

#[tokio::test]
async fn a_delivery_over_https_verifies_the_certificate_of_the_test_ca() {
    let pki = pki();
    let db = ready().await;
    let receiver = Receiver::https(Reply::Status(200), pki);
    // By IP address and by name: the certificate has both, and the name is used for the SNI and
    // the verification while the connection goes to the vetted address.
    add_hook(
        &db,
        "by_ip",
        "insert",
        &receiver.url("/hook-path"),
        (true, false),
    )
    .await;
    add_hook(
        &db,
        "by_name",
        "insert",
        &receiver.localhost_url("/hook-path"),
        (true, false),
    )
    .await;
    let binary = binary();
    insert_note(&db, &binary, "secure").await;
    let run = run_jobs(&binary, &db).await;
    assert_eq!((run.done(), run.retried(), run.dead()), (2, 0, 0));
    let requests = receiver.requests();
    assert_eq!(requests.len(), 2);
    for request in &requests {
        let timestamp = request.header("x-forgebase-timestamp").expect("timestamp");
        assert_eq!(
            request.header("x-forgebase-signature"),
            Some(expected_signature(SECRET, timestamp, &request.body).as_str())
        );
        assert_eq!(request.json()["data"]["title"], "secure");
    }
    db.cleanup().await;
}

#[tokio::test]
async fn a_certificate_of_another_authority_is_a_retryable_tls_error() {
    pki();
    let rogue = Pki::new("rogue");
    let db = ready().await;
    let receiver = Receiver::https(Reply::Status(200), &rogue);
    add_hook(
        &db,
        "orders",
        "insert",
        &receiver.url("/hook-path"),
        (true, false),
    )
    .await;
    let binary = binary();
    insert_note(&db, &binary, "x").await;
    let run = run_jobs(&binary, &db).await;
    assert_eq!((run.done(), run.retried(), run.dead()), (0, 1, 0));
    let (_, _, error) = job(&db).await;
    assert!(error.contains("TLS"), "{error}");
    assert_clean(&error);
    assert!(
        receiver.requests().is_empty(),
        "no request was sent over the failed handshake"
    );
    db.cleanup().await;
}

// ---- sites -----------------------------------------------------------------------------------

#[tokio::test]
async fn a_webhook_of_one_site_does_not_fire_for_another() {
    // A site is a database: the webhook table of site A is not in site B.
    let site_a = ready().await;
    let site_b = ready().await;
    let receiver = Receiver::http(Reply::Status(200));
    add_local(&site_a, "orders", &receiver.url("/x")).await;
    let binary = binary();

    // A write on site B enqueues nothing and sends nothing.
    insert_note(&site_b, &binary, "on b").await;
    assert!(site_b.rows("SELECT 1 FROM fb_job").await.is_empty());
    let run = run_jobs(&binary, &site_b).await;
    assert_eq!(run.done(), 0);
    assert!(receiver.requests().is_empty());

    // The same write on site A does.
    insert_note(&site_a, &binary, "on a").await;
    let run = run_jobs(&binary, &site_a).await;
    assert_eq!(run.done(), 1);
    assert_eq!(receiver.requests().len(), 1);
    assert_eq!(receiver.requests()[0].json()["data"]["title"], "on a");
    site_a.cleanup().await;
    site_b.cleanup().await;
}
