//! Fixtures end to end (M9.4, ADR 0022): `migrate` applies the records an app ships, through the
//! document pipeline, with a checksum per record in `fb_fixture`. Real PostgreSQL
//! (`forgebase::testing::TestDb`).
//!
//! Two apps: `base` has `customer_group` and `price_list` (with `price_line` rows), `ext` depends
//! on `base` and has `region`, which links to a `customer_group`. A new version of a fixture is a
//! second `FixtureFile` list built from another string.
//!
//! Every test fails loudly (never skips) when `FORGEBASE_TEST_DATABASE_URL` is not set.

use forgebase::testing::{Migrated, TestDb};
use forgebase::{
    App, DocTypeFile, DocTypeHooks, Document, FixtureFile, Forgebase, HookContext, HookError,
    MigrationFile, Value,
};

const GROUP: &str = r#"
name = "customer_group"
label = "Customer group"
naming = { field = "title" }
[[fields]]
name = "title"
type = "data"
required = true
unique = true
[[fields]]
name = "description"
type = "data"
max_length = 20
[[fields]]
name = "rank"
type = "int"
"#;

const LINE: &str = r#"
name = "price_line"
label = "Price line"
is_child = true
[[fields]]
name = "item"
type = "data"
[[fields]]
name = "price"
type = "int"
"#;

const LIST: &str = r#"
name = "price_list"
label = "Price list"
naming = { field = "code" }
[[fields]]
name = "code"
type = "data"
required = true
unique = true
[[fields]]
name = "group"
type = "link"
target = "customer_group"
[[fields]]
name = "lines"
type = "table"
target = "price_line"
"#;

const REGION: &str = r#"
name = "region"
label = "Region"
naming = { field = "code" }
[[fields]]
name = "code"
type = "data"
required = true
unique = true
[[fields]]
name = "group"
type = "link"
target = "customer_group"
[[fields]]
name = "label"
type = "data"
"#;

const SEED: MigrationFile = MigrationFile::new(
    1,
    "seed",
    "migrations/0001_seed.sql",
    "CREATE TABLE base_seed (entry text)",
);

// ---- fixture versions ---------------------------------------------------------------------------

const GROUPS_V1: &str = r#"{"doctype": "customer_group", "records": [
    {"title": "Retail", "description": "d1"},
    {"title": "Wholesale", "description": "w1", "rank": 2}
]}"#;
/// Retail changes (and loses `rank`, which it never had), Wholesale is the same with other
/// whitespace and key order.
const GROUPS_V2: &str = r#"{"records": [
    {"description": "d2", "title": "Retail"},
    {"rank": 2,"description":"w1","title":"Wholesale"}
], "doctype": "customer_group"}"#;
/// Wholesale left the file.
const GROUPS_V3: &str = r#"{"doctype": "customer_group", "records": [
    {"title": "Retail", "description": "d2"}
]}"#;
/// A new record next to a changed one.
const GROUPS_V4: &str = r#"{"doctype": "customer_group", "records": [
    {"title": "Retail", "description": "d3"},
    {"title": "Wholesale", "description": "w1", "rank": 2},
    {"title": "Online"}
]}"#;
const GROUPS_BAD_SECOND: &str = r#"{"doctype": "customer_group", "records": [
    {"title": "Retail"},
    {"title": "Wholesale", "description": "this value is much too long for the field"}
]}"#;

const LISTS_V1: &str = r#"{"doctype": "price_list", "records": [
    {"code": "std", "group": "Retail", "lines": [{"item": "a", "price": 1}, {"item": "b", "price": 2}]}
]}"#;
const LISTS_V2: &str = r#"{"doctype": "price_list", "records": [
    {"code": "std", "lines": [{"item": "c", "price": 3}]}
]}"#;
const LISTS_BAD_LINK: &str = r#"{"doctype": "price_list", "records": [
    {"code": "std", "group": "No such group"}
]}"#;

const REGIONS_V1: &str = r#"{"doctype": "region", "records": [
    {"code": "north", "group": "Retail", "label": "North"}
]}"#;

const GROUP_FILES_V1: &[FixtureFile] =
    &[FixtureFile::new("fixtures/customer_group.json", GROUPS_V1)];
const GROUP_FILES_V2: &[FixtureFile] =
    &[FixtureFile::new("fixtures/customer_group.json", GROUPS_V2)];
const GROUP_FILES_V3: &[FixtureFile] =
    &[FixtureFile::new("fixtures/customer_group.json", GROUPS_V3)];
