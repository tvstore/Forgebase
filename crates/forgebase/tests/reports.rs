//! Query reports through the public API (M8.4, ADR 0016), written with `forgebase::testing`: a
//! fixture app that defines reports, the role gate over HTTP, the static check of the SQL at
//! `migrate` (a parameter count, a column count and a type that do not fit), the reports of an
//! app that is not installed, and the effect of a report that tries to write.

use std::time::Duration;

use forgebase::testing::{MigrateError, TestDb, check_reports, json};
use forgebase::{App, Column, DocTypeFile, Forgebase, Param, Report, Value};

const CUSTOMER: &str = "name = \"customer\"\nlabel = \"Customer\"\nnaming = { field = \"code\" }\n\
    title_field = \"full_name\"\n\
    [[fields]]\nname = \"code\"\ntype = \"data\"\nrequired = true\nunique = true\n\
    [[fields]]\nname = \"full_name\"\ntype = \"data\"\n\
    [[permissions]]\nrole = \"Sales Manager\"\nread = true\n";

const SEED: &str = "CREATE TABLE sale (customer text, orders int, total numeric, day date); \
    INSERT INTO sale VALUES ('c-1', 2, 30.5, '2026-10-02'), ('c-2', 5, 80, '2026-10-09'), ('c-1', 1, 4.25, '2026-11-01')";

const BY_CUSTOMER: &str = "SELECT customer, sum(orders)::int8 AS orders, sum(total) AS total \
    FROM sale WHERE day >= $1 AND day <= $2 AND ($3::text IS NULL OR customer = $3) \
    GROUP BY customer ORDER BY total DESC;\n";

fn crm() -> App {
    App::new("crm")
        .doctypes(const { &[DocTypeFile::new("doctypes/customer.toml", CUSTOMER)] })
        .migrations(
            const {
                &[forgebase::MigrationFile::new(
                    1,
                    "sales",
                    "migrations/0001_sales.sql",
                    SEED,
                )]
            },
        )
}

fn sales_report() -> Report {
    Report::sql(BY_CUSTOMER)
        .label("Sales by customer")
        .roles(&["Sales Manager"])
        .param(Param::date("from").required())
        .param(Param::date("to").required())
        .param(Param::link("customer", "customer"))
        .column(Column::link("customer", "customer"))
        .column(Column::int("orders"))
        .column(Column::decimal("total"))
}

fn sales() -> App {
    App::new("sales")
        .depends_on(&["crm"])
        .report("by_customer", sales_report())
        .report(
            "defaults",
            Report::sql("SELECT $1::text, $2::int8")
                .roles(&["Sales Manager"])
                .timeout(Duration::from_secs(5))
                .max_rows(5)
                .param(
                    Param::select("period", &["month", "year"])
                        .default_value(Value::Text("year".into())),
                )
                .param(Param::int("limit").required().default_value(Value::Int(10)))
                .column(Column::data("period"))
                .column(Column::int("limit")),
        )
        .report(
            "danger",
            Report::sql("DELETE FROM sale RETURNING orders")
                .roles(&["Sales Manager"])
                .column(Column::int("orders")),
        )
}

fn binary() -> Forgebase {
    Forgebase::new().app(crm()).app(sales())
}

async fn migrated(binary: &Forgebase) -> TestDb {
    let db = TestDb::new().await;
    db.migrate(binary, false).await.expect("migrate");
    db
}

