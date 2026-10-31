//! Permission enforcement (M3.2, ADR 0011) against a real PostgreSQL (see `common`): the
//! authorization matrix, `if_owner`, deny by default, the union of roles, hidden documents that
//! look missing (also for stale versions and links), and what errors may say.

mod common;

use std::collections::BTreeMap;
use std::sync::{Arc, Mutex};

use common::{TestDb, registry};
use forgebase_core::document::{Changes, Document, Row, ValidationError};
use forgebase_core::hooks::{DocTypeHooks, HookContext, HookError};
use forgebase_core::meta::Registry;
use forgebase_core::permission::{ADMINISTRATOR, Caller, Right};
use forgebase_core::value::Value;
use forgebase_db::{DbError, StoredDocument, amend, cancel, delete, get, insert, submit, update};

/// Submittable, one role per right (each also reads), and `Own`, which has every right on own
/// documents only. A Link to `party` in the document and in its rows.
const TICKET: &str = r#"
name = "ticket"
label = "Ticket"
submittable = true
[[fields]]
name = "title"
type = "data"
required = true
[[fields]]
name = "customer"
type = "link"
target = "party"
[[fields]]
name = "lines"
type = "table"
target = "ticket_line"

[[permissions]]
role = "Reader"
read = true
[[permissions]]
role = "Writer"
read = true
write = true
[[permissions]]
role = "Creator"
read = true
create = true
[[permissions]]
role = "Deleter"
read = true
delete = true
[[permissions]]
role = "Submitter"
read = true
submit = true
[[permissions]]
role = "Canceller"
read = true
cancel = true
[[permissions]]
role = "Amender"
read = true
amend = true
[[permissions]]
role = "WriteOnly"
write = true
[[permissions]]
role = "Own"
read = true
write = true
create = true
delete = true
submit = true
cancel = true
amend = true
if_owner = true
"#;

const TICKET_LINE: &str = r#"
name = "ticket_line"
label = "Ticket Line"
is_child = true
[[fields]]
name = "qty"
type = "int"
[[fields]]
name = "contact"
type = "link"
target = "party"
"#;

/// A Link target: `Reader` reads all, `OwnReader` reads own, `PartyAdmin` reads and deletes all.
const PARTY: &str = r#"
name = "party"
label = "Party"
naming = { field = "email" }
[[fields]]
name = "email"
type = "data"
required = true
unique = true

[[permissions]]
role = "Reader"
read = true
[[permissions]]
role = "OwnReader"
read = true
if_owner = true
[[permissions]]
role = "PartyAdmin"
read = true
delete = true
[[permissions]]
role = "PartyCreator"
create = true
"#;

/// Readable by the whole team, but only the owner may change or delete.
const MEMO: &str = r#"
name = "memo"
label = "Memo"
[[fields]]
name = "body"
type = "text"

[[permissions]]
role = "Team"
read = true
create = true
[[permissions]]
role = "Team"
write = true
delete = true
if_owner = true
"#;

/// No rules at all: deny by default.
const SECRET: &str = r#"
name = "secret"
label = "Secret"
[[fields]]
name = "body"
type = "text"
"#;

async fn setup_with(reg: Registry) -> (TestDb, Registry) {
    let db = TestDb::migrated().await;
    db.sync(&reg).await.expect("sync");
    (db, reg)
}

async fn setup() -> (TestDb, Registry) {
    setup_with(registry(&[TICKET, TICKET_LINE, PARTY, MEMO, SECRET])).await
}

fn user(name: &str, roles: &[&str]) -> Caller {
    Caller::user(name, roles.iter().map(|r| (*r).to_owned()).collect())
}

fn admin() -> Caller {
    user("root", &[ADMINISTRATOR])
}

fn sys() -> Caller {
    Caller::system("sys")
}

fn text(s: &str) -> Value {
    Value::Text(s.to_owned())
}

fn doc(pairs: &[(&str, Value)]) -> Document {
    Document {
        name: None,
        values: pairs
            .iter()
            .map(|(k, v)| ((*k).to_owned(), v.clone()))
            .collect(),
        tables: BTreeMap::new(),
    }
}

fn change(field: &str, value: Value) -> Changes {
    Changes {
        values: BTreeMap::from([(field.to_owned(), value)]),
        tables: BTreeMap::new(),
    }
}

fn row(contact: Option<&str>) -> Row {
    BTreeMap::from([
        ("qty".to_owned(), Value::Int(1)),
        ("contact".to_owned(), contact.map_or(Value::Null, text)),
    ])
}

async fn make_ticket(db: &TestDb, reg: &Registry, owner: &Caller) -> StoredDocument {
    let mut client = db.pool.get().await.unwrap();
    insert(
        &mut **client,
        reg,
        "ticket",
        doc(&[("title", text("t"))]),
        owner,
    )
    .await
    .unwrap()
}

async fn make_party(db: &TestDb, reg: &Registry, email: &str, owner: &Caller) -> StoredDocument {
    let mut client = db.pool.get().await.unwrap();
    insert(
        &mut **client,
        reg,
        "party",
        doc(&[("email", text(email))]),
        owner,
    )
    .await
    .unwrap()
}