const GROUP_FILES_V4: &[FixtureFile] =
    &[FixtureFile::new("fixtures/customer_group.json", GROUPS_V4)];
const GROUP_FILES_BAD: &[FixtureFile] = &[FixtureFile::new(
    "fixtures/customer_group.json",
    GROUPS_BAD_SECOND,
)];
const LIST_FILES_V1: &[FixtureFile] = &[FixtureFile::new("fixtures/price_list.json", LISTS_V1)];
const LIST_FILES_V2: &[FixtureFile] = &[FixtureFile::new("fixtures/price_list.json", LISTS_V2)];
const LIST_FILES_BAD: &[FixtureFile] =
    &[FixtureFile::new("fixtures/price_list.json", LISTS_BAD_LINK)];
const REGION_FILES: &[FixtureFile] = &[FixtureFile::new("fixtures/region.json", REGIONS_V1)];

/// Sets `rank` to 99 on a group that has none: proof that the hooks run for a fixture.
struct Rank99;

impl DocTypeHooks for Rank99 {
    fn validate(&self, _ctx: &HookContext<'_>, doc: &mut Document) -> Result<(), HookError> {
        let missing = matches!(doc.values.get("rank"), None | Some(Value::Null));
        if missing {
            doc.values.insert("rank".to_owned(), Value::Int(99));
        }
        Ok(())
    }
}

fn base(groups: &'static [FixtureFile], lists: &'static [FixtureFile]) -> App {
    App::new("base")
        .doctypes(
            const {
                &[
                    DocTypeFile::new("doctypes/customer_group.toml", GROUP),
                    DocTypeFile::new("doctypes/price_line.toml", LINE),
                    DocTypeFile::new("doctypes/price_list.toml", LIST),
                ]
            },
        )
        .migrations(&[SEED])
        .fixtures(groups)
        .fixtures(lists)
}

fn ext(regions: &'static [FixtureFile]) -> App {
    App::new("ext")
        .depends_on(&["base"])
        .doctypes(const { &[DocTypeFile::new("doctypes/region.toml", REGION)] })
        .fixtures(regions)
}

fn binary(groups: &'static [FixtureFile]) -> Forgebase {
    Forgebase::new()
        .app(base(groups, &[]))
        .app(ext(REGION_FILES))
}

/// `(app, inserted, updated, kept, unchanged)` of each app that has fixtures.
fn summary(migrated: &Migrated) -> Vec<(&str, usize, usize, usize, usize)> {
    migrated
        .fixtures
        .iter()
        .map(|f| (f.app.as_str(), f.inserted, f.updated, f.kept, f.unchanged))
        .collect()
}

async fn migrate(db: &TestDb, binary: &Forgebase) -> Migrated {
    db.migrate(binary, false).await.expect("migrate")
}

/// The groups as `title|description|rank|version|modified_by`, by title.
async fn groups(db: &TestDb) -> Vec<String> {
    db.rows(
        "SELECT concat_ws('|', title, coalesce(description, '-'), coalesce(rank::text, '-'), \
         version::text, modified_by) FROM doc_customer_group ORDER BY title",
    )
    .await
    .into_iter()
    .map(|row| row[0].clone())
    .collect()
}

async fn scalar(db: &TestDb, sql: &str) -> String {
    db.rows(sql).await[0][0].clone()
}

async fn checksum_of(db: &TestDb, name: &str) -> String {
    scalar(
        db,
        &format!(
            "SELECT checksum FROM fb_fixture WHERE doctype = 'customer_group' AND name = '{name}'"
        ),
    )
    .await
}

/// Everything a write would change: the documents, the fixture table and the job table, with the
/// row versions of PostgreSQL (`xmin`), so that even a write of the same values shows.
async fn snapshot(db: &TestDb) -> Vec<Vec<String>> {
    db.rows(
        "SELECT 'group', name, version::text, modified_at::text, xmin::text FROM doc_customer_group \
         UNION ALL SELECT 'region', name, version::text, modified_at::text, xmin::text FROM doc_region \
         UNION ALL SELECT 'fixture', doctype || '/' || name, checksum, applied_at::text, xmin::text FROM fb_fixture \
         UNION ALL SELECT 'job', id::text, name, status, xmin::text FROM fb_job \
         ORDER BY 1, 2",
    )
    .await
}

