//! Link titles (UX.4) against a real PostgreSQL (see `common`): titles of readable targets, the
//! caller's read rules (deny and `if_owner`), targets without a `title_field`, child-row links
//! and a page of many documents.

mod common;

use std::collections::BTreeMap;

use common::{TestDb, registry};
use forgebase_core::document::{Document, Row};
use forgebase_core::meta::Registry;
use forgebase_core::permission::{ADMINISTRATOR, Caller};
use forgebase_core::value::Value;
use forgebase_db::{DbError, ListQuery, StoredDocument, get, insert, link_titles, list};

/// A titled target everybody with a role reads: `Reader` all, `OwnReader` only own documents.
const CLIENT: &str = r#"
name = "client"
label = "Client"
naming = { field = "code" }
title_field = "full_name"
[[fields]]
name = "code"
type = "data"
required = true
unique = true
[[fields]]
name = "full_name"
type = "data"
[[permissions]]
role = "Reader"
read = true
[[permissions]]
role = "OwnReader"
read = true
if_owner = true
"#;

/// A target without a `title_field`.
const PLAIN: &str = r#"
name = "plain"
label = "Plain"
naming = { field = "code" }
[[fields]]
name = "code"
type = "data"
required = true
unique = true
[[permissions]]
role = "Reader"
read = true
"#;

/// A titled target that only the administrator may read (no rules).
const HIDDEN: &str = r#"
name = "hidden"
label = "Hidden"
naming = { field = "code" }
title_field = "full_name"
[[fields]]
name = "code"
type = "data"
required = true
unique = true
[[fields]]
name = "full_name"
type = "data"
"#;

/// A titled target whose title is a Link: shown as the name of what it points to.
const BADGE: &str = r#"
name = "badge"
label = "Badge"
naming = { field = "code" }
title_field = "holder"
[[fields]]
name = "code"
type = "data"
required = true
unique = true
[[fields]]
name = "holder"
type = "link"
target = "plain"
[[permissions]]
role = "Reader"
read = true
"#;

const INVOICE: &str = r#"
name = "invoice"
label = "Invoice"
[[fields]]
name = "client"
type = "link"
target = "client"
[[fields]]
name = "plain"
type = "link"
target = "plain"
[[fields]]
name = "hidden"
type = "link"
target = "hidden"
[[fields]]
name = "badge"
type = "link"
target = "badge"
[[fields]]
name = "lines"
type = "table"
target = "invoice_line"
[[permissions]]
role = "Reader"
read = true
create = true
[[permissions]]
role = "OwnReader"
read = true
create = true
"#;

const INVOICE_LINE: &str = r#"
name = "invoice_line"
label = "Invoice Line"
is_child = true
[[fields]]
name = "client"
type = "link"
target = "client"
"#;

fn text(s: &str) -> Value {
    Value::Text(s.to_owned())
}

fn user(name: &str, roles: &[&str]) -> Caller {
    Caller::user(name, roles.iter().map(|r| (*r).to_owned()).collect())
}

fn admin() -> Caller {
    user("root", &[ADMINISTRATOR])
}

async fn setup() -> (TestDb, Registry) {
    let reg = registry(&[CLIENT, PLAIN, HIDDEN, BADGE, INVOICE, INVOICE_LINE]);
    let db = TestDb::migrated().await;
    db.sync(&reg).await.expect("sync");
    (db, reg)
}

async fn make(
    db: &TestDb,
    reg: &Registry,
    doctype: &str,
    pairs: &[(&str, Value)],
    tables: &[(&str, Vec<Row>)],
    owner: &Caller,
) -> StoredDocument {
    let mut client = db.pool.get().await.unwrap();
    insert(
        &mut **client,
        reg,
        doctype,
        Document {
            name: None,
            values: pairs
                .iter()
                .map(|(k, v)| ((*k).to_owned(), v.clone()))
                .collect(),
            tables: tables
                .iter()
                .map(|(k, v)| ((*k).to_owned(), v.clone()))
                .collect(),
        },
        owner,
    )
    .await
    .unwrap()
}

fn line(client: &str) -> Row {
    BTreeMap::from([("client".to_owned(), text(client))])
}

async fn titles(
    db: &TestDb,
    reg: &Registry,
    caller: &Caller,
    documents: &[StoredDocument],
) -> forgebase_db::LinkTitles {
    let client = db.pool.get().await.unwrap();
    link_titles(&**client, reg, caller, "invoice", documents)
        .await
        .unwrap()
}

#[tokio::test]
async fn readable_targets_get_titles_and_others_get_no_entry() {
    let (db, reg) = setup().await;
    let sys = Caller::system("sys");
    make(
        &db,
        &reg,
        "client",
        &[("code", text("C-1")), ("full_name", text("Ada Lovelace"))],
        &[],
        &sys,
    )
    .await;
    make(&db, &reg, "plain", &[("code", text("P-1"))], &[], &sys).await;
    make(
        &db,
        &reg,
        "hidden",
        &[("code", text("H-1")), ("full_name", text("Secret Name"))],
        &[],
        &sys,
    )
    .await;
    make(
        &db,
        &reg,
        "badge",
        &[("code", text("B-1")), ("holder", text("P-1"))],
        &[],
        &sys,
    )
    .await;
    let invoice = make(
        &db,
        &reg,
        "invoice",
        &[
            ("client", text("C-1")),
            ("plain", text("P-1")),
            ("hidden", text("H-1")),
            ("badge", text("B-1")),
        ],
        &[],
        &sys,
    )
    .await;

    let reader = user("rita", &["Reader"]);
    let got = titles(&db, &reg, &reader, std::slice::from_ref(&invoice)).await;
    // The client has a title; the Link title of the badge is the name of what it points to.
    assert_eq!(
        got,
        BTreeMap::from([
            (
                "client".to_owned(),
                BTreeMap::from([("C-1".to_owned(), "Ada Lovelace".to_owned())])
            ),
            (
                "badge".to_owned(),
                BTreeMap::from([("B-1".to_owned(), "P-1".to_owned())])
            ),
        ]),
        "no entry for `plain` (no title_field) or `hidden` (no read right)"
    );

    // The administrator may read the hidden target.
    let got = titles(&db, &reg, &admin(), &[invoice]).await;
    assert_eq!(got["hidden"]["H-1"], "Secret Name");
    assert!(!got.contains_key("plain"));
}

