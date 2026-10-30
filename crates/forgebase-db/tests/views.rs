//! Saved list views (M8.3) against a real PostgreSQL (see `common`): privacy, the Administrator
//! rule for shared views, validation of the definition, labels, limits, versions and the cascade
//! with the user.

mod common;

use common::{TestDb, registry};
use forgebase_core::meta::Registry;
use forgebase_core::permission::{ADMINISTRATOR, Caller};
use forgebase_core::value::Value;
use forgebase_db::{
    DbError, Filter, FilterOp, ListQuery, MAX_DEFINITION_BYTES, MAX_PRIVATE_VIEWS,
    MAX_SHARED_VIEWS, Sort, View, ViewChanges, ViewDefinition, create_view, delete_view,
    list_views, update_view,
};
use serde_json::json;
use uuid::Uuid;

const ORDER: &str = r#"
name = "order"
label = "Order"
submittable = true
[[fields]]
name = "title"
type = "data"
[[fields]]
name = "status"
type = "select"
options = ["draft", "open"]
[[fields]]
name = "note"
type = "text"
[[fields]]
name = "qty"
type = "int"
[[fields]]
name = "lines"
type = "table"
target = "order_line"

[[permissions]]
role = "Clerk"
read = true
write = true
create = true
[[permissions]]
role = "Own"
read = true
if_owner = true
"#;

const LINE: &str = r#"
name = "order_line"
label = "Order Line"
is_child = true
[[fields]]
name = "qty"
type = "int"
"#;

const OTHER: &str = r#"
name = "other"
label = "Other"
[[fields]]
name = "title"
type = "data"
[[permissions]]
role = "Clerk"
read = true
"#;

async fn setup() -> (TestDb, Registry) {
    let db = TestDb::migrated().await;
    for name in ["ann", "bob", "root"] {
        db.exec(&format!(
            "INSERT INTO fb_user (id, username, password_hash) VALUES (gen_random_uuid(), '{name}', 'x')"
        ))
        .await;
    }
    (db, registry(&[ORDER, LINE, OTHER]))
}

fn user(name: &str, roles: &[&str]) -> Caller {
    Caller::user(name, roles.iter().map(|r| (*r).to_owned()).collect())
}

fn ann() -> Caller {
    user("ann", &["Clerk"])
}

fn bob() -> Caller {
    user("bob", &["Clerk"])
}

fn root() -> Caller {
    user("root", &[ADMINISTRATOR])
}

/// A definition with one filter (`status = draft`), one sort and the given columns.
fn def(columns: &[&str]) -> ViewDefinition {
    ViewDefinition {
        json: json!({
            "filters": [{"field": "status", "op": "=", "value": "draft"}],
            "sort": ["title:desc"],
            "columns": columns,
        }),
        query: ListQuery {
            filters: vec![Filter::new(
                "status",
                FilterOp::Eq(Value::Text("draft".into())),
            )],
            sort: vec![Sort::desc("title")],
            ..ListQuery::default()
        },
    }
}

fn empty() -> ViewDefinition {
    ViewDefinition {
        json: json!({"filters": [], "sort": [], "columns": []}),
        query: ListQuery::default(),
    }
}

async fn create(
    db: &TestDb,
    reg: &Registry,
    caller: &Caller,
    label: &str,
    shared: bool,
) -> Result<View, DbError> {
    let mut client = db.pool.get().await.unwrap();
    create_view(
        &mut **client,
        reg,
        "order",
        caller,
        label,
        &def(&[]),
        shared,
    )
    .await
}

async fn labels(db: &TestDb, reg: &Registry, caller: &Caller) -> Vec<String> {
    let client = db.pool.get().await.unwrap();
    list_views(&**client, reg, "order", caller)
        .await
        .unwrap()
        .into_iter()
        .map(|v| v.label)
        .collect()
}

async fn update(
    db: &TestDb,
    reg: &Registry,
    caller: &Caller,
    id: Uuid,
    version: i64,
    changes: &ViewChanges<'_>,
) -> Result<View, DbError> {
    let client = db.pool.get().await.unwrap();
    update_view(&**client, reg, "order", id, version, caller, changes).await
}