#[tokio::test]
async fn the_first_migrate_inserts_the_records_and_counts_them() {
    let db = TestDb::new().await;
    let migrated = migrate(&db, &binary(GROUP_FILES_V1)).await;
    assert_eq!(
        summary(&migrated),
        vec![("base", 2, 0, 0, 0), ("ext", 1, 0, 0, 0)]
    );
    assert_eq!(
        groups(&db).await,
        vec![
            "Retail|d1|-|1|fixture:base".to_owned(),
            "Wholesale|w1|2|1|fixture:base".to_owned()
        ]
    );
    // `ext` came after `base`: its record links to a `base` record.
    assert_eq!(
        db.rows("SELECT r.code, (SELECT g.name FROM doc_customer_group g WHERE g.id = r.\"group\"), r.label, r.modified_by FROM doc_region r")
            .await,
        vec![vec![
            "north".to_owned(),
            "Retail".into(),
            "North".into(),
            "fixture:ext".into()
        ]]
    );
    // One row per record, with the app and a SHA-256.
    assert_eq!(
        db.rows(
            "SELECT doctype, name, app, length(checksum)::text FROM fb_fixture ORDER BY doctype, name"
        )
        .await,
        vec![
            vec!["customer_group".to_owned(), "Retail".into(), "base".into(), "64".into()],
            vec!["customer_group".to_owned(), "Wholesale".into(), "base".into(), "64".into()],
            vec!["region".to_owned(), "north".into(), "ext".into(), "64".into()],
        ]
    );
    // The app migrations ran before the fixtures and stay.
    assert_eq!(
        scalar(&db, "SELECT to_regclass('base_seed')::text").await,
        "base_seed"
    );
    db.cleanup().await;
}

#[tokio::test]
async fn the_second_migrate_changes_nothing() {
    let db = TestDb::new().await;
    // A webhook on the DocType: every fixture write would enqueue a delivery.
    migrate(&db, &Forgebase::new()).await;
    db.exec(
        "INSERT INTO fb_webhook (name, doctype, events, url, secret) \
         VALUES ('w', 'customer_group', '{insert,update}', 'https://example.com/x', 's')",
    )
    .await;
    let binary = binary(GROUP_FILES_V1);
    migrate(&db, &binary).await;
    // The first migrate wrote two deliveries, one per inserted group.
    assert_eq!(scalar(&db, "SELECT count(*)::text FROM fb_job").await, "2");
    let before = snapshot(&db).await;

    let again = migrate(&db, &binary).await;
    assert_eq!(
        summary(&again),
        vec![("base", 0, 0, 0, 2), ("ext", 0, 0, 0, 1)]
    );
    assert_eq!(
        snapshot(&db).await,
        before,
        "the second migrate wrote something"
    );
    db.cleanup().await;
}

#[tokio::test]
async fn a_site_edit_is_kept_until_the_fixture_changes() {
    let db = TestDb::new().await;
    migrate(&db, &binary(GROUP_FILES_V1)).await;
    db.exec(
        "UPDATE doc_customer_group SET description = 'mine', rank = 7, version = version + 1 \
         WHERE title = 'Retail'",
    )
    .await;
    let again = migrate(&db, &binary(GROUP_FILES_V1)).await;
    assert_eq!(summary(&again)[0], ("base", 0, 0, 0, 2));
    assert_eq!(
        groups(&db).await[0],
        "Retail|mine|7|2|fixture:base",
        "the site's edit was overwritten"
    );
    // V2 changes Retail only (Wholesale differs in whitespace and key order, which is no change).
    let v2 = migrate(&db, &binary(GROUP_FILES_V2)).await;
    assert_eq!(summary(&v2)[0], ("base", 0, 1, 0, 1));
    db.cleanup().await;
}

#[tokio::test]
async fn a_changed_fixture_updates_only_the_fields_it_has() {
    let db = TestDb::new().await;
    migrate(&db, &binary(GROUP_FILES_V1)).await;
    let before = checksum_of(&db, "Retail").await;
    let wholesale = checksum_of(&db, "Wholesale").await;
    // The site sets a field the fixture does not have, and changes one it has.
    db.exec(
        "UPDATE doc_customer_group SET rank = 5, description = 'mine', version = version + 1 \
         WHERE title = 'Retail'",
    )
    .await;

    let v2 = migrate(&db, &binary(GROUP_FILES_V2)).await;
    assert_eq!(summary(&v2)[0], ("base", 0, 1, 0, 1));
    // `description` is the new version's, `rank` is the site's, and the version moved on by one
    // (the site's edit made it 2).
    assert_eq!(groups(&db).await[0], "Retail|d2|5|3|fixture:base");
    assert_ne!(checksum_of(&db, "Retail").await, before);
    assert_eq!(checksum_of(&db, "Wholesale").await, wholesale);
    // The record that did not change was not written.
    assert_eq!(groups(&db).await[1], "Wholesale|w1|2|1|fixture:base");

    let again = migrate(&db, &binary(GROUP_FILES_V2)).await;
    assert_eq!(summary(&again)[0], ("base", 0, 0, 0, 2));
    assert_eq!(groups(&db).await[0], "Retail|d2|5|3|fixture:base");
    db.cleanup().await;
}