/// A ticket made by the system and moved to the state `right` needs, then `right` attempted by
/// `caller`. The result of the attempt only.
async fn attempt(
    db: &TestDb,
    reg: &Registry,
    right: Right,
    caller: &Caller,
) -> Result<(), DbError> {
    let mut client = db.pool.get().await.unwrap();
    let client = &mut **client;
    if right == Right::Create {
        return insert(client, reg, "ticket", doc(&[("title", text("n"))]), caller)
            .await
            .map(drop);
    }
    let made = make_ticket(db, reg, &sys()).await;
    let (name, mut version) = (made.name, made.version);
    // Move the document to the status the right works on.
    if matches!(right, Right::Cancel | Right::Amend) {
        version = submit(client, reg, "ticket", &name, version, &sys())
            .await
            .unwrap()
            .version;
    }
    if right == Right::Amend {
        version = cancel(client, reg, "ticket", &name, version, &sys())
            .await
            .unwrap()
            .version;
    }
    match right {
        Right::Read => get(client, reg, "ticket", &name, caller)
            .await?
            .map(drop)
            .ok_or_else(|| DbError::NotFound {
                doctype: "ticket".into(),
                name: name.clone(),
            }),
        Right::Write => update(
            client,
            reg,
            "ticket",
            &name,
            version,
            &change("title", text("x")),
            caller,
        )
        .await
        .map(drop),
        Right::Delete => delete(client, reg, "ticket", &name, version, caller).await,
        Right::Submit => submit(client, reg, "ticket", &name, version, caller)
            .await
            .map(drop),
        Right::Cancel => cancel(client, reg, "ticket", &name, version, caller)
            .await
            .map(drop),
        Right::Amend => amend(client, reg, "ticket", &name, version, caller)
            .await
            .map(drop),
        Right::Create => unreachable!("handled above"),
        // Rights of a file, not of one document (the matrix of the combinations is in
        // `tests/import_export.rs`): here only that the evaluator's answer reaches the caller.
        Right::Export => {
            let tx = forgebase_db::begin_export(client).await?;
            let query = forgebase_db::ExportQuery {
                doctype: "ticket",
                filters: &[],
                sort: &[],
                limit: 1_000,
                with_tables: false,
            };
            forgebase_db::Export::start(&tx, reg, caller, &query)
                .await
                .map(drop)
        }
        Right::Import => {
            let row = forgebase_db::ImportRow {
                row: 1,
                values: [("title".to_owned(), text("n"))].into(),
                ..forgebase_db::ImportRow::default()
            };
            forgebase_db::import(
                client,
                reg,
                "ticket",
                caller,
                &[row],
                forgebase_db::ImportMode::Insert,
                false,
            )
            .await
            .map(drop)
        }
    }
}

const ROLE_OF: [(Right, &str); 7] = [
    (Right::Read, "Reader"),
    (Right::Write, "Writer"),
    (Right::Create, "Creator"),
    (Right::Delete, "Deleter"),
    (Right::Submit, "Submitter"),
    (Right::Cancel, "Canceller"),
    (Right::Amend, "Amender"),
];

fn is_denied(result: &Result<(), DbError>, right: Right) -> bool {
    matches!(result, Err(DbError::PermissionDenied { doctype, right: r }) if doctype == "ticket" && *r == right)
}

#[tokio::test]
async fn every_right_is_allowed_with_its_role_and_denied_without() {
    let (db, reg) = setup().await;
    for (right, role) in ROLE_OF {
        let allowed = attempt(&db, &reg, right, &user("u", &[role])).await;
        assert!(allowed.is_ok(), "{right} with {role}: {allowed:?}");
        let none = attempt(&db, &reg, right, &user("u", &[])).await;
        assert!(is_denied(&none, right), "{right} without a role: {none:?}");
        let unrelated = attempt(&db, &reg, right, &user("u", &["Nobody", "reader"])).await;
        assert!(
            is_denied(&unrelated, right),
            "{right} with an unrelated role: {unrelated:?}"
        );
        // Every other role (each of which also reads) is denied this right, except for read.
        for (other, other_role) in ROLE_OF {
            if other == right {
                continue;
            }
            let result = attempt(&db, &reg, right, &user("u", &[other_role])).await;
            if right == Right::Read {
                assert!(result.is_ok(), "read with {other_role}: {result:?}");
            } else {
                assert!(
                    is_denied(&result, right),
                    "{right} with {other_role}: {result:?}"
                );
            }
        }
    }
    // Write without read: the document cannot even be looked at.
    let result = attempt(&db, &reg, Right::Write, &user("u", &["WriteOnly"])).await;
    assert!(is_denied(&result, Right::Write), "{result:?}");
    let result = attempt(&db, &reg, Right::Read, &user("u", &["WriteOnly"])).await;
    assert!(is_denied(&result, Right::Read), "{result:?}");
}

#[tokio::test]
async fn administrator_and_the_system_context_bypass_every_rule() {
    let (db, reg) = setup().await;
    for right in Right::ALL {
        for caller in [admin(), sys(), user("u", &["Nobody", ADMINISTRATOR])] {
            let result = attempt(&db, &reg, right, &caller).await;
            assert!(result.is_ok(), "{right} as {caller:?}: {result:?}");
        }
    }
    // A role that merely looks like the built-in one is no administrator.
    let result = attempt(&db, &reg, Right::Read, &user("u", &["administrator"])).await;
    assert!(is_denied(&result, Right::Read), "{result:?}");
}