async fn delete(
    db: &TestDb,
    reg: &Registry,
    caller: &Caller,
    id: Uuid,
    version: i64,
) -> Result<(), DbError> {
    let client = db.pool.get().await.unwrap();
    delete_view(&**client, reg, "order", id, version, caller).await
}

fn invalid(result: Result<View, DbError>) -> String {
    match result {
        Err(DbError::InvalidView(message)) => message,
        other => panic!("expected InvalidView, got {other:?}"),
    }
}

#[tokio::test]
async fn a_private_view_can_be_created_listed_updated_and_deleted() {
    let (db, reg) = setup().await;
    let view = create(&db, &reg, &ann(), "Open orders", false)
        .await
        .unwrap();
    assert_eq!(view.label, "Open orders");
    assert_eq!(view.owner.as_deref(), Some("ann"));
    assert!(!view.shared());
    assert_eq!(view.version, 1);
    assert_eq!(view.doctype, "order");
    assert_eq!(view.definition, def(&[]).json);

    let client = db.pool.get().await.unwrap();
    let listed = list_views(&**client, &reg, "order", &ann()).await.unwrap();
    assert_eq!(listed, vec![view.clone()]);
    drop(client);

    let new = def(&["title", "qty"]);
    let changed = update(
        &db,
        &reg,
        &ann(),
        view.id,
        1,
        &ViewChanges {
            label: Some("Drafts"),
            definition: Some(&new),
        },
    )
    .await
    .unwrap();
    assert_eq!((changed.label.as_str(), changed.version), ("Drafts", 2));
    assert_eq!(changed.definition, new.json);
    assert!(changed.modified_at >= view.modified_at);

    // Only the label changes: the definition is kept.
    let renamed = update(
        &db,
        &reg,
        &ann(),
        view.id,
        2,
        &ViewChanges {
            label: Some("Drafts 2"),
            definition: None,
        },
    )
    .await
    .unwrap();
    assert_eq!(renamed.definition, new.json);

    delete(&db, &reg, &ann(), view.id, 3).await.unwrap();
    assert!(labels(&db, &reg, &ann()).await.is_empty());
    assert!(matches!(
        delete(&db, &reg, &ann(), view.id, 3).await,
        Err(DbError::NotFound { .. })
    ));
}

#[tokio::test]
async fn another_users_private_view_is_invisible_and_cannot_be_changed() {
    let (db, reg) = setup().await;
    let view = create(&db, &reg, &ann(), "Mine", false).await.unwrap();

    // Not listed for bob, nor for an Administrator.
    assert!(labels(&db, &reg, &bob()).await.is_empty());
    assert!(labels(&db, &reg, &root()).await.is_empty());
    assert!(labels(&db, &reg, &Caller::system("cli")).await.is_empty());

    // NotFound, not a refusal, and before the version is compared.
    let rename = ViewChanges {
        label: Some("Stolen"),
        definition: None,
    };
    for caller in [bob(), root(), Caller::system("cli")] {
        for version in [1, 99] {
            assert!(
                matches!(
                    update(&db, &reg, &caller, view.id, version, &rename).await,
                    Err(DbError::NotFound { .. })
                ),
                "{caller:?} {version}"
            );
            assert!(
                matches!(
                    delete(&db, &reg, &caller, view.id, version).await,
                    Err(DbError::NotFound { .. })
                ),
                "{caller:?} {version}"
            );
        }
    }
    // Unchanged.
    let kept = labels(&db, &reg, &ann()).await;
    assert_eq!(kept, vec!["Mine".to_owned()]);

    // The same label is free for another user.
    create(&db, &reg, &bob(), "Mine", false).await.unwrap();
}

#[tokio::test]
async fn a_view_of_another_doctype_is_not_found() {
    let (db, reg) = setup().await;
    let view = create(&db, &reg, &ann(), "Mine", false).await.unwrap();
    let client = db.pool.get().await.unwrap();
    let result = delete_view(&**client, &reg, "other", view.id, 1, &ann()).await;
    assert!(
        matches!(result, Err(DbError::NotFound { .. })),
        "{result:?}"
    );
}