#[tokio::test]
async fn a_document_that_existed_before_the_fixture_is_adopted() {
    let db = TestDb::new().await;
    // The site has the app without fixtures and creates "Retail" itself.
    let without = Forgebase::new().app(base(&[], &[])).app(ext(&[]));
    migrate(&db, &without).await;
    db.exec(
        "INSERT INTO doc_customer_group (id, name, owner, created_at, modified_at, modified_by, version, title, description) \
         VALUES (gen_random_uuid(), 'Retail', 'ann', now(), now(), 'ann', 1, 'Retail', 'site made')",
    )
    .await;

    let migrated = migrate(&db, &binary(GROUP_FILES_V1)).await;
    // Retail is kept as it is; Wholesale is new.
    assert_eq!(summary(&migrated)[0], ("base", 1, 0, 1, 0));
    assert_eq!(
        groups(&db).await,
        vec![
            "Retail|site made|-|1|ann".to_owned(),
            "Wholesale|w1|2|1|fixture:base".to_owned()
        ]
    );
    assert_eq!(checksum_of(&db, "Retail").await.len(), 64);
    // From now on it is tracked like the others: unchanged, until the app ships a new version.
    let again = migrate(&db, &binary(GROUP_FILES_V1)).await;
    assert_eq!(summary(&again)[0], ("base", 0, 0, 0, 2));
    let v2 = migrate(&db, &binary(GROUP_FILES_V2)).await;
    assert_eq!(summary(&v2)[0], ("base", 0, 1, 0, 1));
    assert_eq!(groups(&db).await[0], "Retail|d2|-|2|fixture:base");
    db.cleanup().await;
}

#[tokio::test]
async fn a_document_the_site_deleted_comes_back_only_with_a_new_version() {
    let db = TestDb::new().await;
    migrate(&db, &binary(GROUP_FILES_V1)).await;
    // `region` links to Retail: delete the region first, then the group.
    db.exec("DELETE FROM doc_region; DELETE FROM doc_customer_group WHERE title = 'Retail'")
        .await;

    let again = migrate(&db, &binary(GROUP_FILES_V1)).await;
    assert_eq!(summary(&again)[0], ("base", 0, 0, 0, 2));
    assert_eq!(
        groups(&db).await,
        vec!["Wholesale|w1|2|1|fixture:base".to_owned()],
        "an unchanged fixture brought a deleted document back"
    );
    // The app ships a new version of the record: it is inserted again.
    let v2 = migrate(&db, &binary(GROUP_FILES_V2)).await;
    assert_eq!(summary(&v2)[0], ("base", 1, 0, 0, 1));
    assert_eq!(
        groups(&db).await,
        vec![
            "Retail|d2|-|1|fixture:base".to_owned(),
            "Wholesale|w1|2|1|fixture:base".to_owned()
        ]
    );
    db.cleanup().await;
}

#[tokio::test]
async fn a_record_removed_from_the_fixture_is_never_deleted() {
    let db = TestDb::new().await;
    migrate(&db, &binary(GROUP_FILES_V1)).await;
    let checksum = checksum_of(&db, "Wholesale").await;
    let v3 = migrate(&db, &binary(GROUP_FILES_V3)).await;
    assert_eq!(summary(&v3)[0], ("base", 0, 1, 0, 0));
    assert_eq!(
        groups(&db).await,
        vec![
            "Retail|d2|-|2|fixture:base".to_owned(),
            "Wholesale|w1|2|1|fixture:base".to_owned()
        ]
    );
    // The row stays, with the checksum it had.
    assert_eq!(checksum_of(&db, "Wholesale").await, checksum);
    // And a later version that has it again (same content) finds it unchanged.
    let v1 = migrate(&db, &binary(GROUP_FILES_V1)).await;
    assert_eq!(summary(&v1)[0], ("base", 0, 1, 0, 1));
    db.cleanup().await;
}