#[tokio::test]
async fn a_doctype_without_rules_is_denied_to_users_and_open_to_administrator_and_system() {
    let (db, reg) = setup().await;
    let mut client = db.pool.get().await.unwrap();
    let client = &mut **client;
    let made = insert(
        client,
        &reg,
        "secret",
        doc(&[("body", text("s"))]),
        &admin(),
    )
    .await
    .unwrap();
    let nobody = user("u", &["Reader", "Writer", "Own", "Team", "Anything"]);
    let denied = |e: DbError, right| {
        assert!(
            matches!(&e, DbError::PermissionDenied { doctype, right: r } if doctype == "secret" && *r == right),
            "{right}: {e:?}"
        );
    };
    denied(
        insert(client, &reg, "secret", doc(&[]), &nobody)
            .await
            .unwrap_err(),
        Right::Create,
    );
    denied(
        get(client, &reg, "secret", &made.name, &nobody)
            .await
            .unwrap_err(),
        Right::Read,
    );
    denied(
        forgebase_db::list(&*client, &reg, "secret", &nobody, &Default::default())
            .await
            .unwrap_err(),
        Right::Read,
    );
    denied(
        update(
            client,
            &reg,
            "secret",
            &made.name,
            1,
            &change("body", text("x")),
            &nobody,
        )
        .await
        .unwrap_err(),
        Right::Write,
    );
    denied(
        delete(client, &reg, "secret", &made.name, 1, &nobody)
            .await
            .unwrap_err(),
        Right::Delete,
    );
    assert!(
        get(client, &reg, "secret", &made.name, &sys())
            .await
            .unwrap()
            .is_some()
    );
    assert_eq!(
        forgebase_db::list(&*client, &reg, "secret", &admin(), &Default::default())
            .await
            .unwrap()
            .len(),
        1
    );
}

#[tokio::test]
async fn roles_are_a_union() {
    let (db, reg) = setup().await;
    let both = user("u", &["Reader", "Writer"]);
    assert!(attempt(&db, &reg, Right::Read, &both).await.is_ok());
    assert!(attempt(&db, &reg, Right::Write, &both).await.is_ok());
    assert!(is_denied(
        &attempt(&db, &reg, Right::Delete, &both).await,
        Right::Delete
    ));
    // `Own` alone covers only own documents, `Reader` adds reading everything.
    let mut client = db.pool.get().await.unwrap();
    let client = &mut **client;
    let others = make_ticket(&db, &reg, &user("zoe", &["Own"])).await;
    let ann_own = user("ann", &["Own"]);
    assert!(
        get(client, &reg, "ticket", &others.name, &ann_own)
            .await
            .unwrap()
            .is_none()
    );
    let ann_both = user("ann", &["Own", "Reader"]);
    assert!(
        get(client, &reg, "ticket", &others.name, &ann_both)
            .await
            .unwrap()
            .is_some()
    );
    // ... but writing stays limited to her own documents.
    let error = update(
        client,
        &reg,
        "ticket",
        &others.name,
        others.version,
        &change("title", text("x")),
        &ann_both,
    )
    .await
    .unwrap_err();
    assert!(
        matches!(
            error,
            DbError::PermissionDenied {
                right: Right::Write,
                ..
            }
        ),
        "{error:?}"
    );
}

/// Every operation as `caller` on `name` with a stale version `99`, all of which must fail the
/// same way for a document the caller cannot read.
async fn hidden_errors(db: &TestDb, reg: &Registry, name: &str, caller: &Caller) -> Vec<String> {
    let mut client = db.pool.get().await.unwrap();
    let client = &mut **client;
    let mut errors = Vec::new();
    let mut push = |e: DbError| errors.push(e.to_string());
    push(
        update(
            client,
            reg,
            "ticket",
            name,
            99,
            &change("title", text("x")),
            caller,
        )
        .await
        .unwrap_err(),
    );
    push(
        delete(client, reg, "ticket", name, 99, caller)
            .await
            .unwrap_err(),
    );
    push(
        submit(client, reg, "ticket", name, 99, caller)
            .await
            .unwrap_err(),
    );
    push(
        cancel(client, reg, "ticket", name, 99, caller)
            .await
            .unwrap_err(),
    );
    push(
        amend(client, reg, "ticket", name, 99, caller)
            .await
            .unwrap_err(),
    );
    errors
}

#[tokio::test]
async fn a_document_of_somebody_else_is_invisible_under_if_owner() {
    let (db, reg) = setup().await;
    let ann = user("ann", &["Own"]);
    let bob = user("bob", &["Own"]);
    let anns = make_ticket(&db, &reg, &ann).await;
    let bobs = make_ticket(&db, &reg, &bob).await;
    let mut client = db.pool.get().await.unwrap();
    let client = &mut **client;

    // get: own is there, the other's is None, exactly like a name that does not exist.
    assert!(
        get(client, &reg, "ticket", &anns.name, &ann)
            .await
            .unwrap()
            .is_some()
    );
    assert!(
        get(client, &reg, "ticket", &bobs.name, &ann)
            .await
            .unwrap()
            .is_none()
    );
    assert!(
        get(client, &reg, "ticket", "no-such", &ann)
            .await
            .unwrap()
            .is_none()
    );

    // Every mutation: NotFound, and the same message as for a missing name, whatever the version
    // (a stale version must not turn into Conflict, a right the caller has must not turn into
    // PermissionDenied).
    let hidden = hidden_errors(&db, &reg, &bobs.name, &ann).await;
    let missing = hidden_errors(&db, &reg, "no-such", &ann).await;
    for (h, m) in hidden.iter().zip(&missing) {
        assert!(h.contains("not found"), "{h}");
        assert_eq!(h.replace(&bobs.name, "X"), m.replace("no-such", "X"));
    }
    // The right version does not help either.
    let error = update(
        client,
        &reg,
        "ticket",
        &bobs.name,
        bobs.version,
        &change("title", text("x")),
        &ann,
    )
    .await
    .unwrap_err();
    assert!(matches!(error, DbError::NotFound { .. }), "{error:?}");

    // Nothing of bob's changed, and ann's own document works, including a stale version, which
    // is a Conflict only for a document she can see.
    let after = get(client, &reg, "ticket", &bobs.name, &bob)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(
        (after.version, after.values["title"].clone()),
        (1, text("t"))
    );
    let error = update(
        client,
        &reg,
        "ticket",
        &anns.name,
        99,
        &change("title", text("x")),
        &ann,
    )
    .await
    .unwrap_err();
    assert!(
        matches!(error, DbError::Conflict { current: 1, .. }),
        "{error:?}"
    );
    let updated = update(
        client,
        &reg,
        "ticket",
        &anns.name,
        1,
        &change("title", text("mine")),
        &ann,
    )
    .await
    .unwrap();
    assert_eq!(updated.modified_by, "ann");
    assert_eq!(updated.owner, "ann");
}

