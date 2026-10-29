//! The app model against a real PostgreSQL (M5.1, ADR 0012), written with `forgebase::testing`:
//! embedded files, hooks across apps, ownership in the metadata API, app migrations.
//!
//! Each test gets its own throwaway `fb_test_app_*` database and fails loudly (never skips) when
//! `FORGEBASE_TEST_DATABASE_URL` is not set; see CONTRIBUTING.md.

use std::sync::{Arc, Mutex};

use forgebase::testing::{AppliedMigration, TestDb, assert_doctypes_listed, json};
use forgebase::{
    App, DocTypeFile, DocTypeHooks, Document, Forgebase, HookContext, HookError, MigrationFile,
    Operation, Value,
};

/// A static list of DocType files, from `(path, toml)` pairs.
macro_rules! files {
    ($(($path:literal, $toml:expr)),* $(,)?) => {
        &[$(DocTypeFile { path: $path, toml: $toml }),*]
    };
}

/// A migration with a made-up path.
const fn migration(version: i32, name: &'static str, sql: &'static str) -> MigrationFile {
    MigrationFile {
        version,
        name,
        path: "migrations/inline.sql",
        sql,
    }
}

fn rows(list: &[&[&str]]) -> Vec<Vec<String>> {
    list.iter()
        .map(|row| row.iter().map(|v| (*v).to_owned()).collect())
        .collect()
}

fn applied(list: &[(&str, i32, &str)]) -> Vec<AppliedMigration> {
    list.iter()
        .map(|(app, version, name)| AppliedMigration {
            app: (*app).to_owned(),
            version: *version,
            name: (*name).to_owned(),
        })
        .collect()
}

// ---- embedded files -------------------------------------------------------------------------

/// The app of the fixture files, embedded with the macros: paths are relative to this crate.
fn fixture_app() -> App {
    App::new("fixture")
        .doctypes(forgebase::doctypes![
            "tests/fixtures/doctypes/widget.toml",
            "tests/fixtures/doctypes/gizmo.toml",
        ])
        .migrations(forgebase::migrations![(
            1,
            "seed",
            "tests/fixtures/migrations/0001_seed.sql"
        )])
}

#[test]
fn the_macros_embed_files_relative_to_the_crate_that_calls_them() {
    // The expansion is evaluated in this crate: its CARGO_MANIFEST_DIR is where the files are.
    let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR"));
    let widget = std::fs::read_to_string(root.join("tests/fixtures/doctypes/widget.toml"))
        .expect("fixture file");
    let seed = std::fs::read_to_string(root.join("tests/fixtures/migrations/0001_seed.sql"))
        .expect("fixture file");
    let doctypes = forgebase::doctypes!["tests/fixtures/doctypes/widget.toml"];
    assert_eq!(doctypes.len(), 1);
    assert_eq!(doctypes[0].path, "tests/fixtures/doctypes/widget.toml");
    assert_eq!(doctypes[0].toml, widget);
    let migrations = forgebase::migrations![(7, "seed", "tests/fixtures/migrations/0001_seed.sql")];
    assert_eq!(migrations.len(), 1);
    assert_eq!(
        (migrations[0].version, migrations[0].name, migrations[0].sql),
        (7, "seed", seed.as_str())
    );
    // Both accept a trailing comma and an empty list.
    let _: &[DocTypeFile] = forgebase::doctypes![];
    let _: &[MigrationFile] = forgebase::migrations![];
    Forgebase::new().app(fixture_app()).check().expect("valid");
}