#[tokio::test]
async fn the_roles_gate_the_list_and_the_run_and_the_administrator_always_passes() {
    let binary = binary();
    let db = migrated(&binary).await;
    db.create_user("mia", &["Sales Manager"]).await;
    db.create_user("cat", &["Clerk"]).await;
    db.create_user("root", &["Administrator"]).await;
    let anon = db.client(&binary);
    let mia = anon.login("mia").await;
    let cat = anon.login("cat").await;
    let root = anon.login("root").await;

    let listed = mia.get("/api/v1/report").await;
    assert_eq!(listed.status, 200);
    let names: Vec<&str> = listed
        .data()
        .as_array()
        .expect("list")
        .iter()
        .map(|r| r["name"].as_str().expect("name"))
        .collect();
    // By label: "Danger", "Defaults", "Sales by customer".
    assert_eq!(
        names,
        ["sales.danger", "sales.defaults", "sales.by_customer"]
    );
    assert_eq!(listed.data()[1]["params"][0]["default"], "year");
    assert_eq!(
        listed.data()[1]["params"][0]["options"],
        json!(["month", "year"])
    );
    assert_eq!(listed.data()[1]["params"][1]["default"], 10);
    assert_eq!(
        root.get("/api/v1/report")
            .await
            .data()
            .as_array()
            .expect("list")
            .len(),
        3
    );
    // The Clerk: no report listed, and a run is the same 404 as for a report that is not there.
    assert_eq!(cat.get("/api/v1/report").await.data(), &json!([]));
    let body = json!({"params": {"from": "2026-10-01", "to": "2026-10-31"}});
    let refused = cat
        .post("/api/v1/report/sales.by_customer", body.clone())
        .await;
    let unknown = cat.post("/api/v1/report/sales.nothing", body.clone()).await;
    assert_eq!((refused.status, unknown.status), (404, 404));
    assert_eq!(refused.body, unknown.body);

    let reply = mia
        .post("/api/v1/report/sales.by_customer", body.clone())
        .await;
    assert_eq!(reply.status, 200, "{:?}", reply.body);
    assert_eq!(
        reply.data()["rows"],
        json!([["c-2", 5, "80"], ["c-1", 2, "30.5"]])
    );
    assert_eq!(reply.data()["truncated"], false);
    let admin = root.post("/api/v1/report/sales.by_customer", body).await;
    assert_eq!(admin.status, 200);
    db.cleanup().await;
}

#[tokio::test]
async fn defaults_fill_absent_params_and_limits_are_the_reports_own() {
    let binary = binary();
    let db = migrated(&binary).await;
    db.create_user("mia", &["Sales Manager"]).await;
    let mia = db.client(&binary).login("mia").await;
    let reply = mia.post("/api/v1/report/sales.defaults", json!({})).await;
    assert_eq!(reply.status, 200, "{:?}", reply.body);
    assert_eq!(reply.data()["rows"], json!([["year", 10]]));
    let given = mia
        .post(
            "/api/v1/report/sales.defaults",
            json!({"params": {"period": "month", "limit": 3}}),
        )
        .await;
    assert_eq!(given.data()["rows"], json!([["month", 3]]));
    db.cleanup().await;
}

#[tokio::test]
async fn a_report_that_writes_fails_and_the_data_is_unchanged() {
    let binary = binary();
    let db = migrated(&binary).await;
    db.create_user("mia", &["Sales Manager"]).await;
    let mia = db.client(&binary).login("mia").await;
    let reply = mia.post("/api/v1/report/sales.danger", json!({})).await;
    assert_eq!(reply.status, 500);
    assert_eq!(reply.error_code(), Some("internal"));
    assert_eq!(
        db.rows("SELECT count(*)::text FROM sale").await,
        vec![vec!["3".to_owned()]]
    );
    db.cleanup().await;
}

