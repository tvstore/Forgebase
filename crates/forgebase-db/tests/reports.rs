//! Query reports (M8.4, ADR 0016) against a real PostgreSQL (see `common`): the role gate,
//! parameter checks, SQL injection, the read-only transaction, the timeout, the row limit, the
//! conversion of every column kind and the static check of the statements.

mod common;

use std::collections::BTreeMap;
use std::time::{Duration, Instant};

use common::TestDb;
use forgebase_core::permission::{ADMINISTRATOR, Caller};
use forgebase_core::report::{
    ColumnKind, DEFAULT_MAX_ROWS, DEFAULT_TIMEOUT, ParamKind, ReportColumn, ReportDef, ReportParam,
};
use forgebase_core::value::Value;
use forgebase_db::{DbError, ReportConnection, check_reports, connect_sized, run_report};
use rust_decimal::Decimal;
use time::{Date, Month};

fn report(sql: &str, params: Vec<ReportParam>, columns: Vec<(&str, ColumnKind)>) -> ReportDef {
    ReportDef {
        app: "t".to_owned(),
        name: "r".to_owned(),
        label: "R".to_owned(),
        sql: sql.to_owned(),
        roles: vec!["Manager".to_owned()],
        params,
        columns: columns
            .into_iter()
            .map(|(name, kind)| ReportColumn {
                name: name.to_owned(),
                label: name.to_owned(),
                kind,
            })
            .collect(),
        timeout: DEFAULT_TIMEOUT,
        max_rows: DEFAULT_MAX_ROWS,
    }
}

fn param(name: &str, kind: ParamKind, required: bool) -> ReportParam {
    ReportParam {
        name: name.to_owned(),
        label: name.to_owned(),
        kind,
        required,
        default: None,
    }
}

fn manager() -> Caller {
    Caller::user("ann", vec!["Manager".to_owned()])
}

fn given(pairs: &[(&str, Value)]) -> BTreeMap<String, Value> {
    pairs
        .iter()
        .map(|(k, v)| ((*k).to_owned(), v.clone()))
        .collect()
}

fn text(s: &str) -> Value {
    Value::Text(s.to_owned())
}

async fn run(
    db: &TestDb,
    report: &ReportDef,
    caller: &Caller,
    params: &BTreeMap<String, Value>,
) -> Result<forgebase_db::ReportResult, DbError> {
    let mut client = db.pool.get().await.expect("connection");
    run_report(&mut client, report, caller, params).await
}

async fn problems(db: &TestDb, reports: &[&ReportDef]) -> Vec<String> {
    let mut client = db.pool.get().await.expect("connection");
    check_reports(&mut client, reports).await.expect("check")
}

/// A table of three rows that the reports read and the write tests try to change.
async fn items() -> TestDb {
    let db = TestDb::new().await;
    db.exec(
        "CREATE TABLE items (id int PRIMARY KEY, label text NOT NULL, amount numeric(12,2), day date);
         INSERT INTO items VALUES (1, 'bolt', 1.50, '2026-10-01'), (2, 'nut', 2.25, '2026-10-15'), (3, 'washer', NULL, NULL)",
    )
    .await;
    db
}

async fn items_unchanged(db: &TestDb) {
    assert_eq!(
        db.rows("SELECT id::text, label, coalesce(amount::text, '-') FROM items ORDER BY id")
            .await,
        vec![
            vec!["1", "bolt", "1.50"],
            vec!["2", "nut", "2.25"],
            vec!["3", "washer", "-"],
        ]
    );
}

const LIST: &str = "SELECT id, label FROM items WHERE ($1::text IS NULL OR label = $1) ORDER BY id";

fn list_report() -> ReportDef {
    report(
        LIST,
        vec![param("label", ParamKind::Data, false)],
        vec![("id", ColumnKind::Int), ("label", ColumnKind::Data)],
    )
}

