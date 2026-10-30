//! Query reports over HTTP (M8.4, ADR 0016): the list and the run route with each status code,
//! authentication, CSRF, the envelopes, the role gate, the parameter errors, the timeout, the
//! read-only refusal, the titles of Link columns and the fact that the SQL is never sent.

mod common;

use std::time::Duration;

use axum::http::StatusCode;
use common::{Env, Reply, Session, get, post};
use forgebase_core::report::{
    ColumnKind, DEFAULT_MAX_ROWS, DEFAULT_TIMEOUT, ParamKind, ReportColumn, ReportDef, ReportParam,
};
use forgebase_http::AppConfig;
use serde_json::{Value as Json, json};

const SALES_SQL: &str = "SELECT customer, orders, total, day FROM sales \
     WHERE day >= $1 AND day <= $2 AND ($3::text IS NULL OR customer = $3) \
     AND ($4::text IS NULL OR kind = $4) ORDER BY total DESC";

fn param(name: &str, kind: ParamKind, required: bool) -> ReportParam {
    ReportParam {
        name: name.to_owned(),
        label: name.to_owned(),
        kind,
        required,
        default: None,
    }
}

fn column(name: &str, kind: ColumnKind) -> ReportColumn {
    ReportColumn {
        name: name.to_owned(),
        label: name.to_owned(),
        kind,
    }
}

fn report(name: &str, label: &str, roles: &[&str], sql: &str) -> ReportDef {
    ReportDef {
        app: "t".to_owned(),
        name: name.to_owned(),
        label: label.to_owned(),
        sql: sql.to_owned(),
        roles: roles.iter().map(|r| (*r).to_owned()).collect(),
        params: vec![],
        columns: vec![column("n", ColumnKind::Int)],
        timeout: DEFAULT_TIMEOUT,
        max_rows: DEFAULT_MAX_ROWS,
    }
}

fn reports() -> Vec<ReportDef> {
    let mut by_customer = report(
        "by_customer",
        "Sales by customer",
        &["Sales Manager"],
        SALES_SQL,
    );
    by_customer.params = vec![
        param("from", ParamKind::Date, true),
        param("to", ParamKind::Date, true),
        param(
            "customer",
            ParamKind::Link {
                doctype: "ledger".to_owned(),
            },
            false,
        ),
        param(
            "kind",
            ParamKind::Select {
                options: vec!["retail".to_owned(), "online".to_owned()],
            },
            false,
        ),
    ];
    by_customer.columns = vec![
        column(
            "customer",
            ColumnKind::Link {
                doctype: "ledger".to_owned(),
            },
        ),
        column("orders", ColumnKind::Int),
        column("total", ColumnKind::Decimal),
        column("day", ColumnKind::Date),
    ];
    by_customer.max_rows = 3;
    let mut slow = report(
        "slow",
        "A slow one",
        &["Sales Manager"],
        "SELECT pg_sleep(10)::text",
    );
    slow.columns = vec![column("s", ColumnKind::Data)];
    slow.timeout = Duration::from_millis(200);
    let mut writer = report(
        "writer",
        "Writer",
        &["Sales Manager"],
        "UPDATE sales SET total = 0 RETURNING orders",
    );
    writer.columns = vec![column("orders", ColumnKind::Int)];
    let mut audit = report("audit", "Audit", &["Auditor"], "SELECT 1 FROM sales");
    audit.columns = vec![column("n", ColumnKind::Int)];
    vec![by_customer, slow, writer, audit]
}

struct Setup {
    env: Env,
    mgr: Session,
    reader: Session,
    clerk: Session,
    root: Session,
    ledger: String,
}