#[tokio::test]
async fn only_an_administrator_creates_and_changes_shared_views() {
    let (db, reg) = setup().await;
    let denied = create(&db, &reg, &ann(), "Team", true).await;
    assert!(matches!(denied, Err(DbError::ViewDenied(_))), "{denied:?}");
    // The denial does not depend on the definition.
    let mut client = db.pool.get().await.unwrap();
    let denied = create_view(&mut **client, &reg, "order", &ann(), "", &empty(), true).await;
    assert!(matches!(denied, Err(DbError::ViewDenied(_))), "{denied:?}");
    drop(client);

    let shared = create(&db, &reg, &root(), "Team", true).await.unwrap();
    assert!(shared.shared());
    assert_eq!(shared.owner, None);

    // Everyone with read sees it, next to their own; shared views come first.
    create(&db, &reg, &ann(), "A mine", false).await.unwrap();
    assert_eq!(labels(&db, &reg, &ann()).await, ["Team", "A mine"]);
    assert_eq!(labels(&db, &reg, &bob()).await, ["Team"]);
    assert_eq!(labels(&db, &reg, &user("zed", &["Own"])).await, ["Team"]);

    // A non-administrator can see it and so is told no, not "not found".
    let rename = ViewChanges {
        label: Some("Mine now"),
        definition: None,
    };
    assert!(matches!(
        update(&db, &reg, &ann(), shared.id, 1, &rename).await,
        Err(DbError::ViewDenied(_))
    ));
    assert!(matches!(
        delete(&db, &reg, &ann(), shared.id, 1).await,
        Err(DbError::ViewDenied(_))
    ));

    // An Administrator and the system context may change and delete it.
    let changed = update(&db, &reg, &root(), shared.id, 1, &rename)
        .await
        .unwrap();
    assert_eq!(changed.version, 2);
    let cli = Caller::system("cli");
    let by_system = create(&db, &reg, &cli, "From the CLI", true).await.unwrap();
    delete(&db, &reg, &cli, by_system.id, 1).await.unwrap();
    delete(&db, &reg, &root(), shared.id, 2).await.unwrap();
    assert!(labels(&db, &reg, &bob()).await.is_empty());
}

#[tokio::test]
async fn the_system_context_cannot_own_a_private_view() {
    let (db, reg) = setup().await;
    let result = create(&db, &reg, &Caller::system("cli"), "x", false).await;
    assert!(matches!(result, Err(DbError::ViewDenied(_))), "{result:?}");
}

#[tokio::test]
async fn the_read_right_on_the_doctype_is_needed() {
    let (db, reg) = setup().await;
    let nobody = user("bob", &["Nobody"]);
    let client = db.pool.get().await.unwrap();
    assert!(matches!(
        list_views(&**client, &reg, "order", &nobody).await,
        Err(DbError::PermissionDenied { .. })
    ));
    drop(client);
    assert!(matches!(
        create(&db, &reg, &nobody, "x", false).await,
        Err(DbError::PermissionDenied { .. })
    ));
    // Update and delete are refused the same way, before any view is looked up.
    let view = create(&db, &reg, &bob(), "x", false).await.unwrap();
    assert!(matches!(
        delete(&db, &reg, &nobody, view.id, 1).await,
        Err(DbError::PermissionDenied { .. })
    ));
    // An unknown and a child DocType.
    let client = db.pool.get().await.unwrap();
    for name in ["nope", "order_line"] {
        let result = list_views(&**client, &reg, name, &ann()).await;
        assert!(
            matches!(
                result,
                Err(DbError::UnknownDocType { .. } | DbError::ChildDocType { .. })
            ),
            "{name}: {result:?}"
        );
    }
}