#[tokio::test]
async fn only_the_roles_the_administrator_and_the_system_run_a_report() {
    let db = items().await;
    let report = list_report();
    let none = BTreeMap::new();
    let rows = |r: forgebase_db::ReportResult| r.rows.len();
    assert_eq!(
        rows(run(&db, &report, &manager(), &none).await.expect("manager")),
        3
    );
    let admin = Caller::user("root", vec![ADMINISTRATOR.to_owned()]);
    assert_eq!(
        rows(run(&db, &report, &admin, &none).await.expect("admin")),
        3
    );
    let system = Caller::system("cli");
    assert_eq!(
        rows(run(&db, &report, &system, &none).await.expect("system")),
        3
    );
    for roles in [vec!["Clerk".to_owned()], vec![]] {
        let clerk = Caller::user("bob", roles);
        let error = run(&db, &report, &clerk, &none).await.expect_err("refused");
        assert!(
            matches!(&error, DbError::ReportNotFound { report } if report == "t.r"),
            "{error:?}"
        );
        // The refusal comes before the parameters are looked at: it says nothing about them.
        let bad = given(&[("nope", Value::Null)]);
        assert!(matches!(
            run(&db, &report, &clerk, &bad).await,
            Err(DbError::ReportNotFound { .. })
        ));
    }
}

#[tokio::test]
async fn parameters_are_bound_in_order_and_an_absent_optional_one_is_null() {
    let db = items().await;
    let report = list_report();
    let all = run(&db, &report, &manager(), &BTreeMap::new())
        .await
        .expect("all");
    assert_eq!(all.rows.len(), 3);
    assert!(!all.truncated);
    let one = run(&db, &report, &manager(), &given(&[("label", text("nut"))]))
        .await
        .expect("one");
    assert_eq!(one.rows, vec![vec![Value::Int(2), text("nut")]]);
    // An explicit null is the same as absent.
    let null = run(&db, &report, &manager(), &given(&[("label", Value::Null)]))
        .await
        .expect("null");
    assert_eq!(null.rows.len(), 3);
}

#[tokio::test]
async fn parameter_problems_are_reported_per_parameter() {
    let db = items().await;
    let mut report = list_report();
    report.params = vec![
        param("from", ParamKind::Date, true),
        param("label", ParamKind::Data, false),
        param(
            "kind",
            ParamKind::Select {
                options: vec!["a".to_owned(), "b".to_owned()],
            },
            false,
        ),
    ];
    let error = run(
        &db,
        &report,
        &manager(),
        &given(&[
            ("label", Value::Int(3)),
            ("kind", text("c")),
            ("extra", text("x")),
        ]),
    )
    .await
    .expect_err("invalid");
    let DbError::Validation(errors) = error else {
        panic!("{error:?}");
    };
    let text: Vec<String> = errors.iter().map(ToString::to_string).collect();
    assert_eq!(
        text,
        [
            "unknown field `extra`",
            "field `from` is required",
            "field `label`: expected text, found int",
            "field `kind`: `c` is not one of the allowed options",
        ]
    );
}

#[tokio::test]
async fn a_parameter_cannot_inject_sql() {
    let db = items().await;
    let report = list_report();
    for hostile in [
        "'); DROP TABLE items; --",
        "x' OR '1'='1",
        "\\'; DELETE FROM items; --",
    ] {
        let result = run(
            &db,
            &report,
            &manager(),
            &given(&[("label", text(hostile))]),
        )
        .await
        .expect("runs");
        assert!(result.rows.is_empty(), "{hostile}: {result:?}");
    }
    items_unchanged(&db).await;
    // A value that is data is returned as data.
    let echo = report_echo();
    let result = run(
        &db,
        &echo,
        &manager(),
        &given(&[("v", text("'); DROP TABLE items; --"))]),
    )
    .await
    .expect("echo");
    assert_eq!(result.rows, vec![vec![text("'); DROP TABLE items; --")]]);
    items_unchanged(&db).await;
}

fn report_echo() -> ReportDef {
    report(
        "SELECT $1::text",
        vec![param("v", ParamKind::Data, true)],
        vec![("v", ColumnKind::Data)],
    )
}