#[tokio::test]
async fn a_readable_document_of_somebody_else_is_permission_denied_for_an_if_owner_write() {
    let (db, reg) = setup().await;
    let mut client = db.pool.get().await.unwrap();
    let client = &mut **client;
    let (ann, bob) = (user("ann", &["Team"]), user("bob", &["Team"]));
    let memo = |body: &str| doc(&[("body", text(body))]);
    let anns = insert(client, &reg, "memo", memo("a"), &ann).await.unwrap();
    // Bob reads it ...
    assert!(
        get(client, &reg, "memo", &anns.name, &bob)
            .await
            .unwrap()
            .is_some()
    );
    // ... but cannot change or delete it, with the right or a stale version alike.
    for version in [1, 99] {
        let error = update(
            client,
            &reg,
            "memo",
            &anns.name,
            version,
            &change("body", text("x")),
            &bob,
        )
        .await
        .unwrap_err();
        assert!(
            matches!(&error, DbError::PermissionDenied { doctype, right: Right::Write } if doctype == "memo"),
            "{error:?}"
        );
        let error = delete(client, &reg, "memo", &anns.name, version, &bob)
            .await
            .unwrap_err();
        assert!(
            matches!(
                error,
                DbError::PermissionDenied {
                    right: Right::Delete,
                    ..
                }
            ),
            "{error:?}"
        );
    }
    // The owner can.
    update(
        client,
        &reg,
        "memo",
        &anns.name,
        1,
        &change("body", text("y")),
        &ann,
    )
    .await
    .unwrap();
    delete(client, &reg, "memo", &anns.name, 2, &ann)
        .await
        .unwrap();
}

#[tokio::test]
async fn amend_needs_a_readable_original_and_the_new_draft_belongs_to_the_caller() {
    let (db, reg) = setup().await;
    let mut client = db.pool.get().await.unwrap();
    let client = &mut **client;
    let (ann, bob) = (user("ann", &["Own"]), user("bob", &["Own"]));
    let made = make_ticket(&db, &reg, &ann).await;
    let v = submit(client, &reg, "ticket", &made.name, 1, &ann)
        .await
        .unwrap()
        .version;
    let v = cancel(client, &reg, "ticket", &made.name, v, &ann)
        .await
        .unwrap()
        .version;
    let error = amend(client, &reg, "ticket", &made.name, v, &bob)
        .await
        .unwrap_err();
    assert!(matches!(error, DbError::NotFound { .. }), "{error:?}");
    // An administrator can amend it; the draft is the administrator's.
    let amended = amend(client, &reg, "ticket", &made.name, v, &admin())
        .await
        .unwrap();
    assert_eq!(
        (amended.owner.as_str(), amended.modified_by.as_str()),
        ("root", "root")
    );
    // Ann amends her own cancelled document (after the amendment is deleted).
    delete(client, &reg, "ticket", &amended.name, 1, &admin())
        .await
        .unwrap();
    let again = amend(client, &reg, "ticket", &made.name, v, &ann)
        .await
        .unwrap();
    assert_eq!(again.owner, "ann");
}

#[tokio::test]
async fn child_rows_follow_the_parent_and_need_no_rules_of_their_own() {
    let (db, reg) = setup().await;
    let mut client = db.pool.get().await.unwrap();
    let client = &mut **client;
    let ann = user("ann", &["Own"]);
    let mut with_rows = doc(&[("title", text("t"))]);
    with_rows
        .tables
        .insert("lines".into(), vec![row(None), row(None)]);
    let made = insert(client, &reg, "ticket", with_rows, &ann)
        .await
        .unwrap();
    assert_eq!(made.tables["lines"].len(), 2);
    let fetched = get(client, &reg, "ticket", &made.name, &ann)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(fetched.tables["lines"].len(), 2);
    // The child DocType has no rules and is not addressable at all.
    let error = get(client, &reg, "ticket_line", "x", &admin())
        .await
        .unwrap_err();
    assert!(matches!(error, DbError::ChildDocType { .. }), "{error:?}");
    let error = get(client, &reg, "ticket_line", "x", &ann)
        .await
        .unwrap_err();
    assert!(matches!(error, DbError::ChildDocType { .. }), "{error:?}");
}

#[tokio::test]
async fn invalid_caller_names_are_rejected() {
    let (db, reg) = setup().await;
    let mut client = db.pool.get().await.unwrap();
    for name in ["", "a\0b"] {
        for caller in [
            Caller::user(name, vec![ADMINISTRATOR.to_owned()]),
            Caller::system(name),
        ] {
            let error = insert(
                &mut **client,
                &reg,
                "ticket",
                doc(&[("title", text("t"))]),
                &caller,
            )
            .await
            .unwrap_err();
            assert!(matches!(error, DbError::InvalidActor), "{error:?}");
        }
    }
}