async fn setup() -> Setup {
    let env = Env::with_reports(&[], reports()).await;
    env.db
        .exec("CREATE TABLE sales (customer text, orders int, total numeric, day date, kind text)")
        .await;
    env.user("mgr", &["Sales Manager", "Keeper"]).await;
    // Runs reports but may not read the ledger: no titles for them.
    env.user("reader", &["Sales Manager"]).await;
    env.user("clerk", &["Clerk"]).await;
    env.user("root", &["Administrator"]).await;
    env.user("audit", &["Auditor"]).await;
    let (mgr, reader, clerk, root) = (
        env.login("mgr").await,
        env.login("reader").await,
        env.login("clerk").await,
        env.login("root").await,
    );
    let ledger = env
        .admin_insert(&root, "ledger", json!({"title": "Acme Ltd"}))
        .await;
    env.db
        .exec(&format!(
            "INSERT INTO sales VALUES ('{ledger}', 3, 10.50, '2026-10-02', 'retail'), \
             ('{ledger}', 1, 99.99, '2026-10-20', 'online'), \
             ('other', 7, 20, '2026-10-10', 'retail'), \
             ('other', 2, 5, '2026-10-12', 'retail'), \
             ('late', 9, 1, '2026-12-01', 'retail')"
        ))
        .await;
    Setup {
        env,
        mgr,
        reader,
        clerk,
        root,
        ledger,
    }
}

async fn run(s: &Setup, who: &Session, name: &str, body: Json) -> Reply {
    s.env
        .send(
            post(&format!("/api/v1/report/{name}"))
                .session(who)
                .json(body),
        )
        .await
}

fn october() -> Json {
    json!({"params": {"from": "2026-10-01", "to": "2026-10-31"}})
}

async fn sales_unchanged(s: &Setup) {
    let client = s.env.db.pool.get().await.expect("connection");
    let row = client
        .query_one("SELECT sum(total)::text FROM sales", &[])
        .await
        .expect("sum");
    assert_eq!(row.get::<_, String>(0), "136.49");
}

#[tokio::test]
async fn the_list_has_the_reports_the_caller_may_run_by_label_and_never_the_sql() {
    let s = setup().await;
    let reply = s.env.send(get("/api/v1/report").session(&s.mgr)).await;
    assert_eq!(reply.status, StatusCode::OK, "{}", reply.text);
    let data = reply.data();
    let names: Vec<&str> = data
        .as_array()
        .expect("list")
        .iter()
        .map(|r| r["name"].as_str().expect("name"))
        .collect();
    // By label in byte order: "A slow one" < "Sales by customer" < "Writer"; the Auditor's
    // report is not listed.
    assert_eq!(names, ["t.slow", "t.by_customer", "t.writer"]);
    let first = &data[1];
    assert_eq!(first["app"], "t");
    assert_eq!(first["label"], "Sales by customer");
    assert_eq!(
        first["params"],
        json!([
            {"name": "from", "label": "from", "kind": "date", "required": true, "default": null},
            {"name": "to", "label": "to", "kind": "date", "required": true, "default": null},
            {"name": "customer", "label": "customer", "kind": "link", "required": false, "default": null, "doctype": "ledger"},
            {"name": "kind", "label": "kind", "kind": "select", "required": false, "default": null, "options": ["retail", "online"]},
        ])
    );
    assert_eq!(
        first["columns"],
        json!([
            {"name": "customer", "label": "customer", "kind": "link", "doctype": "ledger"},
            {"name": "orders", "label": "orders", "kind": "int"},
            {"name": "total", "label": "total", "kind": "decimal"},
            {"name": "day", "label": "day", "kind": "date"},
        ])
    );
    assert!(first.get("sql").is_none() && first.get("roles").is_none());
    // Administrator sees every report; a user without a report role sees none.
    let all = s.env.send(get("/api/v1/report").session(&s.root)).await;
    assert_eq!(all.data().as_array().expect("list").len(), 4);
    let none = s.env.send(get("/api/v1/report").session(&s.clerk)).await;
    assert_eq!(none.status, StatusCode::OK);
    assert_eq!(none.data(), json!([]));
}