#[tokio::test]
async fn an_app_with_embedded_files_migrates_and_serves() {
    let binary = Forgebase::new().app(fixture_app());
    let db = TestDb::new().await;
    let done = db.migrate(&binary, false).await.expect("migrate");
    assert_eq!(done.app_migrations, applied(&[("fixture", 1, "seed")]));
    assert_eq!(
        db.rows("SELECT name FROM fb_doctype ORDER BY name").await,
        rows(&[&["gizmo"], &["widget"]])
    );
    assert_eq!(
        db.rows("SELECT entry FROM fixture_log").await,
        rows(&[&["seeded"]])
    );
    // No version was declared: `fb_app` has the app with a null version.
    assert_eq!(
        db.rows("SELECT name, version FROM fb_app").await,
        rows(&[&["fixture", "NULL"]])
    );

    db.create_user("ann", &["Fixture User"]).await;
    let ann = db.client(&binary).login("ann").await;
    let reply = ann
        .post(
            "/api/v1/resource/widget",
            json!({"code": "w-1", "title": "A widget"}),
        )
        .await;
    assert_eq!(reply.status, 201, "{:?}", reply.body);
    let reply = ann.get("/api/v1/resource/widget/w-1").await;
    assert_eq!(reply.data()["title"], "A widget");
    db.cleanup().await;
}

// ---- the guard for the embedded list ----------------------------------------------------------

const FIXTURE_DOCTYPES: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/tests/fixtures/doctypes");

#[test]
fn the_guard_passes_when_the_list_and_the_directory_agree() {
    assert_doctypes_listed(&fixture_app(), FIXTURE_DOCTYPES);
}

#[test]
#[should_panic(expected = "in the directory but not in doctypes![..]: [\"gizmo.toml\"]")]
fn the_guard_fails_for_a_file_that_is_not_listed() {
    let app =
        App::new("fixture").doctypes(forgebase::doctypes!["tests/fixtures/doctypes/widget.toml"]);
    assert_doctypes_listed(&app, FIXTURE_DOCTYPES);
}

#[test]
#[should_panic(expected = "in doctypes![..] but not in the directory: [\"seed.toml\"]")]
fn the_guard_fails_for_a_listed_file_that_is_not_in_the_directory() {
    // A file of another directory in the list.
    let app = fixture_app().doctypes(files![(
        "elsewhere/seed.toml",
        "name = \"seed\"\nlabel = \"S\"\n"
    )]);
    assert_doctypes_listed(&app, FIXTURE_DOCTYPES);
}

#[test]
#[should_panic(expected = "cannot read DocType directory")]
fn the_guard_fails_for_a_missing_directory() {
    assert_doctypes_listed(&fixture_app(), "/nonexistent/forgebase/doctypes");
}

// ---- hooks across apps ------------------------------------------------------------------------

const CUSTOMER: &str = r#"
name = "customer"
label = "Customer"
naming = { field = "code" }
[[fields]]
name = "code"
type = "data"
required = true
unique = true
[[fields]]
name = "title"
type = "data"
"#;

const ORDER: &str = r#"
name = "order"
label = "Order"
[[fields]]
name = "customer"
type = "link"
target = "customer"
"#;

type Log = Arc<Mutex<Vec<String>>>;

/// Records the hooks it sees, tagged with its name; may reject a customer titled `banned`.
struct Recorder {
    who: &'static str,
    log: Log,
    rejects_banned: bool,
}

impl DocTypeHooks for Recorder {
    fn before_insert(&self, ctx: &HookContext<'_>, _doc: &mut Document) -> Result<(), HookError> {
        self.log
            .lock()
            .expect("log")
            .push(format!("{}:before_insert:{}", self.who, ctx.doctype));
        Ok(())
    }

    fn validate(&self, ctx: &HookContext<'_>, doc: &mut Document) -> Result<(), HookError> {
        self.log.lock().expect("log").push(format!(
            "{}:validate:{}:{}",
            self.who,
            ctx.doctype,
            match ctx.operation {
                Operation::Insert => "insert",
                Operation::Update => "update",
                _ => "other",
            }
        ));
        if self.rejects_banned && doc.values.get("title") == Some(&Value::Text("banned".to_owned()))
        {
            return Err(HookError::new("a banned customer is not allowed"));
        }
        Ok(())
    }
}