// ---- Decision 9: links and what errors say -------------------------------------------------

fn link_not_found(error: &DbError) -> Vec<(String, String)> {
    match error {
        DbError::Validation(errors) => errors
            .iter()
            .map(|e| match e {
                ValidationError::LinkNotFound {
                    field,
                    target,
                    name,
                } => (format!("{field}:{target}"), name.clone()),
                ValidationError::InRow { field, row, error } => match error.as_ref() {
                    ValidationError::LinkNotFound {
                        field: f,
                        target,
                        name,
                    } => (format!("{field}[{row}].{f}:{target}"), name.clone()),
                    other => panic!("unexpected {other:?}"),
                },
                other => panic!("unexpected {other:?}"),
            })
            .collect(),
        other => panic!("expected a validation error, got {other:?}"),
    }
}

fn ticket_linking(customer: &str, contact: Option<&str>) -> Document {
    let mut document = doc(&[("title", text("t")), ("customer", text(customer))]);
    if let Some(contact) = contact {
        document
            .tables
            .insert("lines".into(), vec![row(Some(contact))]);
    }
    document
}

#[tokio::test]
async fn a_link_to_a_document_the_caller_cannot_read_is_reported_as_missing() {
    let (db, reg) = setup().await;
    make_party(&db, &reg, "real@x.y", &sys()).await;
    let mut client = db.pool.get().await.unwrap();
    let client = &mut **client;
    // `Own` may create tickets but has no right on `party` at all.
    let blind = user("ann", &["Own"]);

    for (customer, contact) in [
        ("real@x.y", Some("real@x.y")),
        ("ghost@x.y", Some("ghost@x.y")),
    ] {
        let error = insert(
            client,
            &reg,
            "ticket",
            ticket_linking(customer, contact),
            &blind,
        )
        .await
        .unwrap_err();
        // The same errors for the hidden and for the missing target (names differ, as given).
        assert_eq!(
            link_not_found(&error),
            vec![
                ("customer:party".to_owned(), customer.to_owned()),
                ("lines[1].contact:party".to_owned(), customer.to_owned()),
            ],
            "{error:?}"
        );
        assert_eq!(
            error.to_string(),
            format!(
                "validation failed: field `customer`: party `{customer}` does not exist; \
                 field `lines` row 1: field `contact`: party `{customer}` does not exist"
            )
        );
    }

    // The same on update: the real target is "missing" for a caller who cannot read it.
    let ann_ticket = make_ticket(&db, &reg, &blind).await;
    let error = update(
        client,
        &reg,
        "ticket",
        &ann_ticket.name,
        1,
        &change("customer", text("real@x.y")),
        &blind,
    )
    .await
    .unwrap_err();
    assert_eq!(
        link_not_found(&error),
        vec![("customer:party".to_owned(), "real@x.y".to_owned())]
    );
    // Nothing was linked or locked: the system can delete the party again.
    let party = get(client, &reg, "party", "real@x.y", &sys())
        .await
        .unwrap()
        .unwrap();
    delete(client, &reg, "party", "real@x.y", party.version, &sys())
        .await
        .unwrap();
}

#[tokio::test]
async fn an_if_owner_read_resolves_only_own_link_targets() {
    let (db, reg) = setup().await;
    make_party(&db, &reg, "mine@x.y", &user("ann", &["PartyCreator"])).await;
    make_party(&db, &reg, "theirs@x.y", &user("bob", &["PartyCreator"])).await;
    let mut client = db.pool.get().await.unwrap();
    let client = &mut **client;
    let ann = user("ann", &["Own", "OwnReader"]);
    let ok = insert(
        client,
        &reg,
        "ticket",
        ticket_linking("mine@x.y", Some("mine@x.y")),
        &ann,
    )
    .await
    .unwrap();
    assert_eq!(ok.values["customer"], text("mine@x.y"));
    let error = insert(
        client,
        &reg,
        "ticket",
        ticket_linking("theirs@x.y", None),
        &ann,
    )
    .await
    .unwrap_err();
    assert_eq!(
        link_not_found(&error),
        vec![("customer:party".to_owned(), "theirs@x.y".to_owned())]
    );
    // A caller with full read access on `party` resolves both.
    let reader = user("ann", &["Own", "Reader"]);
    let ok = insert(
        client,
        &reg,
        "ticket",
        ticket_linking("theirs@x.y", None),
        &reader,
    )
    .await
    .unwrap();
    assert_eq!(ok.values["customer"], text("theirs@x.y"));
}

#[tokio::test]
async fn amend_resolves_links_with_the_callers_rights() {
    let (db, reg) = setup().await;
    make_party(&db, &reg, "real@x.y", &sys()).await;
    let mut client = db.pool.get().await.unwrap();
    let client = &mut **client;
    let made = insert(
        client,
        &reg,
        "ticket",
        ticket_linking("real@x.y", None),
        &sys(),
    )
    .await
    .unwrap();
    let v = submit(client, &reg, "ticket", &made.name, 1, &sys())
        .await
        .unwrap()
        .version;
    let v = cancel(client, &reg, "ticket", &made.name, v, &sys())
        .await
        .unwrap()
        .version;
    // `Amender` reads and amends tickets but cannot read `party`.
    let error = amend(
        client,
        &reg,
        "ticket",
        &made.name,
        v,
        &user("u", &["Amender"]),
    )
    .await
    .unwrap_err();
    assert_eq!(
        link_not_found(&error),
        vec![("customer:party".to_owned(), "real@x.y".to_owned())]
    );
    let ok = amend(
        client,
        &reg,
        "ticket",
        &made.name,
        v,
        &user("u", &["Amender", "Reader"]),
    )
    .await;
    // `Reader` reads tickets and parties alike: now the link resolves.
    assert_eq!(ok.unwrap().values["customer"], text("real@x.y"));
}