#[tokio::test]
async fn the_definition_is_validated_like_a_list_query() {
    let (db, reg) = setup().await;
    let make = |json: serde_json::Value, query: ListQuery| ViewDefinition { json, query };
    let create_with = |definition: ViewDefinition| {
        let (db, reg) = (&db, &reg);
        async move {
            let mut client = db.pool.get().await.unwrap();
            create_view(&mut **client, reg, "order", &ann(), "v", &definition, false).await
        }
    };
    let filter = |field: &str, op: FilterOp| ListQuery {
        filters: vec![Filter::new(field, op)],
        ..ListQuery::default()
    };
    let one = |field: &str| json!([{"field": field, "op": "is null"}]);

    // An unknown field, a table field and a wrong operator in a filter.
    for (field, op, needle) in [
        ("nope", FilterOp::IsNull(true), "unknown field"),
        ("lines", FilterOp::IsNull(true), "table field"),
        ("qty", FilterOp::Like("1%".into()), "text fields only"),
    ] {
        let message = invalid(
            create_with(make(
                json!({"filters": one(field), "sort": [], "columns": []}),
                filter(field, op),
            ))
            .await,
        );
        assert!(message.contains(needle), "{field}: {message}");
    }
    // A bad sort.
    let message = invalid(
        create_with(make(
            json!({"filters": [], "sort": ["nope"], "columns": []}),
            ListQuery {
                sort: vec![Sort::asc("nope")],
                ..ListQuery::default()
            },
        ))
        .await,
    );
    assert!(message.contains("unknown field"), "{message}");
    // The JSON and its parsed form must agree.
    let message = invalid(
        create_with(make(
            json!({"filters": one("qty"), "sort": [], "columns": []}),
            ListQuery::default(),
        ))
        .await,
    );
    assert!(message.contains("do not match"), "{message}");

    // Columns: unknown, Table, Text, duplicates, too many, standard columns that do not exist.
    for (columns, needle) in [
        (json!(["nope"]), "unknown column"),
        (json!(["lines"]), "table field"),
        (json!(["note"]), "text field"),
        (json!(["title", "title"]), "twice"),
        (json!(["workflow_state"]), "unknown column"),
        (json!(["version"]), "unknown column"),
        (json!([1]), "list of strings"),
        (json!("title"), "must be a list"),
    ] {
        let message = invalid(
            create_with(make(
                json!({"filters": [], "sort": [], "columns": columns}),
                ListQuery::default(),
            ))
            .await,
        );
        assert!(message.contains(needle), "{columns}: {message}");
    }
    let nine: Vec<&str> = vec![
        "name",
        "owner",
        "modified_by",
        "created_at",
        "modified_at",
        "docstatus",
        "title",
        "status",
        "qty",
    ];
    let message = invalid(
        create_with(make(
            json!({"filters": [], "sort": [], "columns": nine}),
            ListQuery::default(),
        ))
        .await,
    );
    assert!(message.contains("at most 8"), "{message}");
    // Eight of every kind that is allowed work.
    let eight = &nine[..8];
    assert!(
        create_with(make(
            json!({"filters": [], "sort": [], "columns": eight}),
            ListQuery::default(),
        ))
        .await
        .is_ok()
    );

    // Unknown keys and a missing key.
    for json in [
        json!({"filters": [], "sort": [], "columns": [], "limit": 5}),
        json!({"filters": [], "sort": []}),
        json!({"filters": {}, "sort": [], "columns": []}),
        json!([]),
    ] {
        let result = create_with(make(json.clone(), ListQuery::default())).await;
        assert!(
            matches!(result, Err(DbError::InvalidView(_))),
            "{json}: {result:?}"
        );
    }
}

#[tokio::test]
async fn the_definition_has_a_size_cap() {
    let (db, reg) = setup().await;
    let big = "x".repeat(MAX_DEFINITION_BYTES);
    let definition = ViewDefinition {
        json: json!({
            "filters": [{"field": "title", "op": "=", "value": big}],
            "sort": [],
            "columns": [],
        }),
        query: ListQuery {
            filters: vec![Filter::new("title", FilterOp::Eq(Value::Text(big)))],
            ..ListQuery::default()
        },
    };
    let mut client = db.pool.get().await.unwrap();
    let result = create_view(
        &mut **client,
        &reg,
        "order",
        &ann(),
        "big",
        &definition,
        false,
    )
    .await;
    assert!(invalid(result).contains("16 KiB"));
}