fn recorder(who: &'static str, log: &Log, rejects_banned: bool) -> Arc<dyn DocTypeHooks> {
    Arc::new(Recorder {
        who,
        log: log.clone(),
        rejects_banned,
    })
}

/// crm owns `customer`; sales owns `order` and extends `customer` with hooks of its own.
fn hooked_binary(log: &Log) -> Forgebase {
    let crm = App::new("crm")
        .doctypes(files![("doctypes/customer.toml", CUSTOMER)])
        .hooks("customer", recorder("crm1", log, false))
        .hooks("customer", recorder("crm2", log, false));
    let sales = App::new("sales")
        .depends_on(&["crm"])
        .doctypes(files![("doctypes/order.toml", ORDER)])
        .hooks("customer", recorder("sales", log, true));
    Forgebase::new().app(crm).app(sales)
}

#[tokio::test]
async fn hooks_run_in_app_order_then_registration_order_and_the_extending_app_can_reject() {
    let log = Log::default();
    let binary = hooked_binary(&log);
    let db = TestDb::new().await;
    db.migrate(&binary, false).await.expect("migrate");
    db.create_user("root", &["Administrator"]).await;
    let root = db.client(&binary).login("root").await;

    let reply = root
        .post(
            "/api/v1/resource/customer",
            json!({"code": "c-1", "title": "fine"}),
        )
        .await;
    assert_eq!(reply.status, 201, "{:?}", reply.body);
    // The app that owns the DocType first (in registration order), then the app that extends it.
    let expected: Vec<String> = [
        "crm1:before_insert:customer",
        "crm2:before_insert:customer",
        "sales:before_insert:customer",
        "crm1:validate:customer:insert",
        "crm2:validate:customer:insert",
        "sales:validate:customer:insert",
    ]
    .map(str::to_owned)
    .to_vec();
    assert_eq!(*log.lock().expect("log"), expected);

    // The extending app's hook rejects what the owning app accepted; nothing is stored.
    log.lock().expect("log").clear();
    let reply = root
        .post(
            "/api/v1/resource/customer",
            json!({"code": "c-2", "title": "banned"}),
        )
        .await;
    assert_eq!(reply.status, 422, "{:?}", reply.body);
    assert_eq!(reply.error_code(), Some("hook_rejected"));
    assert!(
        reply
            .error_message()
            .is_some_and(|m| m.contains("a banned customer is not allowed")),
        "{:?}",
        reply.body
    );
    assert_eq!(log.lock().expect("log").len(), 6);
    assert_eq!(
        db.rows("SELECT name FROM doc_customer ORDER BY name").await,
        rows(&[&["c-1"]])
    );
    db.cleanup().await;
}

// ---- ownership --------------------------------------------------------------------------------

#[tokio::test]
async fn the_metadata_api_says_which_app_owns_a_doctype() {
    let log = Log::default();
    let binary = hooked_binary(&log);
    let db = TestDb::new().await;
    db.migrate(&binary, false).await.expect("migrate");
    db.create_user("root", &["Administrator"]).await;
    let root = db.client(&binary).login("root").await;

    let reply = root.get("/api/v1/meta").await;
    assert_eq!(reply.status, 200);
    let owners: Vec<(String, String)> = reply
        .data()
        .as_array()
        .expect("array")
        .iter()
        .map(|d| {
            (
                d["name"].as_str().expect("name").to_owned(),
                d["app"].as_str().expect("app").to_owned(),
            )
        })
        .collect();
    assert_eq!(
        owners,
        [
            ("customer".to_owned(), "crm".to_owned()),
            ("order".to_owned(), "sales".to_owned())
        ]
    );
    assert_eq!(root.get("/api/v1/meta/customer").await.data()["app"], "crm");
    assert_eq!(root.get("/api/v1/meta/order").await.data()["app"], "sales");
    db.cleanup().await;
}

// ---- app migrations ---------------------------------------------------------------------------