#[tokio::test]
async fn a_report_that_writes_fails_and_changes_nothing() {
    let db = items().await;
    for sql in [
        "WITH d AS (DELETE FROM items RETURNING 1) SELECT count(*) FROM d",
        "UPDATE items SET label = 'x' RETURNING id",
        "INSERT INTO items VALUES (9, 'new', 1, NULL) RETURNING id",
    ] {
        let report = report(sql, vec![], vec![("n", ColumnKind::Int)]);
        let error = run(&db, &report, &manager(), &BTreeMap::new())
            .await
            .expect_err(sql);
        assert!(
            matches!(&error, DbError::ReportReadOnly { report } if report == "t.r"),
            "{sql}: {error:?}"
        );
        // The message never shows the SQL.
        assert!(!error.to_string().contains("items"), "{error}");
    }
    items_unchanged(&db).await;
}

#[tokio::test]
async fn a_statement_that_is_not_a_query_is_refused_before_it_runs() {
    let db = items().await;
    for sql in ["COMMIT", "DELETE FROM items"] {
        let report = report(sql, vec![], vec![("n", ColumnKind::Int)]);
        let error = run(&db, &report, &manager(), &BTreeMap::new())
            .await
            .expect_err(sql);
        assert!(
            matches!(&error, DbError::Report(m) if m.contains("t.r")),
            "{error:?}"
        );
        assert!(!error.to_string().contains("DELETE"), "{error}");
    }
    items_unchanged(&db).await;
}

#[tokio::test]
async fn a_multi_statement_report_is_refused_and_changes_nothing() {
    // The extended protocol prepares one statement: a second one riding along never runs.
    let db = items().await;
    let sql = "SELECT 1; DELETE FROM items";
    let report = report(sql, vec![], vec![("n", ColumnKind::Int)]);
    let error = run(&db, &report, &manager(), &BTreeMap::new())
        .await
        .expect_err("refused");
    let message = error.to_string();
    assert!(message.contains("multiple commands"), "{message}");
    // The message never shows the SQL.
    assert!(
        !message.contains("DELETE") && !message.contains("items"),
        "{message}"
    );
    items_unchanged(&db).await;
    // The check at migrate says so, naming the report and not the SQL.
    let found = problems(&db, &[&report]).await;
    assert_eq!(found.len(), 1, "{found:?}");
    assert!(
        found[0].contains("report `t.r`: the SQL does not prepare:"),
        "{found:?}"
    );
    assert!(!found[0].contains("DELETE"), "{found:?}");
    items_unchanged(&db).await;
}