#[tokio::test]
async fn a_report_runs_with_typed_params_and_returns_columns_rows_and_the_cut_flag() {
    let s = setup().await;
    let reply = run(&s, &s.mgr, "t.by_customer", october()).await;
    assert_eq!(reply.status, StatusCode::OK, "{}", reply.text);
    let body = reply.json();
    assert!(body.get("link_titles").is_none(), "{body}");
    let data = &body["data"];
    assert_eq!(data["columns"].as_array().expect("columns").len(), 4);
    assert_eq!(
        data["rows"],
        json!([
            [s.ledger, 1, "99.99", "2026-10-20"],
            ["other", 7, "20", "2026-10-10"],
            [s.ledger, 3, "10.50", "2026-10-02"],
        ])
    );
    // 4 rows match, the limit is 3.
    assert_eq!(data["truncated"], true);
    // Optional params narrow the result; null is the same as absent.
    let one = run(
        &s,
        &s.mgr,
        "t.by_customer",
        json!({"params": {"from": "2026-10-01", "to": "2026-10-31", "customer": "other", "kind": null}}),
    )
    .await;
    assert_eq!(one.data()["rows"].as_array().expect("rows").len(), 2);
    assert_eq!(one.data()["truncated"], false);
    let kind = run(
        &s,
        &s.mgr,
        "t.by_customer",
        json!({"params": {"from": "2026-10-01", "to": "2026-10-31", "kind": "online"}}),
    )
    .await;
    assert_eq!(kind.data()["rows"].as_array().expect("rows").len(), 1);
    // An Administrator runs it too; the body may leave `params` out when none is required.
    let admin = run(&s, &s.root, "t.by_customer", october()).await;
    assert_eq!(admin.status, StatusCode::OK);
}

#[tokio::test]
async fn link_titles_follow_the_read_rules_of_the_caller() {
    let s = setup().await;
    // `ledger` has a title field; `Keeper` reads ledgers, so the manager gets the title.
    let path = "t.by_customer?link_titles=1";
    let titled = run(&s, &s.mgr, path, october()).await;
    assert_eq!(titled.status, StatusCode::OK, "{}", titled.text);
    assert_eq!(
        titled.json()["link_titles"],
        json!({"ledger": {s.ledger.clone(): "Acme Ltd"}})
    );
    // The reader runs the report but may not read ledgers: the rows, and no titles.
    let plain = run(&s, &s.reader, path, october()).await;
    assert_eq!(plain.status, StatusCode::OK);
    assert_eq!(plain.data()["rows"].as_array().expect("rows").len(), 3);
    assert_eq!(plain.json()["link_titles"], json!({}));
    let bad = run(&s, &s.mgr, "t.by_customer?link_titles=2", october()).await;
    assert_eq!(bad.status, StatusCode::BAD_REQUEST);
    assert_eq!(bad.code(), "invalid_query");
}

#[tokio::test]
async fn a_report_the_caller_may_not_run_is_404_like_one_that_does_not_exist() {
    let s = setup().await;
    let denied = run(&s, &s.clerk, "t.by_customer", october()).await;
    let unknown = run(&s, &s.clerk, "t.nothing", october()).await;
    let unknown_admin = run(&s, &s.root, "t.nothing", october()).await;
    for reply in [&denied, &unknown, &unknown_admin] {
        assert_eq!(reply.status, StatusCode::NOT_FOUND, "{}", reply.text);
        assert_eq!(reply.code(), "not_found");
    }
    // The same body: nothing tells that the report exists.
    assert_eq!(denied.text, unknown.text);
    // Not even the parameter errors of a report the caller may not run.
    let bad = run(
        &s,
        &s.clerk,
        "t.by_customer",
        json!({"params": {"nope": 1}}),
    )
    .await;
    assert_eq!(bad.text, unknown.text);
    for name in ["by_customer", "t.", ".by_customer", "t.by_customer.x"] {
        assert_eq!(
            run(&s, &s.mgr, name, october()).await.status,
            StatusCode::NOT_FOUND,
            "{name}"
        );
    }
}