#[tokio::test]
async fn labels_are_checked_and_unique() {
    let (db, reg) = setup().await;
    for label in ["", " x", "x ", "a\nb", &"x".repeat(65)] {
        let result = create(&db, &reg, &ann(), label, false).await;
        assert!(matches!(result, Err(DbError::InvalidView(_))), "{label:?}");
    }
    create(&db, &reg, &ann(), &"é".repeat(64), false)
        .await
        .unwrap();

    // Private: unique per user and DocType; case matters.
    let first = create(&db, &reg, &ann(), "Same", false).await.unwrap();
    assert!(matches!(
        create(&db, &reg, &ann(), "Same", false).await,
        Err(DbError::ViewExists { .. })
    ));
    create(&db, &reg, &ann(), "same", false).await.unwrap();
    create(&db, &reg, &bob(), "Same", false).await.unwrap();
    // Another DocType has its own labels.
    let mut client = db.pool.get().await.unwrap();
    create_view(
        &mut **client,
        &reg,
        "other",
        &ann(),
        "Same",
        &empty(),
        false,
    )
    .await
    .unwrap();
    drop(client);

    // Shared: unique per DocType.
    create(&db, &reg, &root(), "Team", true).await.unwrap();
    assert!(matches!(
        create(&db, &reg, &root(), "Team", true).await,
        Err(DbError::ViewExists { .. })
    ));
    // Renaming into a taken label is refused too.
    let result = update(
        &db,
        &reg,
        &ann(),
        first.id,
        1,
        &ViewChanges {
            label: Some("same"),
            definition: None,
        },
    )
    .await;
    assert!(
        matches!(result, Err(DbError::ViewExists { .. })),
        "{result:?}"
    );
    let result = update(
        &db,
        &reg,
        &ann(),
        first.id,
        1,
        &ViewChanges {
            label: Some(" bad"),
            definition: None,
        },
    )
    .await;
    assert!(matches!(result, Err(DbError::InvalidView(_))));
}

#[tokio::test]
async fn there_are_limits_per_user_and_for_shared_views() {
    let (db, reg) = setup().await;
    // Fill by SQL: the limit is the point, not 100 round trips.
    db.exec(&format!(
        "INSERT INTO fb_saved_view (id, doctype, label, owner_id, definition) \
         SELECT gen_random_uuid(), 'order', 'v' || n, (SELECT id FROM fb_user WHERE username = 'ann'), \
         '{{\"filters\":[],\"sort\":[],\"columns\":[]}}' FROM generate_series(1, {MAX_PRIVATE_VIEWS}) n"
    ))
    .await;
    let message = invalid(create(&db, &reg, &ann(), "one too many", false).await);
    assert!(message.contains("at most 100 private views"), "{message}");
    // Per user and per DocType.
    create(&db, &reg, &bob(), "fine", false).await.unwrap();
    let mut client = db.pool.get().await.unwrap();
    create_view(
        &mut **client,
        &reg,
        "other",
        &ann(),
        "fine",
        &empty(),
        false,
    )
    .await
    .unwrap();
    drop(client);
    // Deleting one frees a slot.
    let first = {
        let client = db.pool.get().await.unwrap();
        list_views(&**client, &reg, "order", &ann()).await.unwrap()[0].clone()
    };
    delete(&db, &reg, &ann(), first.id, 1).await.unwrap();
    create(&db, &reg, &ann(), "one more", false).await.unwrap();

    db.exec(&format!(
        "INSERT INTO fb_saved_view (id, doctype, label, owner_id, definition) \
         SELECT gen_random_uuid(), 'order', 's' || n, NULL, \
         '{{\"filters\":[],\"sort\":[],\"columns\":[]}}' FROM generate_series(1, {MAX_SHARED_VIEWS}) n"
    ))
    .await;
    let message = invalid(create(&db, &reg, &root(), "one too many", true).await);
    assert!(message.contains("at most 100 shared views"), "{message}");
}