#[tokio::test]
async fn a_new_record_is_inserted_next_to_a_changed_one() {
    let db = TestDb::new().await;
    migrate(&db, &binary(GROUP_FILES_V1)).await;
    let v4 = migrate(&db, &binary(GROUP_FILES_V4)).await;
    assert_eq!(summary(&v4)[0], ("base", 1, 1, 0, 1));
    assert_eq!(groups(&db).await.len(), 3);
    db.cleanup().await;
}

#[tokio::test]
async fn table_rows_are_inserted_and_replaced_when_the_record_changes() {
    let db = TestDb::new().await;
    let with_lists = |groups, lists| Forgebase::new().app(base(groups, lists));
    migrate(&db, &with_lists(GROUP_FILES_V1, LIST_FILES_V1)).await;
    let lines = || {
        db.rows(
            "SELECT string_agg(l.item || ':' || l.price, ',' ORDER BY l.idx) FROM doc_price_list p \
             JOIN doc_price_line l ON l.parent_id = p.id WHERE p.code = 'std'",
        )
    };
    assert_eq!(lines().await, vec![vec!["a:1,b:2".to_owned()]]);

    // V2 gives other rows and no `group`: the rows are replaced, the link stays as it was.
    let v2 = migrate(&db, &with_lists(GROUP_FILES_V1, LIST_FILES_V2)).await;
    assert_eq!(summary(&v2)[0], ("base", 0, 1, 0, 2));
    assert_eq!(lines().await, vec![vec!["c:3".to_owned()]]);
    assert_eq!(
        scalar(
            &db,
            "SELECT g.name FROM doc_price_list p JOIN doc_customer_group g ON g.id = p.\"group\" WHERE p.code = 'std'"
        )
        .await,
        "Retail"
    );
    db.cleanup().await;
}

#[tokio::test]
async fn a_record_that_fails_validation_rolls_back_the_fixtures_of_its_app() {
    let db = TestDb::new().await;
    let bad = Forgebase::new().app(base(GROUP_FILES_BAD, &[]));
    let error = db
        .migrate(&bad, false)
        .await
        .expect_err("migrate must fail");
    let message = error.to_string();
    // It names the app, the file, the record and the problem.
    assert!(
        message.contains("app `base`")
            && message.contains("fixtures/customer_group.json")
            && message.contains("record 2 (`Wholesale`)")
            && message.contains("description"),
        "{message}"
    );
    // The first record was valid, but nothing from the app's fixtures was kept.
    assert_eq!(
        scalar(&db, "SELECT count(*)::text FROM doc_customer_group").await,
        "0"
    );
    assert_eq!(
        scalar(&db, "SELECT count(*)::text FROM fb_fixture").await,
        "0"
    );
    // The migrations before it stay applied, as they do for any later failure.
    assert_eq!(
        scalar(&db, "SELECT to_regclass('base_seed')::text").await,
        "base_seed"
    );
    assert_eq!(
        scalar(
            &db,
            "SELECT count(*)::text FROM fb_app_migration WHERE app = 'base'"
        )
        .await,
        "1"
    );

    // A link to a record nobody has fails the same way, and `base` is rolled back as a whole.
    let bad_link = Forgebase::new().app(base(GROUP_FILES_V1, LIST_FILES_BAD));
    let error = db
        .migrate(&bad_link, false)
        .await
        .expect_err("migrate must fail");
    assert!(error.to_string().contains("price_list.json"), "{error}");
    assert_eq!(
        scalar(&db, "SELECT count(*)::text FROM doc_customer_group").await,
        "0"
    );

    // Fixed, it applies.
    migrate(&db, &binary(GROUP_FILES_V1)).await;
    assert_eq!(
        scalar(&db, "SELECT count(*)::text FROM doc_customer_group").await,
        "2"
    );
    db.cleanup().await;
}

#[tokio::test]
async fn each_app_has_its_own_transaction() {
    let db = TestDb::new().await;
    const BAD_REGIONS: &[FixtureFile] = &[FixtureFile::new(
        "fixtures/region.json",
        r#"{"doctype": "region", "records": [{"code": "n", "group": "Nowhere"}]}"#,
    )];
    let binary = Forgebase::new()
        .app(base(GROUP_FILES_V1, &[]))
        .app(ext(BAD_REGIONS));
    let error = db.migrate(&binary, false).await.expect_err("ext fails");
    assert!(error.to_string().contains("app `ext`"), "{error}");
    // `base` came first and committed; `ext` kept nothing.
    assert_eq!(
        scalar(&db, "SELECT count(*)::text FROM doc_customer_group").await,
        "2"
    );
    assert_eq!(
        scalar(&db, "SELECT count(*)::text FROM doc_region").await,
        "0"
    );
    assert_eq!(
        db.rows("SELECT DISTINCT app FROM fb_fixture").await,
        vec![vec!["base".to_owned()]]
    );
    db.cleanup().await;
}

