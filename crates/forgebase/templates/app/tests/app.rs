//! Tests of the `{{name}}` app. The last one needs PostgreSQL (see README.md) and fails, it does
//! not skip, when `FORGEBASE_TEST_DATABASE_URL` is not set.

use forgebase::testing::{TestDb, assert_doctypes_listed, json};
use forgebase::{DocTypeHooks, Document, Forgebase, HookContext, Operation, Value};

use {{name}}::{NoteRules, app};

const DOCTYPE: &str = "{{name}}_note";

#[test]
fn every_doctype_file_is_listed() {
    assert_doctypes_listed(&app(), env!("CARGO_MANIFEST_DIR"), "doctypes");
}

#[test]
fn the_app_fits_together() {
    Forgebase::new().app(app()).check().expect("valid app");
}

#[test]
fn the_title_is_trimmed_and_may_not_be_blank() {
    let ctx = HookContext::new(DOCTYPE, "ann", Operation::Insert);
    let mut doc = Document::default();
    doc.values
        .insert("title".to_owned(), Value::Text("  Hello  ".to_owned()));
    NoteRules.validate(&ctx, &mut doc).expect("valid");
    assert_eq!(doc.values["title"], Value::Text("Hello".to_owned()));

    doc.values
        .insert("title".to_owned(), Value::Text("   ".to_owned()));
    assert!(NoteRules.validate(&ctx, &mut doc).is_err());
}

#[tokio::test]
async fn a_user_can_create_and_read_a_note() {
    let binary = Forgebase::new().app(app());
    let db = TestDb::new().await;
    db.migrate(&binary, false).await.expect("migrate");
    db.create_user("ann", &["{{title}} User"]).await;
    let ann = db.client(&binary).login("ann").await;

    let path = format!("/api/v1/resource/{DOCTYPE}");
    let created = ann.post(&path, json!({"title": "  Hello  "})).await;
    assert_eq!(created.status, 201, "{:?}", created.body);
    let name = created.data()["name"].as_str().expect("name").to_owned();

    let read = ann.get(&format!("{path}/{name}")).await;
    assert_eq!(read.status, 200, "{:?}", read.body);
    assert_eq!(read.data()["title"], "Hello"); // trimmed by the hook
    db.cleanup().await;
}