#[tokio::test]
async fn concurrent_creates_cannot_pass_the_limit() {
    let (db, reg) = setup().await;
    db.exec(&format!(
        "INSERT INTO fb_saved_view (id, doctype, label, owner_id, definition) \
         SELECT gen_random_uuid(), 'order', 'v' || n, (SELECT id FROM fb_user WHERE username = 'ann'), \
         '{{\"filters\":[],\"sort\":[],\"columns\":[]}}' FROM generate_series(1, {}) n",
        MAX_PRIVATE_VIEWS - 1
    ))
    .await;
    let attempt = |i: u32| {
        let (db, reg) = (&db, &reg);
        async move { create(db, reg, &ann(), &format!("race {i}"), false).await }
    };
    let (r0, r1, r2, r3) = tokio::join!(attempt(0), attempt(1), attempt(2), attempt(3));
    let results = [r0, r1, r2, r3];
    assert_eq!(
        results.iter().filter(|r| r.is_ok()).count(),
        1,
        "{results:?}"
    );
    assert_eq!(
        db.rows("SELECT count(*)::text FROM fb_saved_view").await,
        vec![vec!["100".to_owned()]]
    );
}

#[tokio::test]
async fn versions_are_optimistic() {
    let (db, reg) = setup().await;
    let view = create(&db, &reg, &ann(), "v", false).await.unwrap();
    let rename = ViewChanges {
        label: Some("w"),
        definition: None,
    };
    update(&db, &reg, &ann(), view.id, 1, &rename)
        .await
        .unwrap();
    let stale = update(&db, &reg, &ann(), view.id, 1, &rename).await;
    match stale {
        Err(DbError::Conflict {
            expected, current, ..
        }) => assert_eq!((expected, current), (1, 2)),
        other => panic!("{other:?}"),
    }
    assert!(matches!(
        delete(&db, &reg, &ann(), view.id, 1).await,
        Err(DbError::Conflict { .. })
    ));
    delete(&db, &reg, &ann(), view.id, 2).await.unwrap();
    // No change at all.
    let view = create(&db, &reg, &ann(), "v", false).await.unwrap();
    assert!(matches!(
        update(&db, &reg, &ann(), view.id, 1, &ViewChanges::default()).await,
        Err(DbError::NoChanges)
    ));
}

#[tokio::test]
async fn a_stale_definition_is_returned_as_stored() {
    let (db, reg) = setup().await;
    // A view that names a field the DocType no longer has: written by hand.
    db.exec(
        "INSERT INTO fb_saved_view (id, doctype, label, owner_id, definition) \
         VALUES (gen_random_uuid(), 'order', 'Old', NULL, \
         '{\"filters\":[{\"field\":\"gone\",\"op\":\"is null\"}],\"sort\":[],\"columns\":[\"gone\"]}')",
    )
    .await;
    let client = db.pool.get().await.unwrap();
    let views = list_views(&**client, &reg, "order", &bob()).await.unwrap();
    assert_eq!(views.len(), 1);
    assert_eq!(views[0].definition["columns"], json!(["gone"]));
}

#[tokio::test]
async fn labels_sort_by_bytes_within_each_group() {
    let (db, reg) = setup().await;
    for label in ["b", "B", "a", "_x", "Z"] {
        create(&db, &reg, &ann(), label, false).await.unwrap();
    }
    create(&db, &reg, &root(), "zz shared", true).await.unwrap();
    assert_eq!(
        labels(&db, &reg, &ann()).await,
        ["zz shared", "B", "Z", "_x", "a", "b"]
    );
}

#[tokio::test]
async fn deleting_a_user_deletes_their_private_views_only() {
    let (db, reg) = setup().await;
    create(&db, &reg, &ann(), "mine", false).await.unwrap();
    create(&db, &reg, &bob(), "his", false).await.unwrap();
    create(&db, &reg, &root(), "shared", true).await.unwrap();
    db.exec("DELETE FROM fb_user WHERE username = 'ann'").await;
    assert_eq!(
        db.rows("SELECT label FROM fb_saved_view ORDER BY label")
            .await,
        vec![vec!["his".to_owned()], vec!["shared".to_owned()]]
    );
}