#[tokio::test]
async fn a_still_linked_error_names_the_linking_doctype_only_to_a_caller_who_can_read_it() {
    let (db, reg) = setup().await;
    let party = make_party(&db, &reg, "p@x.y", &sys()).await;
    let mut client = db.pool.get().await.unwrap();
    let client = &mut **client;
    insert(
        client,
        &reg,
        "ticket",
        ticket_linking("p@x.y", None),
        &sys(),
    )
    .await
    .unwrap();

    let blind = user("u", &["PartyAdmin"]); // reads and deletes parties, nothing on tickets
    let error = delete(client, &reg, "party", &party.name, party.version, &blind)
        .await
        .unwrap_err();
    assert!(
        matches!(
            &error,
            DbError::LinkedDocumentExists {
                referenced_by: None,
                ..
            }
        ),
        "{error:?}"
    );
    let message = error.to_string();
    assert_eq!(
        message,
        "party `p@x.y` cannot be deleted: it is linked from another document"
    );

    // With read access to tickets the DocType is named.
    let sighted = user("u", &["PartyAdmin", "Reader"]);
    let error = delete(client, &reg, "party", &party.name, party.version, &sighted)
        .await
        .unwrap_err();
    assert!(
        matches!(&error, DbError::LinkedDocumentExists { referenced_by: Some(by), .. } if by == "ticket"),
        "{error:?}"
    );
    assert!(error.to_string().contains("ticket"));

    // A link in a child row names the owning (parent) DocType, to a reader of it only.
    let lone = make_party(&db, &reg, "q@x.y", &sys()).await;
    insert(
        client,
        &reg,
        "ticket",
        ticket_linking("p@x.y", Some("q@x.y")),
        &sys(),
    )
    .await
    .unwrap();
    let error = delete(client, &reg, "party", &lone.name, lone.version, &blind)
        .await
        .unwrap_err();
    assert!(
        matches!(
            &error,
            DbError::LinkedDocumentExists {
                referenced_by: None,
                ..
            }
        ),
        "{error:?}"
    );
    let error = delete(client, &reg, "party", &lone.name, lone.version, &sighted)
        .await
        .unwrap_err();
    assert!(
        matches!(&error, DbError::LinkedDocumentExists { referenced_by: Some(by), .. } if by == "ticket"),
        "{error:?}"
    );
    // The system (and an administrator) see the DocType.
    let error = delete(client, &reg, "party", &lone.name, lone.version, &sys())
        .await
        .unwrap_err();
    assert!(
        matches!(
            &error,
            DbError::LinkedDocumentExists {
                referenced_by: Some(_),
                ..
            }
        ),
        "{error:?}"
    );
}

#[tokio::test]
async fn permission_denied_never_names_a_document() {
    let (db, reg) = setup().await;
    let made = make_ticket(&db, &reg, &sys()).await;
    let mut client = db.pool.get().await.unwrap();
    let error = update(
        &mut **client,
        &reg,
        "ticket",
        &made.name,
        1,
        &change("title", text("x")),
        &user("u", &["Reader"]),
    )
    .await
    .unwrap_err();
    let message = error.to_string();
    assert_eq!(
        message,
        "permission denied: no `write` right on DocType `ticket`"
    );
    assert!(!message.contains(&made.name));
}

// ---- Hooks ---------------------------------------------------------------------------------

#[tokio::test]
async fn hooks_receive_the_username_and_the_owner_is_the_username() {
    struct Spy(Arc<Mutex<Vec<String>>>);
    impl DocTypeHooks for Spy {
        fn before_insert(&self, ctx: &HookContext<'_>, _: &mut Document) -> Result<(), HookError> {
            self.0.lock().unwrap().push(format!("insert:{}", ctx.actor));
            Ok(())
        }
        fn validate(&self, ctx: &HookContext<'_>, _: &mut Document) -> Result<(), HookError> {
            self.0
                .lock()
                .unwrap()
                .push(format!("validate:{}", ctx.actor));
            Ok(())
        }
        fn before_delete(&self, ctx: &HookContext<'_>, _: &Document) -> Result<(), HookError> {
            self.0.lock().unwrap().push(format!("delete:{}", ctx.actor));
            Ok(())
        }
        fn before_cancel(&self, ctx: &HookContext<'_>, _: &Document) -> Result<(), HookError> {
            self.0.lock().unwrap().push(format!("cancel:{}", ctx.actor));
            Ok(())
        }
    }
    let log = Arc::new(Mutex::new(Vec::new()));
    let mut reg = registry(&[TICKET, TICKET_LINE, PARTY, MEMO, SECRET]);
    reg.add_hooks("ticket", Arc::new(Spy(log.clone()))).unwrap();
    let (db, reg) = setup_with(reg).await;
    let mut client = db.pool.get().await.unwrap();
    let client = &mut **client;
    let ann = user("ann", &["Own"]);
    let made = insert(client, &reg, "ticket", doc(&[("title", text("t"))]), &ann)
        .await
        .unwrap();
    assert_eq!(made.owner, "ann");
    let v = submit(client, &reg, "ticket", &made.name, 1, &ann)
        .await
        .unwrap()
        .version;
    let v = cancel(client, &reg, "ticket", &made.name, v, &ann)
        .await
        .unwrap()
        .version;
    delete(client, &reg, "ticket", &made.name, v, &ann)
        .await
        .unwrap();
    let seen = std::mem::take(&mut *log.lock().unwrap());
    assert_eq!(
        seen,
        [
            "insert:ann",
            "validate:ann",
            "validate:ann",
            "cancel:ann",
            "delete:ann"
        ]
    );
    // A refused operation runs no hook.
    let denied = insert(
        client,
        &reg,
        "ticket",
        doc(&[("title", text("t"))]),
        &user("bob", &[]),
    )
    .await
    .unwrap_err();
    assert!(matches!(denied, DbError::PermissionDenied { .. }));
    assert!(log.lock().unwrap().is_empty());
}