const CRM_MIGRATIONS: &[MigrationFile] = &[
    migration(
        1,
        "create_log",
        "CREATE TABLE zz_log (id serial PRIMARY KEY, entry text NOT NULL); \
         INSERT INTO zz_log (entry) VALUES ('crm 1')",
    ),
    migration(2, "second", "INSERT INTO zz_log (entry) VALUES ('crm 2')"),
];

const SALES_MIGRATIONS: &[MigrationFile] = &[
    // Runs after the schema sync: the DocType table of crm exists.
    migration(
        1,
        "after_sync",
        "INSERT INTO zz_log (entry) SELECT 'sales 1, doc_customer has ' || count(*) FROM doc_customer",
    ),
    migration(
        5,
        "gap_in_versions_is_fine",
        "INSERT INTO zz_log (entry) VALUES ('sales 5')",
    ),
];

fn crm_with(migrations: &'static [MigrationFile]) -> App {
    App::new("crm")
        .version("1.0.0")
        .doctypes(files![("doctypes/customer.toml", CUSTOMER)])
        .migrations(migrations)
}

fn sales_with(migrations: &'static [MigrationFile]) -> App {
    App::new("sales")
        .version("2.0.0")
        .depends_on(&["crm"])
        .doctypes(files![("doctypes/order.toml", ORDER)])
        .migrations(migrations)
}

fn migrating_binary() -> Forgebase {
    Forgebase::new()
        .app(crm_with(CRM_MIGRATIONS))
        .app(sales_with(SALES_MIGRATIONS))
}

async fn log_entries(db: &TestDb) -> Vec<Vec<String>> {
    db.rows("SELECT entry FROM zz_log ORDER BY id").await
}

#[tokio::test]
async fn app_migrations_run_once_in_app_order_and_are_recorded() {
    let db = TestDb::new().await;
    let binary = migrating_binary();
    let done = db.migrate(&binary, false).await.expect("migrate");
    assert_eq!(
        done.app_migrations,
        applied(&[
            ("crm", 1, "create_log"),
            ("crm", 2, "second"),
            ("sales", 1, "after_sync"),
            ("sales", 5, "gap_in_versions_is_fine"),
        ])
    );
    assert_eq!(
        log_entries(&db).await,
        rows(&[
            &["crm 1"],
            &["crm 2"],
            &["sales 1, doc_customer has 0"],
            &["sales 5"]
        ])
    );
    assert_eq!(
        db.rows(
            "SELECT app, version::text, name, length(checksum)::text FROM fb_app_migration \
             ORDER BY app, version"
        )
        .await,
        rows(&[
            &["crm", "1", "create_log", "64"],
            &["crm", "2", "second", "64"],
            &["sales", "1", "after_sync", "64"],
            &["sales", "5", "gap_in_versions_is_fine", "64"],
        ])
    );
    assert_eq!(
        db.rows("SELECT name, version FROM fb_app ORDER BY name")
            .await,
        rows(&[&["crm", "1.0.0"], &["sales", "2.0.0"]])
    );

    // A second migrate skips everything that was applied.
    let again = db.migrate(&binary, false).await.expect("migrate again");
    assert!(again.app_migrations.is_empty());
    assert_eq!(log_entries(&db).await.len(), 4);

    // A new version of an app: its new migration runs, `fb_app` has the new version.
    const MORE: &[MigrationFile] = &[
        CRM_MIGRATIONS[0],
        CRM_MIGRATIONS[1],
        migration(3, "third", "INSERT INTO zz_log (entry) VALUES ('crm 3')"),
    ];
    let newer = Forgebase::new()
        .app(crm_with(MORE).version("1.1.0"))
        .app(sales_with(SALES_MIGRATIONS));
    let done = db.migrate(&newer, false).await.expect("migrate newer");
    assert_eq!(done.app_migrations, applied(&[("crm", 3, "third")]));
    assert_eq!(
        db.rows("SELECT version FROM fb_app WHERE name = 'crm'")
            .await,
        rows(&[&["1.1.0"]])
    );
    db.cleanup().await;
}