#[tokio::test]
async fn a_report_connection_is_closed_so_session_state_cannot_leak_to_the_next_checkout() {
    // A pool of one connection makes the next checkout deterministic: it is the same pool, and
    // a leaked setting or lock would be on whatever connection it hands out.
    let db = items().await;
    let pool = connect_sized(&db.url(), Some(1))
        .await
        .expect("pool of one");
    // Session-level state, which only the connection that ran the report would keep.
    let leaky = report(
        "SELECT 1 AS n FROM (SELECT set_config('search_path', 'pg_catalog', false)) c, \
         (SELECT pg_advisory_lock(4242)) l",
        vec![],
        vec![("n", ColumnKind::Int)],
    );
    {
        let mut connection = ReportConnection::get(&pool).await.expect("connection");
        let result = run_report(&mut connection, &leaky, &manager(), &BTreeMap::new())
            .await
            .expect("runs");
        assert_eq!(result.rows, vec![vec![Value::Int(1)]]);
        // The advisory lock really is held by this connection (a session lock is not released by
        // the rollback that ends the report's transaction), so the check below proves something.
        // (A session-level `set_config` is undone by that rollback already.)
        let held: i64 = connection
            .query_one(
                "SELECT count(*) FROM pg_locks WHERE locktype = 'advisory' AND objsubid = 1 \
                 AND objid = 4242 AND pid = pg_backend_pid()",
                &[],
            )
            .await
            .expect("locks")
            .get(0);
        assert_eq!(held, 1);
    }
    // The next checkout is a new connection: the default setting, and no advisory lock held.
    let client = pool.get().await.expect("next checkout");
    let seen: String = client
        .query_one("SELECT current_setting('search_path')", &[])
        .await
        .expect("setting")
        .get(0);
    assert!(seen.contains("public"), "{seen}");
    // The server notices the closed connection a moment later: poll for the lock to go.
    let started = Instant::now();
    loop {
        let locks: i64 = client
            .query_one(
                "SELECT count(*) FROM pg_locks WHERE locktype = 'advisory' AND objsubid = 1 AND objid = 4242",
                &[],
            )
            .await
            .expect("locks")
            .get(0);
        if locks == 0 {
            break;
        }
        assert!(
            started.elapsed() < Duration::from_secs(10),
            "the advisory lock of the report is still held"
        );
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    items_unchanged(&db).await;
}

#[tokio::test]
async fn the_timeout_stops_a_slow_report_and_the_connection_stays_usable() {
    let db = items().await;
    let mut report = report(
        "SELECT pg_sleep(10)::text",
        vec![],
        vec![("s", ColumnKind::Data)],
    );
    report.timeout = Duration::from_millis(200);
    let mut client = db.pool.get().await.expect("connection");
    let started = Instant::now();
    let error = run_report(&mut client, &report, &manager(), &BTreeMap::new())
        .await
        .expect_err("timeout");
    assert!(
        matches!(&error, DbError::ReportTimeout { report } if report == "t.r"),
        "{error:?}"
    );
    // The ten seconds of the statement were not waited for.
    assert!(
        started.elapsed() < Duration::from_secs(5),
        "{:?}",
        started.elapsed()
    );
    // The same connection runs the next report, and the timeout did not stay on it.
    let fine = list_report();
    let result = run_report(&mut client, &fine, &manager(), &BTreeMap::new())
        .await
        .expect("next");
    assert_eq!(result.rows.len(), 3);
    let setting = client
        .query_one("SHOW statement_timeout", &[])
        .await
        .expect("show");
    assert_eq!(setting.get::<_, String>(0), "0");
}

#[tokio::test]
async fn the_row_limit_cuts_the_result_and_says_so() {
    let db = TestDb::new().await;
    let mut report = report(
        "SELECT g FROM generate_series(1, 25) AS g",
        vec![],
        vec![("g", ColumnKind::Int)],
    );
    report.max_rows = 10;
    let cut = run(&db, &report, &manager(), &BTreeMap::new())
        .await
        .expect("cut");
    assert_eq!(cut.rows.len(), 10);
    assert!(cut.truncated);
    assert_eq!(cut.rows[9], vec![Value::Int(10)]);
    // Exactly the limit is not truncated; one more is.
    report.max_rows = 25;
    let exact = run(&db, &report, &manager(), &BTreeMap::new())
        .await
        .expect("exact");
    assert_eq!((exact.rows.len(), exact.truncated), (25, false));
    report.max_rows = 24;
    let over = run(&db, &report, &manager(), &BTreeMap::new())
        .await
        .expect("over");
    assert_eq!((over.rows.len(), over.truncated), (24, true));
}

#[tokio::test]
async fn every_column_kind_is_converted_to_a_value() {
    let db = TestDb::new().await;
    let report = report(
        "SELECT 'x'::text, 'y'::varchar, 7::int2, 8::int4, 9::int8, 1.50::numeric, \
         date '2026-10-30', timestamptz '2026-10-30 10:20:30+02', true, 'C-1'::text",
        vec![],
        vec![
            ("a", ColumnKind::Data),
            ("b", ColumnKind::Data),
            ("c", ColumnKind::Int),
            ("d", ColumnKind::Int),
            ("e", ColumnKind::Int),
            ("f", ColumnKind::Decimal),
            ("g", ColumnKind::Date),
            ("h", ColumnKind::Datetime),
            ("i", ColumnKind::Check),
            (
                "j",
                ColumnKind::Link {
                    doctype: "customer".to_owned(),
                },
            ),
        ],
    );
    let result = run(&db, &report, &manager(), &BTreeMap::new())
        .await
        .expect("row");
    let day = Date::from_calendar_date(2026, Month::October, 30).expect("date");
    let at = day.with_hms(8, 20, 30).expect("time").assume_utc();
    assert_eq!(
        result.rows,
        vec![vec![
            text("x"),
            text("y"),
            Value::Int(7),
            Value::Int(8),
            Value::Int(9),
            Value::Decimal(Decimal::new(150, 2)),
            Value::Date(day),
            Value::Datetime(at),
            Value::Bool(true),
            text("C-1"),
        ]]
    );
    // Nulls of every kind.
    let nulls = ReportDef {
        sql: "SELECT NULL::text, NULL::int4, NULL::numeric, NULL::date, NULL::timestamptz, NULL::bool"
            .to_owned(),
        columns: vec![
            ReportColumn { name: "a".into(), label: "a".into(), kind: ColumnKind::Data },
            ReportColumn { name: "b".into(), label: "b".into(), kind: ColumnKind::Int },
            ReportColumn { name: "c".into(), label: "c".into(), kind: ColumnKind::Decimal },
            ReportColumn { name: "d".into(), label: "d".into(), kind: ColumnKind::Date },
            ReportColumn { name: "e".into(), label: "e".into(), kind: ColumnKind::Datetime },
            ReportColumn { name: "f".into(), label: "f".into(), kind: ColumnKind::Check },
        ],
        ..report
    };
    let result = run(&db, &nulls, &manager(), &BTreeMap::new())
        .await
        .expect("nulls");
    assert_eq!(result.rows, vec![vec![Value::Null; 6]]);
}

#[tokio::test]
async fn parameters_of_every_kind_reach_the_statement_with_their_value() {
    let db = TestDb::new().await;
    let report = report(
        "SELECT $1::int4 + 1, $2::numeric * 2, $3::date + 1, NOT $4::bool, $5::text, $6::text",
        vec![
            param("i", ParamKind::Int, true),
            param("d", ParamKind::Decimal, true),
            param("day", ParamKind::Date, true),
            param("b", ParamKind::Check, true),
            param(
                "l",
                ParamKind::Link {
                    doctype: "customer".to_owned(),
                },
                true,
            ),
            param(
                "s",
                ParamKind::Select {
                    options: vec!["a".to_owned()],
                },
                true,
            ),
        ],
        vec![
            ("i", ColumnKind::Int),
            ("d", ColumnKind::Decimal),
            ("day", ColumnKind::Date),
            ("b", ColumnKind::Check),
            ("l", ColumnKind::Data),
            ("s", ColumnKind::Data),
        ],
    );
    let day = Date::from_calendar_date(2026, Month::October, 30).expect("date");
    let result = run(
        &db,
        &report,
        &manager(),
        &given(&[
            ("i", Value::Int(41)),
            ("d", Value::Decimal(Decimal::new(125, 2))),
            ("day", Value::Date(day)),
            ("b", Value::Bool(false)),
            ("l", text("C-1")),
            ("s", text("a")),
        ]),
    )
    .await
    .expect("row");
    assert_eq!(
        result.rows,
        vec![vec![
            Value::Int(42),
            Value::Decimal(Decimal::new(250, 2)),
            Value::Date(day.next_day().expect("next")),
            Value::Bool(true),
            text("C-1"),
            text("a"),
        ]]
    );
    // An integer that does not fit the type PostgreSQL inferred is a database error, not a wrap.
    let error = run(
        &db,
        &report,
        &manager(),
        &given(&[
            ("i", Value::Int(i64::MAX)),
            ("d", Value::Decimal(Decimal::ONE)),
            ("day", Value::Date(day)),
            ("b", Value::Bool(true)),
            ("l", text("x")),
            ("s", text("a")),
        ]),
    )
    .await
    .expect_err("out of range");
    assert!(matches!(error, DbError::Postgres(_)), "{error:?}");
}

#[tokio::test]
async fn the_check_accepts_a_report_that_fits() {
    let db = items().await;
    let good = report(
        "SELECT id, label, amount, day FROM items WHERE id >= $1 AND ($2::text IS NULL OR label = $2) AND day >= $3 AND amount > $4",
        vec![
            param("min", ParamKind::Int, true),
            param("label", ParamKind::Data, false),
            param("from", ParamKind::Date, false),
            param("above", ParamKind::Decimal, false),
        ],
        vec![
            ("id", ColumnKind::Int),
            ("label", ColumnKind::Data),
            ("amount", ColumnKind::Decimal),
            ("day", ColumnKind::Date),
        ],
    );
    assert_eq!(
        problems(&db, &[&good, &list_report()]).await,
        Vec::<String>::new()
    );
}

#[tokio::test]
async fn the_check_names_the_report_and_the_problem() {
    let db = items().await;
    let sql = "SELECT id, label FROM items WHERE id >= $1";
    let fewer = report(
        sql,
        vec![],
        vec![("id", ColumnKind::Int), ("label", ColumnKind::Data)],
    );
    let more_columns = report(
        sql,
        vec![param("min", ParamKind::Int, false)],
        vec![("id", ColumnKind::Int)],
    );
    let wrong_type = report(
        sql,
        vec![param("min", ParamKind::Int, false)],
        vec![("id", ColumnKind::Decimal), ("label", ColumnKind::Check)],
    );
    let wrong_param = report(
        sql,
        vec![param("min", ParamKind::Date, false)],
        vec![("id", ColumnKind::Int), ("label", ColumnKind::Data)],
    );
    let broken = report(
        "SELECT nope FROM items",
        vec![],
        vec![("id", ColumnKind::Int)],
    );
    let missing_table = report(
        "SELECT 1 FROM ghosts",
        vec![],
        vec![("id", ColumnKind::Int)],
    );
    let found = problems(
        &db,
        &[
            &fewer,
            &more_columns,
            &wrong_type,
            &wrong_param,
            &broken,
            &missing_table,
        ],
    )
    .await;
    let text = found.join("\n");
    for needle in [
        "report `t.r`: the SQL has 1 parameter(s) but 0 are declared",
        "report `t.r`: the SQL returns 2 column(s) but 1 are declared",
        "column 1 (`id`) is declared decimal, but the SQL returns int4",
        "column 2 (`label`) is declared check, but the SQL returns text",
        "parameter $1 (`min`) is declared date, but the SQL expects int4",
        "the SQL does not prepare: column \"nope\" does not exist",
        "the SQL does not prepare: relation \"ghosts\" does not exist",
    ] {
        assert!(text.contains(needle), "{needle} in\n{text}");
    }
    assert_eq!(found.len(), 7, "{text}");
    // Preparing wrote nothing and left no savepoint or transaction behind.
    items_unchanged(&db).await;
}

#[tokio::test]
async fn a_parameter_typed_as_text_fits_every_kind() {
    let db = TestDb::new().await;
    let report = report(
        "SELECT $1::text, $2::text, $3::text, $4::text",
        vec![
            param("a", ParamKind::Int, false),
            param("b", ParamKind::Decimal, false),
            param("c", ParamKind::Date, false),
            param("d", ParamKind::Check, false),
        ],
        vec![
            ("a", ColumnKind::Data),
            ("b", ColumnKind::Data),
            ("c", ColumnKind::Data),
            ("d", ColumnKind::Data),
        ],
    );
    assert!(problems(&db, &[&report]).await.is_empty());
    let day = Date::from_calendar_date(2026, Month::October, 30).expect("date");
    let result = run(
        &db,
        &report,
        &manager(),
        &given(&[
            ("a", Value::Int(5)),
            ("b", Value::Decimal(Decimal::new(125, 2))),
            ("c", Value::Date(day)),
            ("d", Value::Bool(true)),
        ]),
    )
    .await
    .expect("row");
    assert_eq!(
        result.rows,
        vec![vec![
            text("5"),
            text("1.25"),
            text("2026-10-30"),
            text("true")
        ]]
    );
}