#[tokio::test]
async fn migrate_checks_every_report_and_names_the_report_and_the_problem() {
    let broken = |report: Report| {
        Forgebase::new()
            .app(crm())
            .app(App::new("sales").depends_on(&["crm"]).report("r", report))
    };
    let cases: [(Report, &str); 4] = [
        (
            // The SQL has three parameters, two are declared.
            Report::sql(
                "SELECT customer FROM sale WHERE day >= $1 AND day <= $2 AND customer = $3",
            )
            .roles(&["A"])
            .param(Param::date("from"))
            .param(Param::date("to"))
            .column(Column::data("customer")),
            "report `sales.r`: the SQL has 3 parameter(s) but 2 are declared",
        ),
        (
            Report::sql("SELECT customer, orders FROM sale")
                .roles(&["A"])
                .column(Column::data("customer")),
            "report `sales.r`: the SQL returns 2 column(s) but 1 are declared",
        ),
        (
            Report::sql("SELECT customer, orders FROM sale")
                .roles(&["A"])
                .column(Column::data("customer"))
                .column(Column::data("orders")),
            "column 2 (`orders`) is declared data, but the SQL returns int4",
        ),
        (
            Report::sql("SELECT customer FROM sale WHERE orders > $1")
                .roles(&["A"])
                .param(Param::date("min"))
                .column(Column::data("customer")),
            "parameter $1 (`min`) is declared date, but the SQL expects int4",
        ),
    ];
    for (report, expected) in cases {
        let binary = broken(report);
        let db = TestDb::new().await;
        let error = db.migrate(&binary, false).await.expect_err(expected);
        let MigrateError::Failed(message) = &error else {
            panic!("{error:?}");
        };
        assert!(message.contains(expected), "{expected} in {message}");
        // The migrations were applied before the check: the table of the app migration exists.
        assert_eq!(
            db.rows("SELECT count(*)::text FROM sale").await,
            vec![vec!["3".to_owned()]]
        );
        // The helper gives the same messages without failing a migrate.
        let found = check_reports(&binary, &db).await;
        assert_eq!(found.len(), 1, "{found:?}");
        assert!(found[0].contains(expected), "{found:?}");
        db.cleanup().await;
    }
}

#[tokio::test]
async fn a_dry_run_does_not_check_and_a_report_that_fits_passes() {
    let binary = binary();
    let db = TestDb::new().await;
    // Nothing exists yet, so the SQL could not be prepared: a dry run does not try.
    db.migrate(&binary, true).await.expect("dry run");
    db.migrate(&binary, false).await.expect("migrate");
    assert_eq!(check_reports(&binary, &db).await, Vec::<String>::new());
    // A second migrate checks again and passes.
    db.migrate(&binary, false).await.expect("migrate again");
    db.cleanup().await;
}

#[tokio::test]
async fn the_reports_of_an_app_that_is_not_installed_are_neither_listed_nor_run() {
    // A site that installs only `crm` composes only crm: the registry has no report of `sales`.
    let full = binary();
    let only_crm = Forgebase::new().app(crm());
    let db = migrated(&full).await;
    db.create_user("mia", &["Sales Manager"]).await;
    db.create_user("root", &["Administrator"]).await;
    for (who, binary) in [("mia", &only_crm), ("root", &only_crm)] {
        let client = db.client(binary).login(who).await;
        assert_eq!(
            client.get("/api/v1/report").await.data(),
            &json!([]),
            "{who}"
        );
        let reply = client
            .post(
                "/api/v1/report/sales.by_customer",
                json!({"params": {"from": "2026-10-01", "to": "2026-10-31"}}),
            )
            .await;
        assert_eq!(reply.status, 404, "{who}");
    }
    let client = db.client(&full).login("mia").await;
    assert_eq!(
        client
            .get("/api/v1/report")
            .await
            .data()
            .as_array()
            .expect("list")
            .len(),
        3
    );
    db.cleanup().await;
}

#[test]
fn a_startup_error_names_the_report() {
    // A report with no roles and no columns is a startup error that names it, among the others.
    assert!(Forgebase::new().app(crm()).check().is_ok());
    let bad = Forgebase::new().app(crm()).app(
        App::new("sales")
            .depends_on(&["crm"])
            .report("r", Report::sql("SELECT 1")),
    );
    let error = bad.check().expect_err("refused");
    let text = error.to_string();
    assert!(
        text.contains("report `r`: no roles") && text.contains("report `r`: no columns"),
        "{text}"
    );
}