#[tokio::test]
async fn a_dry_run_lists_the_pending_migrations_and_keeps_nothing() {
    let db = TestDb::new().await;
    let binary = migrating_binary();
    // On an empty database the framework migrations are pending too: every declared migration
    // is listed, and nothing exists afterwards.
    let dry = db.migrate(&binary, true).await.expect("dry run");
    assert_eq!(dry.app_migrations.len(), 4);
    assert_eq!(
        db.rows("SELECT to_regclass('fb_app')::text, to_regclass('zz_log')::text")
            .await,
        rows(&[&["NULL", "NULL"]])
    );

    db.migrate(&binary, false).await.expect("migrate");
    // Up to date: nothing pending.
    assert!(
        db.migrate(&binary, true)
            .await
            .expect("dry")
            .app_migrations
            .is_empty()
    );

    // A new migration is listed but not run, and fb_app is not touched.
    const MORE: &[MigrationFile] = &[
        CRM_MIGRATIONS[0],
        CRM_MIGRATIONS[1],
        migration(3, "third", "INSERT INTO zz_log (entry) VALUES ('crm 3')"),
    ];
    let newer = Forgebase::new()
        .app(crm_with(MORE).version("9.9.9"))
        .app(sales_with(SALES_MIGRATIONS));
    let dry = db.migrate(&newer, true).await.expect("dry run");
    assert_eq!(dry.app_migrations, applied(&[("crm", 3, "third")]));
    assert_eq!(log_entries(&db).await.len(), 4);
    assert_eq!(
        db.rows("SELECT version FROM fb_app WHERE name = 'crm'")
            .await,
        rows(&[&["1.0.0"]])
    );
    db.cleanup().await;
}

#[tokio::test]
async fn a_changed_migration_is_refused_before_anything_else_is_applied() {
    let db = TestDb::new().await;
    db.migrate(&migrating_binary(), false)
        .await
        .expect("migrate");

    // crm 1 was edited; the same release adds a migration and a DocType.
    const EDITED: &[MigrationFile] = &[
        migration(
            1,
            "create_log",
            "CREATE TABLE zz_log (id serial PRIMARY KEY, entry text NOT NULL); \
             INSERT INTO zz_log (entry) VALUES ('crm 1, edited')",
        ),
        CRM_MIGRATIONS[1],
        migration(3, "third", "INSERT INTO zz_log (entry) VALUES ('crm 3')"),
    ];
    const WIDGET: &str = "name = \"widget\"\nlabel = \"W\"\n";
    let edited = Forgebase::new()
        .app(crm_with(EDITED).doctypes(files![("doctypes/widget.toml", WIDGET)]))
        .app(sales_with(SALES_MIGRATIONS));
    for dry_run in [false, true] {
        let error = db.migrate(&edited, dry_run).await.expect_err("refused");
        assert!(
            error
                .contains("migration 1 (create_log) of app `crm` was changed after it was applied"),
            "{error}"
        );
    }
    // Nothing was applied: no new migration, no schema sync, the app row has the old data.
    assert_eq!(log_entries(&db).await.len(), 4);
    assert_eq!(
        db.rows("SELECT count(*)::text FROM fb_app_migration").await,
        rows(&[&["4"]])
    );
    assert_eq!(
        db.rows("SELECT to_regclass('doc_widget')::text").await,
        rows(&[&["NULL"]])
    );
    db.cleanup().await;
}