#[tokio::test]
async fn an_if_owner_target_gives_titles_only_for_own_documents() {
    let (db, reg) = setup().await;
    let (rita, sam) = (user("rita", &["OwnReader"]), user("sam", &["OwnReader"]));
    make(
        &db,
        &reg,
        "client",
        &[("code", text("C-RITA")), ("full_name", text("Rita's"))],
        &[],
        &Caller::system("rita"),
    )
    .await;
    make(
        &db,
        &reg,
        "client",
        &[("code", text("C-SAM")), ("full_name", text("Sam's"))],
        &[],
        &Caller::system("sam"),
    )
    .await;
    let rows = [("C-RITA", "a"), ("C-SAM", "b")];
    let mut invoices = Vec::new();
    for (client, _) in rows {
        invoices.push(
            make(
                &db,
                &reg,
                "invoice",
                &[("client", text(client))],
                &[],
                &Caller::system("sys"),
            )
            .await,
        );
    }
    let got = titles(&db, &reg, &rita, &invoices).await;
    assert_eq!(
        got["client"],
        BTreeMap::from([("C-RITA".to_owned(), "Rita's".to_owned())]),
        "sam's client has no title for rita"
    );
    let got = titles(&db, &reg, &sam, &invoices).await;
    assert_eq!(
        got["client"],
        BTreeMap::from([("C-SAM".to_owned(), "Sam's".to_owned())])
    );
}

#[tokio::test]
async fn a_caller_without_any_role_gets_nothing() {
    let (db, reg) = setup().await;
    make(
        &db,
        &reg,
        "client",
        &[("code", text("C-1")), ("full_name", text("Ada"))],
        &[],
        &Caller::system("sys"),
    )
    .await;
    let invoice = make(
        &db,
        &reg,
        "invoice",
        &[("client", text("C-1"))],
        &[],
        &Caller::system("sys"),
    )
    .await;
    let nobody = user("nobody", &["Other"]);
    assert!(titles(&db, &reg, &nobody, &[invoice]).await.is_empty());
}

#[tokio::test]
async fn child_row_links_are_covered_and_a_null_title_is_omitted() {
    let (db, reg) = setup().await;
    let sys = Caller::system("sys");
    make(
        &db,
        &reg,
        "client",
        &[("code", text("C-1")), ("full_name", text("Ada"))],
        &[],
        &sys,
    )
    .await;
    // No full_name: a null title is omitted.
    make(&db, &reg, "client", &[("code", text("C-2"))], &[], &sys).await;
    let invoice = make(
        &db,
        &reg,
        "invoice",
        &[],
        &[("lines", vec![line("C-1"), line("C-2"), line("C-1")])],
        &sys,
    )
    .await;
    let mut client = db.pool.get().await.unwrap();
    let stored = get(&mut **client, &reg, "invoice", &invoice.name, &sys)
        .await
        .unwrap()
        .unwrap();
    drop(client);
    let got = titles(&db, &reg, &user("rita", &["Reader"]), &[stored]).await;
    assert_eq!(
        got["client"],
        BTreeMap::from([("C-1".to_owned(), "Ada".to_owned())])
    );
}

#[tokio::test]
async fn a_page_of_fifty_documents_is_titled_correctly() {
    let (db, reg) = setup().await;
    let sys = Caller::system("sys");
    for i in 0..10 {
        make(
            &db,
            &reg,
            "client",
            &[
                ("code", text(&format!("C-{i}"))),
                ("full_name", text(&format!("Client {i}"))),
            ],
            &[],
            &sys,
        )
        .await;
    }
    for i in 0..50 {
        make(
            &db,
            &reg,
            "invoice",
            &[("client", text(&format!("C-{}", i % 10)))],
            &[],
            &sys,
        )
        .await;
    }
    let reader = user("rita", &["Reader"]);
    let client = db.pool.get().await.unwrap();
    let page = list(
        &**client,
        &reg,
        "invoice",
        &reader,
        &ListQuery {
            limit: Some(50),
            ..ListQuery::default()
        },
    )
    .await
    .unwrap();
    assert_eq!(page.len(), 50);
    let got = link_titles(&**client, &reg, &reader, "invoice", &page)
        .await
        .unwrap();
    assert_eq!(got["client"].len(), 10);
    for i in 0..10 {
        assert_eq!(got["client"][&format!("C-{i}")], format!("Client {i}"));
    }
    // The titles agree with the documents on the page.
    for document in &page {
        let Value::Text(name) = &document.values["client"] else {
            panic!("a link value");
        };
        assert!(got["client"].contains_key(name));
    }
}

#[tokio::test]
async fn an_unknown_or_child_doctype_is_an_error() {
    let (db, reg) = setup().await;
    let client = db.pool.get().await.unwrap();
    for name in ["nope", "invoice_line"] {
        let result = link_titles(&**client, &reg, &admin(), name, &[]).await;
        assert!(
            matches!(
                result,
                Err(DbError::UnknownDocType { .. } | DbError::ChildDocType { .. })
            ),
            "{name}: {result:?}"
        );
    }
}