/// Links already stored on a document do not block `update` and `submit` when the caller cannot
/// read their targets; only new or changed Link values get the caller's read check.
#[tokio::test]
async fn stored_links_to_unreadable_targets_do_not_block_update_or_submit() {
    let (db, reg) = setup().await;
    let hidden = make_party(&db, &reg, "hidden@x.y", &sys()).await;
    let other = make_party(&db, &reg, "other@x.y", &sys()).await;
    let u = user("u", &["Writer", "Submitter", "OwnReader", "PartyCreator"]);
    let mut client = db.pool.get().await.unwrap();
    let client = &mut **client;
    let own = insert(
        client,
        &reg,
        "party",
        doc(&[("email", text("own@x.y"))]),
        &u,
    )
    .await
    .unwrap();
    // `u` cannot read `hidden` and `other`, but can read `own`.
    assert!(
        get(client, &reg, "party", &hidden.name, &u)
            .await
            .unwrap()
            .is_none()
    );
    let made = insert(
        client,
        &reg,
        "ticket",
        ticket_linking(&hidden.name, Some(&hidden.name)),
        &sys(),
    )
    .await
    .unwrap();

    // Only the title changes: the stored Links (document and row) are kept.
    let updated = update(
        client,
        &reg,
        "ticket",
        &made.name,
        made.version,
        &change("title", text("changed")),
        &u,
    )
    .await
    .unwrap();
    assert_eq!(updated.values["customer"], text(&hidden.name));

    // Replacing the rows: a stored name is kept, a new unreadable one is reported as missing.
    let rows = |contacts: &[&str]| Changes {
        values: BTreeMap::new(),
        tables: BTreeMap::from([(
            "lines".to_owned(),
            contacts.iter().map(|c| row(Some(c))).collect::<Vec<_>>(),
        )]),
    };
    let error = update(
        client,
        &reg,
        "ticket",
        &made.name,
        updated.version,
        &rows(&[&hidden.name, &other.name]),
        &u,
    )
    .await
    .unwrap_err();
    assert_eq!(
        link_not_found(&error),
        vec![("lines[2].contact:party".to_owned(), other.name.clone())]
    );
    let updated = update(
        client,
        &reg,
        "ticket",
        &made.name,
        updated.version,
        &rows(&[&hidden.name, &own.name]),
        &u,
    )
    .await
    .unwrap();
    assert_eq!(updated.tables["lines"].len(), 2);

    // Changing the document Link to another unreadable party is still refused; to an own one
    // it works.
    let error = update(
        client,
        &reg,
        "ticket",
        &made.name,
        updated.version,
        &change("customer", text(&other.name)),
        &u,
    )
    .await
    .unwrap_err();
    assert_eq!(
        link_not_found(&error),
        vec![("customer:party".to_owned(), other.name.clone())]
    );
    let updated = update(
        client,
        &reg,
        "ticket",
        &made.name,
        updated.version,
        &change("customer", text(&own.name)),
        &u,
    )
    .await
    .unwrap();
    assert_eq!(updated.values["customer"], text(&own.name));

    // Submit with stored Links to unreadable targets (a second ticket, rows included).
    let second = insert(
        client,
        &reg,
        "ticket",
        ticket_linking(&hidden.name, Some(&hidden.name)),
        &sys(),
    )
    .await
    .unwrap();
    let submitted = submit(client, &reg, "ticket", &second.name, second.version, &u)
        .await
        .unwrap();
    assert_eq!(
        submitted.docstatus,
        Some(forgebase_core::document::DocStatus::Submitted)
    );
}

// ---- Stored Links are kept per field and per table ----------------------------------------

/// Two Link fields with different targets and two child tables, one Link in each. The caller
/// (`CaseWriter`) can read neither `party` nor `vendor`.
const CASE: &str = r#"
name = "case"
label = "Case"
[[fields]]
name = "a"
type = "link"
target = "party"
[[fields]]
name = "b"
type = "link"
target = "vendor"
[[fields]]
name = "rows"
type = "table"
target = "case_row"
[[fields]]
name = "others"
type = "table"
target = "case_other"
[[permissions]]
role = "CaseWriter"
read = true
write = true
create = true
"#;

const CASE_ROW: &str = r#"
name = "case_row"
label = "Case Row"
is_child = true
[[fields]]
name = "a_ref"
type = "link"
target = "party"
"#;

const CASE_OTHER: &str = r#"
name = "case_other"
label = "Case Other"
is_child = true
[[fields]]
name = "b_ref"
type = "link"
target = "vendor"
"#;

const VENDOR: &str = r#"
name = "vendor"
label = "Vendor"
naming = { field = "email" }
[[fields]]
name = "email"
type = "data"
required = true
unique = true
"#;