#[tokio::test]
async fn parameter_problems_are_422_with_a_detail_per_parameter() {
    let s = setup().await;
    // Wrong kinds are found while the body is mapped.
    let kinds = run(
        &s,
        &s.mgr,
        "t.by_customer",
        json!({"params": {"from": "yesterday", "to": 5, "customer": 7}}),
    )
    .await;
    assert_eq!(
        kinds.status,
        StatusCode::UNPROCESSABLE_ENTITY,
        "{}",
        kinds.text
    );
    assert_eq!(kinds.code(), "validation_failed");
    let details = kinds.json()["error"]["details"].clone();
    let details: Vec<&str> = details
        .as_array()
        .expect("details")
        .iter()
        .map(|d| d.as_str().expect("text"))
        .collect();
    assert_eq!(details.len(), 3, "{details:?}");
    assert!(
        details.iter().any(|d| d.starts_with("field `from`:")),
        "{details:?}"
    );
    assert!(
        details.iter().any(|d| d.starts_with("field `to`:")),
        "{details:?}"
    );
    assert!(
        details.iter().any(|d| d.starts_with("field `customer`:")),
        "{details:?}"
    );
    // Missing, unknown and not an option are found by the checks of the report.
    let rules = run(
        &s,
        &s.mgr,
        "t.by_customer",
        json!({"params": {"kind": "wholesale", "extra": true}}),
    )
    .await;
    assert_eq!(rules.status, StatusCode::UNPROCESSABLE_ENTITY);
    assert_eq!(
        rules.json()["error"]["details"],
        json!([
            "unknown field `extra`",
            "field `from` is required",
            "field `to` is required",
            "field `kind`: `wholesale` is not one of the allowed options",
        ])
    );
}

#[tokio::test]
async fn a_hostile_param_is_data_and_the_table_survives() {
    let s = setup().await;
    let reply = run(
        &s,
        &s.mgr,
        "t.by_customer",
        json!({"params": {"from": "2026-10-01", "to": "2026-10-31", "customer": "x'); DROP TABLE sales; --"}}),
    )
    .await;
    assert_eq!(reply.status, StatusCode::OK, "{}", reply.text);
    assert_eq!(reply.data()["rows"], json!([]));
    sales_unchanged(&s).await;
}

#[tokio::test]
async fn bad_bodies_and_queries_are_refused_with_the_envelope() {
    let s = setup().await;
    let cases: [(Json, &str); 3] = [
        (json!([]), "bad_request"),
        (json!({"parms": {}}), "bad_request"),
        (json!({"params": [1]}), "bad_request"),
    ];
    for (body, code) in cases {
        let reply = run(&s, &s.mgr, "t.by_customer", body.clone()).await;
        assert_eq!(
            reply.status,
            StatusCode::BAD_REQUEST,
            "{body}: {}",
            reply.text
        );
        assert_eq!(reply.code(), code);
    }
    let query = s
        .env
        .send(
            post("/api/v1/report/t.by_customer?limit=5")
                .session(&s.mgr)
                .json(october()),
        )
        .await;
    assert_eq!(query.code(), "invalid_query");
    let not_json = s
        .env
        .send(
            post("/api/v1/report/t.by_customer")
                .session(&s.mgr)
                .raw("text/plain", b"x"),
        )
        .await;
    assert_eq!(not_json.status, StatusCode::UNSUPPORTED_MEDIA_TYPE);
    let list_query = s.env.send(get("/api/v1/report?x=1").session(&s.mgr)).await;
    assert_eq!(list_query.code(), "invalid_query");
    // Methods: the list is GET only and a run is POST only.
    let wrong = s
        .env
        .send(post("/api/v1/report").session(&s.mgr).json(json!({})))
        .await;
    assert_eq!(wrong.status, StatusCode::METHOD_NOT_ALLOWED);
    let wrong = s
        .env
        .send(get("/api/v1/report/t.by_customer").session(&s.mgr))
        .await;
    assert_eq!(wrong.status, StatusCode::METHOD_NOT_ALLOWED);
}