#[tokio::test]
async fn fixtures_go_through_the_hooks_and_fire_webhooks() {
    let db = TestDb::new().await;
    migrate(&db, &Forgebase::new()).await;
    db.exec(
        "INSERT INTO fb_webhook (name, doctype, events, url, secret) \
         VALUES ('w', 'customer_group', '{insert,update}', 'https://example.com/x', 's')",
    )
    .await;
    let hooked = |groups| Forgebase::new().app(base(groups, &[]).hooks("customer_group", Rank99));
    migrate(&db, &hooked(GROUP_FILES_V1)).await;
    // The `validate` hook filled `rank` of Retail (Wholesale has its own).
    assert_eq!(
        groups(&db).await,
        vec![
            "Retail|d1|99|1|fixture:base".to_owned(),
            "Wholesale|w1|2|1|fixture:base".to_owned()
        ]
    );
    // One delivery per insert, as for any write.
    assert_eq!(
        scalar(
            &db,
            "SELECT count(*)::text FROM fb_job WHERE name = 'forgebase.deliver_webhook'"
        )
        .await,
        "2"
    );
    // An update of a record is a write too.
    migrate(&db, &hooked(GROUP_FILES_V2)).await;
    assert_eq!(
        scalar(
            &db,
            "SELECT count(*)::text FROM fb_job WHERE name = 'forgebase.deliver_webhook'"
        )
        .await,
        "3"
    );
    db.cleanup().await;
}

#[tokio::test]
async fn a_dry_run_reports_the_plan_and_writes_nothing() {
    let db = TestDb::new().await;
    let binary = binary(GROUP_FILES_V1);

    // On an empty database the framework migrations are pending: the fixtures are listed, not
    // planned, and nothing was created.
    let dry = db.migrate(&binary, true).await.expect("dry run");
    assert_eq!(dry.fixtures.len(), 2);
    assert_eq!(
        scalar(&db, "SELECT to_regclass('fb_fixture')::text").await,
        "NULL"
    );

    // With only the framework tables, the DocType tables are missing too: all would be inserted.
    migrate(&db, &Forgebase::new()).await;
    let dry = db.migrate(&binary, true).await.expect("dry run");
    assert_eq!(
        summary(&dry),
        vec![("base", 2, 0, 0, 0), ("ext", 1, 0, 0, 0)]
    );
    assert_eq!(
        scalar(&db, "SELECT to_regclass('doc_customer_group')::text").await,
        "NULL"
    );

    // After a real migrate: a new version and a site-made document.
    migrate(&db, &binary).await;
    db.exec("DELETE FROM doc_region").await;
    let before = snapshot(&db).await;
    let dry = db
        .migrate(
            &Forgebase::new()
                .app(base(GROUP_FILES_V4, &[]))
                .app(ext(REGION_FILES)),
            true,
        )
        .await
        .expect("dry run");
    // Retail changed, Online is new, Wholesale did not change; the region is unchanged for the
    // checksum even though its document is gone.
    assert_eq!(
        summary(&dry),
        vec![("base", 1, 1, 0, 1), ("ext", 0, 0, 0, 1)]
    );
    assert_eq!(snapshot(&db).await, before, "the dry run wrote something");
    assert_eq!(groups(&db).await.len(), 2);
    // The real run does what the plan said.
    let real = migrate(
        &db,
        &Forgebase::new()
            .app(base(GROUP_FILES_V4, &[]))
            .app(ext(REGION_FILES)),
    )
    .await;
    assert_eq!(summary(&real), summary(&dry));
    db.cleanup().await;
}

#[tokio::test]
async fn an_unchanged_fixture_is_not_compared_with_its_document() {
    // The checksum decides, not the document: a deleted document is no reason to write.
    let db = TestDb::new().await;
    let binary = binary(GROUP_FILES_V1);
    migrate(&db, &binary).await;
    db.exec("DELETE FROM doc_region").await;
    let again = migrate(&db, &binary).await;
    assert_eq!(
        summary(&again),
        vec![("base", 0, 0, 0, 2), ("ext", 0, 0, 0, 1)]
    );
    db.cleanup().await;
}