#[tokio::test]
async fn a_failing_migration_is_rolled_back_and_stops_the_run() {
    const BROKEN: &[MigrationFile] = &[
        migration(1, "fine", "INSERT INTO zz_log (entry) VALUES ('sales 1')"),
        // Writes a row and then fails: the row must not stay.
        migration(
            2,
            "boom",
            "INSERT INTO zz_log (entry) VALUES ('sales 2'); SELECT 1 / 0",
        ),
        migration(3, "never", "INSERT INTO zz_log (entry) VALUES ('sales 3')"),
    ];
    let db = TestDb::new().await;
    let broken = Forgebase::new()
        .app(crm_with(CRM_MIGRATIONS))
        .app(sales_with(BROKEN));
    let error = db.migrate(&broken, false).await.expect_err("fails");
    assert!(
        error.contains("migration 2 (boom) of app `sales` failed and was rolled back")
            && error.contains("earlier migrations stay applied")
            && error.contains("division by zero"),
        "{error}"
    );
    // The migrations before the failing one stay; the failing one left nothing; the next did not run.
    assert_eq!(
        log_entries(&db).await,
        rows(&[&["crm 1"], &["crm 2"], &["sales 1"]])
    );
    assert_eq!(
        db.rows("SELECT app, version::text FROM fb_app_migration ORDER BY app, version")
            .await,
        rows(&[&["crm", "1"], &["crm", "2"], &["sales", "1"]])
    );
    // crm was done; sales was not, so its `fb_app` row is missing.
    assert_eq!(
        db.rows("SELECT name FROM fb_app ORDER BY name").await,
        rows(&[&["crm"]])
    );

    // Fixed (the failed migration was never recorded, so its SQL may change): the run continues.
    const FIXED: &[MigrationFile] = &[
        BROKEN[0],
        migration(2, "boom", "INSERT INTO zz_log (entry) VALUES ('sales 2')"),
        BROKEN[2],
    ];
    let fixed = Forgebase::new()
        .app(crm_with(CRM_MIGRATIONS))
        .app(sales_with(FIXED));
    let done = db.migrate(&fixed, false).await.expect("migrate");
    assert_eq!(
        done.app_migrations,
        applied(&[("sales", 2, "boom"), ("sales", 3, "never")])
    );
    assert_eq!(log_entries(&db).await.len(), 5);
    assert_eq!(
        db.rows("SELECT name FROM fb_app ORDER BY name").await,
        rows(&[&["crm"], &["sales"]])
    );
    db.cleanup().await;
}

#[tokio::test]
async fn a_migration_the_app_no_longer_declares_is_refused_and_absent_apps_are_left_alone() {
    let db = TestDb::new().await;
    db.migrate(&migrating_binary(), false)
        .await
        .expect("migrate");

    // crm dropped its migration 2: the database is ahead of the app.
    let behind = Forgebase::new()
        .app(crm_with(&CRM_MIGRATIONS[..1]))
        .app(sales_with(SALES_MIGRATIONS));
    let error = db.migrate(&behind, false).await.expect_err("refused");
    assert!(
        error.contains("migration 2 (second) of app `crm`") && error.contains("does not declare"),
        "{error}"
    );

    // A binary without sales leaves what sales recorded as it is.
    let crm_only = Forgebase::new().app(crm_with(CRM_MIGRATIONS));
    let done = db.migrate(&crm_only, false).await.expect("migrate");
    assert!(done.app_migrations.is_empty());
    assert_eq!(
        db.rows("SELECT name FROM fb_app ORDER BY name").await,
        rows(&[&["crm"], &["sales"]])
    );
    assert_eq!(
        db.rows("SELECT count(*)::text FROM fb_app_migration WHERE app = 'sales'")
            .await,
        rows(&[&["2"]])
    );
    db.cleanup().await;
}

#[tokio::test]
async fn migrate_reports_startup_errors_without_touching_the_database() {
    let db = TestDb::new().await;
    let invalid = Forgebase::new().app(sales_with(SALES_MIGRATIONS));
    let error = db
        .migrate(&invalid, false)
        .await
        .expect_err("startup error");
    assert!(
        error.contains("app `sales` depends on `crm`, which is not registered"),
        "{error}"
    );
    assert_eq!(
        db.rows("SELECT to_regclass('fb_migration')::text").await,
        rows(&[&["NULL"]])
    );
    db.cleanup().await;
}