#[tokio::test]
async fn the_body_limit_applies() {
    let env = Env::build_with(
        AppConfig {
            body_limit_bytes: 256,
            ..AppConfig::default()
        },
        &[],
        reports(),
    )
    .await;
    env.user("mgr", &["Sales Manager"]).await;
    let mgr = env.login("mgr").await;
    let big = json!({"params": {"customer": "x".repeat(1000)}});
    let reply = env
        .send(post("/api/v1/report/t.by_customer").session(&mgr).json(big))
        .await;
    assert_eq!(reply.status, StatusCode::PAYLOAD_TOO_LARGE);
    assert_eq!(reply.code(), "payload_too_large");
}

#[tokio::test]
async fn authentication_and_csrf_are_asked_like_for_every_other_route() {
    let s = setup().await;
    let body = october();
    // No credential.
    let list = s.env.send(get("/api/v1/report")).await;
    assert_eq!(
        (list.status, list.code()),
        (StatusCode::UNAUTHORIZED, "unauthenticated".to_owned())
    );
    let none = s
        .env
        .send(post("/api/v1/report/t.by_customer").json(body.clone()))
        .await;
    assert_eq!(none.status, StatusCode::UNAUTHORIZED);
    // The cookie without the CSRF token: refused, and the report did not run.
    let csrf = s
        .env
        .send(
            post("/api/v1/report/t.by_customer")
                .cookie(&s.mgr)
                .json(body.clone()),
        )
        .await;
    assert_eq!(
        (csrf.status, csrf.code()),
        (StatusCode::FORBIDDEN, "csrf_failed".to_owned())
    );
    let wrong = s
        .env
        .send(
            post("/api/v1/report/t.by_customer")
                .cookie(&s.mgr)
                .csrf("wrong")
                .json(body.clone()),
        )
        .await;
    assert_eq!(wrong.code(), "csrf_failed");
    // An API token needs no CSRF token.
    let token = s.env.api_token("mgr").await;
    let ok = s
        .env
        .send(
            post("/api/v1/report/t.by_customer")
                .token(&token)
                .json(body),
        )
        .await;
    assert_eq!(ok.status, StatusCode::OK, "{}", ok.text);
    // A GET needs only the cookie.
    let list = s.env.send(get("/api/v1/report").cookie(&s.mgr)).await;
    assert_eq!(list.status, StatusCode::OK);
}

#[tokio::test]
async fn a_slow_report_is_504_and_a_report_that_writes_is_an_internal_error() {
    let s = setup().await;
    let slow = run(&s, &s.mgr, "t.slow", json!({})).await;
    assert_eq!(slow.status, StatusCode::GATEWAY_TIMEOUT, "{}", slow.text);
    assert_eq!(slow.code(), "report_timeout");
    assert!(
        slow.json()["error"]["message"]
            .as_str()
            .expect("message")
            .contains("took too long"),
        "{}",
        slow.text
    );
    let writer = run(&s, &s.mgr, "t.writer", json!({})).await;
    assert_eq!(
        writer.status,
        StatusCode::INTERNAL_SERVER_ERROR,
        "{}",
        writer.text
    );
    assert_eq!(writer.code(), "internal");
    assert_eq!(writer.json()["error"]["message"], "internal server error");
    sales_unchanged(&s).await;
}

#[tokio::test]
async fn the_sql_is_in_no_response() {
    let s = setup().await;
    let _ = s.env.send(get("/api/v1/report").session(&s.mgr)).await;
    let _ = run(&s, &s.mgr, "t.by_customer", october()).await;
    let _ = run(&s, &s.mgr, "t.by_customer", json!({"params": {"x": 1}})).await;
    let _ = run(&s, &s.mgr, "t.slow", json!({})).await;
    let _ = run(&s, &s.mgr, "t.writer", json!({})).await;
    let _ = run(&s, &s.clerk, "t.by_customer", october()).await;
    let log = s.env.log.lock().expect("log").clone();
    assert!(log.len() >= 6);
    for reply in log {
        for needle in ["SELECT", "pg_sleep", "UPDATE sales", "FROM sales", "$1"] {
            assert!(!reply.text.contains(needle), "{needle} in {}", reply.text);
        }
    }
}