/// A stored Link is kept only in the field (and table) it is stored in: the same name moved to
/// another field of another target DocType, or to the other table, is a new value and is checked
/// against the caller's read access like any other.
#[tokio::test]
async fn the_kept_rule_is_scoped_per_field_and_per_table() {
    let (db, reg) = setup_with(registry(&[
        CASE,
        CASE_ROW,
        CASE_OTHER,
        PARTY,
        VENDOR,
        TICKET,
        TICKET_LINE,
        MEMO,
        SECRET,
    ]))
    .await;
    let mut client = db.pool.get().await.unwrap();
    let client = &mut **client;
    // The same names exist as `party` and as `vendor`, so only the permission can refuse them.
    for name in ["x1@x.y", "y1@x.y"] {
        make_party(&db, &reg, name, &sys()).await;
        insert(
            client,
            &reg,
            "vendor",
            doc(&[("email", text(name))]),
            &sys(),
        )
        .await
        .unwrap();
    }
    let u = user("u", &["CaseWriter"]);
    let mut stored = doc(&[("a", text("x1@x.y")), ("b", text("y1@x.y"))]);
    stored.tables.insert(
        "rows".into(),
        vec![BTreeMap::from([("a_ref".to_owned(), text("x1@x.y"))])],
    );
    stored.tables.insert(
        "others".into(),
        vec![BTreeMap::from([("b_ref".to_owned(), text("y1@x.y"))])],
    );
    let made = insert(client, &reg, "case", stored, &sys()).await.unwrap();

    let rows = |table: &str, field: &str, name: &str| Changes {
        values: BTreeMap::new(),
        tables: BTreeMap::from([(
            table.to_owned(),
            vec![BTreeMap::from([(field.to_owned(), text(name))])],
        )]),
    };
    let mut version = made.version;

    // Refused: a stored name moved to the other Link field.
    for (changes, expected) in [
        (change("b", text("x1@x.y")), "b:vendor"),
        (change("a", text("y1@x.y")), "a:party"),
        // Moved between the tables: each table keeps only its own stored value.
        (rows("others", "b_ref", "x1@x.y"), "others[1].b_ref:vendor"),
        (rows("rows", "a_ref", "y1@x.y"), "rows[1].a_ref:party"),
    ] {
        let error = update(client, &reg, "case", &made.name, version, &changes, &u)
            .await
            .unwrap_err();
        let found = link_not_found(&error);
        assert_eq!(found.len(), 1, "{expected}: {error:?}");
        assert_eq!(found[0].0, expected);
    }
    // Kept: the same values in their own fields and tables (a changed title would be no
    // change at all, so the Links are rewritten as they are stored).
    let same = Changes {
        values: BTreeMap::from([
            ("a".to_owned(), text("x1@x.y")),
            ("b".to_owned(), text("y1@x.y")),
        ]),
        tables: BTreeMap::from([
            (
                "rows".to_owned(),
                vec![
                    BTreeMap::from([("a_ref".to_owned(), text("x1@x.y"))]),
                    BTreeMap::from([("a_ref".to_owned(), text("x1@x.y"))]),
                ],
            ),
            (
                "others".to_owned(),
                vec![BTreeMap::from([("b_ref".to_owned(), text("y1@x.y"))])],
            ),
        ]),
    };
    let updated = update(client, &reg, "case", &made.name, version, &same, &u)
        .await
        .unwrap();
    version = updated.version;
    assert_eq!(updated.tables["rows"].len(), 2);
    assert_eq!(version, made.version + 1);
}

/// A `before_submit` hook that sets a Link to a document the caller cannot read gets the same
/// `LinkNotFound` as the caller would for their own input: hooks do not widen read access.
#[tokio::test]
async fn a_before_submit_hook_cannot_link_to_an_unreadable_document() {
    struct Linker(String);
    impl DocTypeHooks for Linker {
        fn before_submit(&self, _: &HookContext<'_>, doc: &mut Document) -> Result<(), HookError> {
            doc.values.insert("customer".to_owned(), text(&self.0));
            Ok(())
        }
    }
    let mut reg = registry(&[TICKET, TICKET_LINE, PARTY, MEMO, SECRET]);
    reg.add_hooks("ticket", Arc::new(Linker("hidden@x.y".to_owned())))
        .unwrap();
    let (db, reg) = setup_with(reg).await;
    let hidden = make_party(&db, &reg, "hidden@x.y", &sys()).await;
    let made = make_ticket(&db, &reg, &sys()).await;
    let mut client = db.pool.get().await.unwrap();
    let client = &mut **client;

    // `Submitter` reads tickets but has no right on `party`.
    let error = submit(
        client,
        &reg,
        "ticket",
        &made.name,
        made.version,
        &user("u", &["Submitter"]),
    )
    .await
    .unwrap_err();
    assert_eq!(
        link_not_found(&error),
        vec![("customer:party".to_owned(), hidden.name.clone())]
    );
    // Nothing was persisted: still a draft, and the same hook is fine for a caller who can read it.
    let stored = get(client, &reg, "ticket", &made.name, &sys())
        .await
        .unwrap()
        .unwrap();
    assert_eq!(stored.version, made.version);
    assert_eq!(stored.values["customer"], Value::Null);
    let submitted = submit(
        client,
        &reg,
        "ticket",
        &made.name,
        made.version,
        &user("v", &["Submitter", "Reader"]),
    )
    .await
    .unwrap();
    assert_eq!(submitted.values["customer"], text(&hidden.name));
}
