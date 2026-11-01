//! Field permission levels (M10.2, ADR 0024) against a real PostgreSQL: what each kind of caller
//! reads and writes, that a hidden field is unknown everywhere (byte-identical errors, checked
//! against a registry where the field does not exist at all), that hidden data survives an
//! update by a lower-level user, hooks, `if_owner`, export, saved views and the leak review.

mod common;

use std::collections::BTreeMap;
use std::sync::{Arc, Mutex};

use common::{TestDb, registry};
use forgebase_core::document::{Changes, Document, Row, ValidationError};
use forgebase_core::hooks::{DocTypeHooks, HookContext, HookError};
use forgebase_core::meta::Registry;
use forgebase_core::permission::{ADMINISTRATOR, Caller};
use forgebase_core::value::Value;
use forgebase_db::{
    AggregateFn, DbError, Export, ExportQuery, Filter, FilterOp, ListQuery, Sort, StoredDocument,
    SummaryQuery, View, ViewDefinition, amend, begin_export, cancel, create_view, get, insert,
    list, list_views, submit, summary, transition, update,
};
use serde_json::json;

/// The account: `credit_limit` (level 1), `vault_code` (level 2, no rule names it), a table
/// `ledger` at level 1, a unique `internal_ref` and a Link `partner` at level 1. `hidden`
/// leaves those fields out, which is what the caller without them "sees".
fn account(with_levels: bool) -> String {
    let hidden = if with_levels {
        r#"
[[fields]]
name = "credit_limit"
type = "int"
perm_level = 1
[[fields]]
name = "vault_code"
type = "data"
perm_level = 2
[[fields]]
name = "internal_ref"
type = "data"
unique = true
perm_level = 1
[[fields]]
name = "partner"
type = "link"
target = "party"
perm_level = 1
[[fields]]
name = "grade"
type = "select"
options = ["a", "b"]
perm_level = 1
[[fields]]
name = "ledger"
type = "table"
target = "ledger_line"
perm_level = 1
"#
    } else {
        ""
    };
    format!(
        r#"
name = "account"
label = "Account"
title_field = "title"
[[fields]]
name = "title"
type = "data"
required = true
[[fields]]
name = "region"
type = "data"
[[fields]]
name = "revenue"
type = "int"
{hidden}
[[permissions]]
role = "User"
read = true
write = true
create = true
delete = true
export = true
[[permissions]]
role = "Manager"
read = true
write = true
create = true
export = true
[[permissions]]
role = "Manager"
level = 1
read = true
write = true
[[permissions]]
role = "Auditor"
read = true
[[permissions]]
role = "Auditor"
level = 1
read = true
[[permissions]]
role = "Mine"
read = true
write = true
create = true
if_owner = true
[[permissions]]
role = "Mine"
level = 1
read = true
[[permissions]]
role = "Viewer"
read = true
[[permissions]]
role = "Spy"
level = 2
read = true
write = true
"#
    )
}

const LEDGER_LINE: &str = r#"
name = "ledger_line"
label = "Ledger Line"
is_child = true
[[fields]]
name = "qty"
type = "int"
"#;

const PARTY: &str = r#"
name = "party"
label = "Party"
[[fields]]
name = "label"
type = "data"
[[permissions]]
role = "User"
read = true
create = true
[[permissions]]
role = "Manager"
read = true
create = true
"#;

/// A submittable DocType with a note at level 1; `Clerk` has every document right.
const INVOICE: &str = r#"
name = "invoice"
label = "Invoice"
submittable = true
[[fields]]
name = "title"
type = "data"
[[fields]]
name = "internal_note"
type = "data"
perm_level = 1
[[permissions]]
role = "Clerk"
read = true
write = true
create = true
submit = true
cancel = true
amend = true
[[permissions]]
role = "Manager"
read = true
write = true
create = true
[[permissions]]
role = "Manager"
level = 1
read = true
write = true
"#;

/// A workflow DocType with a note at level 1.
const TICKET: &str = r#"
name = "ticket"
label = "Ticket"
[[fields]]
name = "title"
type = "data"
[[fields]]
name = "secret_note"
type = "data"
perm_level = 1
[[permissions]]
role = "Clerk"
read = true
write = true
create = true
[[permissions]]
role = "Manager"
read = true
write = true
create = true
[[permissions]]
role = "Manager"
level = 1
read = true
write = true
[workflow]
[[workflow.states]]
name = "Open"
docstatus = 0
[[workflow.states]]
name = "Done"
docstatus = 0
[[workflow.transitions]]
action = "Finish"
from = "Open"
to = "Done"
roles = ["Clerk"]
"#;

/// What the hook of `account` saw on its last `validate`, and what it sets for the leak tests.
#[derive(Default)]
struct Spy {
    seen: Mutex<Vec<Option<Value>>>,
    calls: Mutex<usize>,
}

struct SpyHook(Arc<Spy>);

impl DocTypeHooks for SpyHook {
    fn validate(&self, _ctx: &HookContext<'_>, doc: &mut Document) -> Result<(), HookError> {
        *self.0.calls.lock().unwrap() += 1;
        self.0
            .seen
            .lock()
            .unwrap()
            .push(doc.values.get("credit_limit").cloned());
        match doc.values.get("title") {
            Some(Value::Text(t)) if t == "dup" => {
                doc.values
                    .insert("internal_ref".into(), Value::Text("same".into()));
            }
            Some(Value::Text(t)) if t == "badhook" => {
                doc.values
                    .insert("credit_limit".into(), Value::Text("not a number".into()));
            }
            Some(Value::Text(t)) if t == "ghostlink" => {
                doc.values
                    .insert("partner".into(), Value::Text("no such party".into()));
            }
            _ => {}
        }
        Ok(())
    }
}

fn texts(with_levels: bool) -> Vec<String> {
    vec![
        account(with_levels),
        LEDGER_LINE.to_owned(),
        PARTY.to_owned(),
        INVOICE.to_owned(),
        TICKET.to_owned(),
    ]
}

fn build(with_levels: bool) -> (Registry, Arc<Spy>) {
    let texts = texts(with_levels);
    let refs: Vec<&str> = texts.iter().map(String::as_str).collect();
    let mut reg = registry(&refs);
    let spy = Arc::new(Spy::default());
    reg.add_hooks("account", Arc::new(SpyHook(spy.clone())))
        .expect("hooks");
    (reg, spy)
}

struct Env {
    db: TestDb,
    reg: Registry,
    /// The same DocType without the levelled fields: the "unknown field" reference.
    plain_reg: Registry,
    spy: Arc<Spy>,
}

async fn env() -> Env {
    let (reg, spy) = build(true);
    let (plain_reg, _) = build(false);
    let db = TestDb::migrated().await;
    db.sync(&reg).await.expect("sync");
    Env {
        db,
        reg,
        plain_reg,
        spy,
    }
}

fn user(name: &str, roles: &[&str]) -> Caller {
    Caller::user(name, roles.iter().map(|r| (*r).to_owned()).collect())
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

fn changes(pairs: &[(&str, Value)]) -> Changes {
    Changes {
        values: pairs
            .iter()
            .map(|(k, v)| ((*k).to_owned(), v.clone()))
            .collect(),
        tables: BTreeMap::new(),
    }
}

fn line(qty: i64) -> Row {
    BTreeMap::from([("qty".to_owned(), Value::Int(qty))])
}

impl Env {
    async fn insert(
        &self,
        doctype: &str,
        document: Document,
        who: &Caller,
    ) -> Result<StoredDocument, DbError> {
        let mut client = self.db.pool.get().await.unwrap();
        insert(&mut **client, &self.reg, doctype, document, who).await
    }

    async fn update(
        &self,
        doctype: &str,
        stored: &StoredDocument,
        changes: &Changes,
        who: &Caller,
    ) -> Result<StoredDocument, DbError> {
        let mut client = self.db.pool.get().await.unwrap();
        update(
            &mut **client,
            &self.reg,
            doctype,
            &stored.name,
            stored.version,
            changes,
            who,
        )
        .await
    }

    async fn get(&self, doctype: &str, name: &str, who: &Caller) -> Option<StoredDocument> {
        let mut client = self.db.pool.get().await.unwrap();
        get(&mut **client, &self.reg, doctype, name, who)
            .await
            .expect("get")
    }

    async fn list(&self, who: &Caller, query: &ListQuery) -> Result<Vec<StoredDocument>, DbError> {
        self.list_in(&self.reg, who, query).await
    }

    async fn list_in(
        &self,
        reg: &Registry,
        who: &Caller,
        query: &ListQuery,
    ) -> Result<Vec<StoredDocument>, DbError> {
        let client = self.db.pool.get().await.unwrap();
        list(&**client, reg, "account", who, query).await
    }

    /// An account made by the administrator with every level filled.
    async fn full_account(&self) -> StoredDocument {
        self.full_account_ref("REF-1").await
    }

    async fn full_account_ref(&self, internal_ref: &str) -> StoredDocument {
        let mut document = doc(&[
            ("title", text("Acme")),
            ("region", text("north")),
            ("revenue", Value::Int(10)),
            ("credit_limit", Value::Int(500)),
            ("vault_code", text("1234")),
            ("internal_ref", text(internal_ref)),
        ]);
        document
            .tables
            .insert("ledger".to_owned(), vec![line(1), line(2)]);
        self.insert("account", document, &admin())
            .await
            .expect("insert")
    }
}

fn admin() -> Caller {
    user("root", &[ADMINISTRATOR])
}

fn sys() -> Caller {
    Caller::system("sys")
}

fn query_filter(field: &str) -> ListQuery {
    ListQuery {
        filters: vec![Filter::new(field, FilterOp::Eq(Value::Int(500)))],
        ..ListQuery::default()
    }
}

fn message(result: Result<impl std::fmt::Debug, DbError>) -> String {
    result.expect_err("refused").to_string()
}

// ----- Reads -------------------------------------------------------------------------------

#[tokio::test]
async fn a_user_without_level_one_does_not_see_the_field_anywhere() {
    let env = env().await;
    let stored = env.full_account().await;
    let user = user("u", &["User"]);

    // Get.
    let got = env
        .get("account", &stored.name, &user)
        .await
        .expect("found");
    for hidden in ["credit_limit", "vault_code", "internal_ref", "partner"] {
        assert!(!got.values.contains_key(hidden), "{hidden}");
    }
    assert!(!got.tables.contains_key("ledger"));
    assert_eq!(got.values["title"], text("Acme"));
    assert_eq!(got.values["revenue"], Value::Int(10));
    // List.
    let rows = env.list(&user, &ListQuery::default()).await.expect("list");
    assert_eq!(rows.len(), 1);
    assert!(!rows[0].values.contains_key("credit_limit"));
    // The standard columns are level 0.
    assert_eq!(rows[0].owner, "root");

    // Administrator and System see every level.
    for who in [admin(), sys()] {
        let all = env.get("account", &stored.name, &who).await.expect("found");
        assert_eq!(all.values["credit_limit"], Value::Int(500));
        assert_eq!(all.values["vault_code"], text("1234"));
        assert_eq!(all.tables["ledger"].len(), 2);
        let rows = env.list(&who, &ListQuery::default()).await.expect("list");
        assert_eq!(rows[0].values["internal_ref"], text("REF-1"));
    }
}

#[tokio::test]
async fn manager_sees_and_writes_level_one_but_not_level_two() {
    let env = env().await;
    let stored = env.full_account().await;
    let manager = user("m", &["Manager"]);
    let got = env
        .get("account", &stored.name, &manager)
        .await
        .expect("found");
    assert_eq!(got.values["credit_limit"], Value::Int(500));
    assert_eq!(got.tables["ledger"].len(), 2);
    assert!(!got.values.contains_key("vault_code"));
    // Writes level 1, also the table.
    let mut change = changes(&[("credit_limit", Value::Int(900))]);
    change.tables.insert("ledger".to_owned(), vec![line(7)]);
    let updated = env
        .update("account", &stored, &change, &manager)
        .await
        .expect("update");
    assert_eq!(updated.values["credit_limit"], Value::Int(900));
    assert_eq!(updated.tables["ledger"], vec![line(7)]);
    assert!(!updated.values.contains_key("vault_code"));
    // Level 2 is unknown to the Manager, whatever the value.
    let err = message(
        env.update(
            "account",
            &updated,
            &changes(&[("vault_code", text("x"))]),
            &manager,
        )
        .await,
    );
    assert_eq!(err, "validation failed: unknown field `vault_code`");
    // The Spy role has a level 2 rule but no level 0 right: it gets nothing.
    let spy = user("s", &["Spy"]);
    assert!(matches!(
        env.list(&spy, &ListQuery::default()).await,
        Err(DbError::PermissionDenied { .. })
    ));
    // Spy with a level 0 role too reads and writes level 2, and not level 1.
    let both = user("s", &["Spy", "Viewer"]);
    let got = env
        .get("account", &stored.name, &both)
        .await
        .expect("found");
    assert_eq!(got.values["vault_code"], text("1234"));
    assert!(!got.values.contains_key("credit_limit"));
}

#[tokio::test]
async fn hidden_fields_are_unknown_to_filters_sort_summary_and_export_with_identical_errors() {
    let env = env().await;
    env.full_account().await;
    let user = user("u", &["User"]);
    // The reference: the same query on a registry where the field does not exist.
    for field in [
        "credit_limit",
        "vault_code",
        "internal_ref",
        "partner",
        "ledger",
    ] {
        // Filter.
        let query = query_filter(field);
        let hidden = message(env.list(&user, &query).await);
        let missing = message(env.list_in(&env.plain_reg, &user, &query).await);
        // `ledger` does not exist in the plain registry either, so the messages are those of an
        // unknown field in both.
        assert_eq!(hidden, missing, "filter {field}");
        assert!(hidden.contains("unknown field"), "{hidden}");
        // Sort.
        let query = ListQuery {
            sort: vec![Sort::desc(field)],
            ..ListQuery::default()
        };
        assert_eq!(
            message(env.list(&user, &query).await),
            message(env.list_in(&env.plain_reg, &user, &query).await),
            "sort {field}"
        );
        // Group by.
        let group = SummaryQuery {
            filters: vec![],
            group_by: field.to_owned(),
            aggregates: vec![],
        };
        let client = env.db.pool.get().await.unwrap();
        let hidden = message(summary(&**client, &env.reg, "account", &user, &group).await);
        let missing = message(summary(&**client, &env.plain_reg, "account", &user, &group).await);
        assert_eq!(hidden, missing, "group_by {field}");
        // An aggregate.
        let aggregate = SummaryQuery {
            filters: vec![],
            group_by: "region".to_owned(),
            aggregates: vec![(AggregateFn::Sum, field.to_owned())],
        };
        assert_eq!(
            message(summary(&**client, &env.reg, "account", &user, &aggregate).await),
            message(summary(&**client, &env.plain_reg, "account", &user, &aggregate).await),
            "aggregate {field}"
        );
    }
    // A group-by on a visible field and an aggregate on one work as always.
    let client = env.db.pool.get().await.unwrap();
    let ok = summary(
        &**client,
        &env.reg,
        "account",
        &user,
        &SummaryQuery {
            filters: vec![],
            group_by: "region".to_owned(),
            aggregates: vec![(AggregateFn::Sum, "revenue".to_owned())],
        },
    )
    .await
    .expect("summary");
    assert_eq!(ok.groups.len(), 1);
    // The Administrator may use them.
    let rows = env
        .list(&admin(), &query_filter("credit_limit"))
        .await
        .expect("admin filter");
    assert_eq!(rows.len(), 1);
}

async fn export(
    env: &Env,
    reg: &Registry,
    who: &Caller,
    filters: &[Filter],
    sort: &[Sort],
) -> Result<Vec<StoredDocument>, DbError> {
    let mut client = env.db.pool.get().await.unwrap();
    let tx = begin_export(&mut client).await?;
    let query = ExportQuery {
        doctype: "account",
        filters,
        sort,
        limit: 1000,
        with_tables: true,
    };
    let mut export = Export::start(&tx, reg, who, &query).await?;
    let mut all = Vec::new();
    while let Some(page) = export.next_page(&tx, reg).await? {
        all.extend(page);
    }
    Ok(all)
}

#[tokio::test]
async fn export_leaves_out_hidden_fields_and_refuses_them_as_unknown() {
    let env = env().await;
    env.full_account().await;
    let user = user("u", &["User"]);
    let docs = export(&env, &env.reg, &user, &[], &[])
        .await
        .expect("export");
    assert_eq!(docs.len(), 1);
    assert!(!docs[0].values.contains_key("credit_limit"));
    assert!(!docs[0].tables.contains_key("ledger"));
    // Manager: level 1 included.
    let docs = export(&env, &env.reg, &user_with("m", "Manager"), &[], &[])
        .await
        .expect("export");
    assert_eq!(docs[0].values["credit_limit"], Value::Int(500));
    assert_eq!(docs[0].tables["ledger"].len(), 2);
    // Administrator: everything.
    let docs = export(&env, &env.reg, &admin(), &[], &[])
        .await
        .expect("export");
    assert_eq!(docs[0].values["vault_code"], text("1234"));
    // Filters and sort on a hidden field: the error of an unknown field.
    let filter = [Filter::new("credit_limit", FilterOp::Eq(Value::Int(1)))];
    let sort = [Sort::asc("credit_limit")];
    assert_eq!(
        message(export(&env, &env.reg, &user, &filter, &[]).await),
        message(export(&env, &env.plain_reg, &user, &filter, &[]).await)
    );
    assert_eq!(
        message(export(&env, &env.reg, &user, &[], &sort).await),
        message(export(&env, &env.plain_reg, &user, &[], &sort).await)
    );
}

fn user_with(name: &str, role: &str) -> Caller {
    user(name, &[role])
}

// ----- Writes ------------------------------------------------------------------------------

#[tokio::test]
async fn writing_a_hidden_field_is_an_unknown_field_error_like_a_missing_one() {
    let env = env().await;
    let user = user("u", &["User"]);
    // Insert: the very error of a field that does not exist (any value, null too).
    for value in [Value::Int(1), Value::Null, text("not even an int")] {
        let document = doc(&[("title", text("t")), ("credit_limit", value)]);
        let hidden = message(env.insert("account", document.clone(), &user).await);
        let mut client = env.db.pool.get().await.unwrap();
        let missing =
            message(insert(&mut **client, &env.plain_reg, "account", document, &user).await);
        assert_eq!(hidden, missing);
        assert_eq!(hidden, "validation failed: unknown field `credit_limit`");
    }
    // A hidden Table field in `tables`, and one named in `values`.
    let mut document = doc(&[("title", text("t"))]);
    document.tables.insert("ledger".to_owned(), vec![line(1)]);
    assert_eq!(
        message(env.insert("account", document, &user).await),
        "validation failed: unknown field `ledger`"
    );
    // Nothing was written.
    let rows = env.db.rows("SELECT count(*)::text FROM doc_account").await;
    assert_eq!(rows[0][0], "0");

    // Update: also when the value equals the stored one.
    let stored = env.full_account().await;
    for value in [Value::Int(500), Value::Int(1), Value::Null] {
        let err = message(
            env.update(
                "account",
                &stored,
                &changes(&[("credit_limit", value)]),
                &user,
            )
            .await,
        );
        assert_eq!(err, "validation failed: unknown field `credit_limit`");
    }
    // Several problems are all reported, hidden and missing alike, in field order.
    let err = message(
        env.update(
            "account",
            &stored,
            &changes(&[("credit_limit", Value::Int(1)), ("nope", Value::Int(1))]),
            &user,
        )
        .await,
    );
    assert_eq!(
        err,
        "validation failed: unknown field `credit_limit`; unknown field `nope`"
    );
    // The hook did not run for any of these (it ran once for the administrator's account).
    assert_eq!(*env.spy.calls.lock().unwrap(), 1);
}

#[tokio::test]
async fn an_update_by_a_user_keeps_the_hidden_data_and_returns_documents_without_it() {
    let env = env().await;
    let stored = env.full_account().await;
    let user = user("u", &["User"]);
    let updated = env
        .update(
            "account",
            &stored,
            &changes(&[("title", text("Acme Inc"))]),
            &user,
        )
        .await
        .expect("update");
    // What the call returns has no hidden field.
    assert_eq!(updated.values["title"], text("Acme Inc"));
    assert!(!updated.values.contains_key("credit_limit"));
    assert!(!updated.tables.contains_key("ledger"));
    // What is stored is untouched, rows included (their ids too: the table was not replaced).
    let all = env
        .get("account", &stored.name, &admin())
        .await
        .expect("found");
    assert_eq!(all.values["credit_limit"], Value::Int(500));
    assert_eq!(all.values["vault_code"], text("1234"));
    assert_eq!(all.values["internal_ref"], text("REF-1"));
    assert_eq!(all.tables["ledger"].len(), 2);
    assert_eq!(all.version, 2);
    // Insert returns the document without the hidden fields, too (defaults: none set by a User).
    let created = env
        .insert("account", doc(&[("title", text("New"))]), &user)
        .await
        .expect("insert");
    assert!(!created.values.contains_key("credit_limit"));
    assert!(!created.tables.contains_key("ledger"));
    let stored_new = env
        .get("account", &created.name, &admin())
        .await
        .expect("found");
    assert_eq!(stored_new.values["credit_limit"], Value::Null);
}

#[tokio::test]
async fn a_reader_of_level_one_cannot_write_it_but_may_send_the_stored_value_back() {
    let env = env().await;
    let stored = env.full_account().await;
    // The Auditor reads level 0 and level 1 and has no write right at all: use a caller with the
    // right to write level 0 and read-only level 1 (User + Auditor).
    let both = user("a", &["User", "Auditor"]);
    let got = env
        .get("account", &stored.name, &both)
        .await
        .expect("found");
    assert_eq!(got.values["credit_limit"], Value::Int(500));
    assert_eq!(got.tables["ledger"].len(), 2);

    let calls_before = *env.spy.calls.lock().unwrap();
    // A different value, null over a stored value, and a new row list: refused as not permitted,
    // before any hook runs.
    for change in [
        changes(&[("credit_limit", Value::Int(1))]),
        changes(&[("credit_limit", Value::Null)]),
    ] {
        let err = env
            .update("account", &stored, &change, &both)
            .await
            .expect_err("refused");
        match &err {
            DbError::Validation(errors) => assert_eq!(
                errors,
                &vec![ValidationError::NotPermitted {
                    field: "credit_limit".into()
                }]
            ),
            other => panic!("{other:?}"),
        }
        assert_eq!(
            err.to_string(),
            "validation failed: field `credit_limit` cannot be changed with your permissions"
        );
    }
    let mut rows = Changes::default();
    rows.tables.insert("ledger".to_owned(), vec![line(9)]);
    assert!(matches!(
        env.update("account", &stored, &rows, &both).await,
        Err(DbError::Validation(e)) if e == vec![ValidationError::NotPermitted { field: "ledger".into() }]
    ));
    assert_eq!(*env.spy.calls.lock().unwrap(), calls_before);

    // Sending the stored value (and the stored rows) back is not a change.
    let mut same = changes(&[
        ("credit_limit", Value::Int(500)),
        ("title", text("Renamed")),
    ]);
    same.tables
        .insert("ledger".to_owned(), got.tables["ledger"].clone());
    let updated = env
        .update("account", &stored, &same, &both)
        .await
        .expect("allowed");
    assert_eq!(updated.values["title"], text("Renamed"));
    assert_eq!(updated.values["credit_limit"], Value::Int(500));
    // An insert with a value is refused too; null (nothing) and absence are fine.
    let err = env
        .insert(
            "account",
            doc(&[("title", text("x")), ("credit_limit", Value::Int(1))]),
            &both,
        )
        .await
        .expect_err("refused");
    assert_eq!(
        err.to_string(),
        "validation failed: field `credit_limit` cannot be changed with your permissions"
    );
    env.insert(
        "account",
        doc(&[("title", text("x")), ("credit_limit", Value::Null)]),
        &both,
    )
    .await
    .expect("null is nothing");
    env.insert("account", doc(&[("title", text("y"))]), &both)
        .await
        .expect("absent");
}

#[tokio::test]
async fn administrator_and_system_write_every_level() {
    let env = env().await;
    for (who, internal_ref) in [(admin(), "REF-A"), (sys(), "REF-S")] {
        let stored = env.full_account_ref(internal_ref).await;
        let mut change = changes(&[("credit_limit", Value::Int(1)), ("vault_code", text("9"))]);
        change.tables.insert("ledger".to_owned(), vec![line(5)]);
        let updated = env
            .update("account", &stored, &change, &who)
            .await
            .expect("update");
        assert_eq!(updated.values["credit_limit"], Value::Int(1));
        assert_eq!(updated.values["vault_code"], text("9"));
        assert_eq!(updated.tables["ledger"], vec![line(5)]);
    }
}

#[tokio::test]
async fn a_table_at_level_one_is_hidden_and_protected() {
    let env = env().await;
    let stored = env.full_account().await;
    let user = user("u", &["User"]);
    let mut change = Changes::default();
    change.tables.insert("ledger".to_owned(), vec![]);
    // Clearing the rows of a hidden table is an unknown field, and the rows stay.
    assert_eq!(
        message(env.update("account", &stored, &change, &user).await),
        "validation failed: unknown field `ledger`"
    );
    let all = env
        .get("account", &stored.name, &admin())
        .await
        .expect("found");
    assert_eq!(all.tables["ledger"].len(), 2);
    // A Manager replaces them.
    let manager = user_with("m", "Manager");
    let mut change = Changes::default();
    change.tables.insert("ledger".to_owned(), vec![line(3)]);
    env.update("account", &stored, &change, &manager)
        .await
        .expect("update");
}

// ----- Hooks -------------------------------------------------------------------------------

#[tokio::test]
async fn hooks_see_the_whole_document_during_an_update_by_a_user() {
    let env = env().await;
    let stored = env.full_account().await;
    env.spy.seen.lock().unwrap().clear();
    let user = user("u", &["User"]);
    env.update(
        "account",
        &stored,
        &changes(&[("title", text("Hooked"))]),
        &user,
    )
    .await
    .expect("update");
    let seen = env.spy.seen.lock().unwrap().clone();
    assert_eq!(seen, vec![Some(Value::Int(500))]);
}

// ----- if_owner ----------------------------------------------------------------------------

#[tokio::test]
async fn if_owner_at_level_zero_limits_the_documents_a_level_is_seen_on() {
    let env = env().await;
    let mine = user("mine", &["Mine"]);
    let own = env
        .insert("account", doc(&[("title", text("own"))]), &mine)
        .await
        .expect("insert");
    // Somebody else's document with a level 1 value.
    let other = env.full_account().await;
    // On the own document the level 1 read rule applies; the field is null but present.
    let got = env.get("account", &own.name, &mine).await.expect("own");
    assert_eq!(got.values.get("credit_limit"), Some(&Value::Null));
    // The other's document is not found at all.
    assert!(env.get("account", &other.name, &mine).await.is_none());
    let rows = env
        .list(&mine, &query_filter("credit_limit"))
        .await
        .expect("list");
    assert!(rows.is_empty());
    // Level 1 is read only for `Mine`: writing it is not permitted (and a level 0 write works).
    let err = env
        .update(
            "account",
            &own,
            &changes(&[("credit_limit", Value::Int(1))]),
            &mine,
        )
        .await
        .expect_err("refused");
    assert!(err.to_string().contains("cannot be changed"), "{err}");
    env.update("account", &own, &changes(&[("region", text("x"))]), &mine)
        .await
        .expect("level 0");
}

// ----- Saved views -------------------------------------------------------------------------

#[tokio::test]
async fn a_saved_view_on_a_hidden_field_is_unknown_and_hidden_from_the_list() {
    let env = env().await;
    let user = user("u", &["User"]);
    let definition = |field: &str| ViewDefinition {
        json: json!({
            "filters": [{"field": field, "op": "=", "value": 1}],
            "sort": [],
            "columns": []
        }),
        query: ListQuery {
            filters: vec![Filter::new(field, FilterOp::Eq(Value::Int(1)))],
            ..ListQuery::default()
        },
    };
    let region_definition = ViewDefinition {
        json: json!({
            "filters": [{"field": "region", "op": "=", "value": "north"}],
            "sort": [],
            "columns": []
        }),
        query: ListQuery {
            filters: vec![Filter::new(
                "region",
                FilterOp::Eq(Value::Text("north".into())),
            )],
            ..ListQuery::default()
        },
    };
    // A private view needs a stored user.
    env.db
        .exec("INSERT INTO fb_user (id, username, password_hash) VALUES (gen_random_uuid(), 'u', 'x')")
        .await;
    let mut client = env.db.pool.get().await.unwrap();
    let hidden = message(
        create_view(
            &mut **client,
            &env.reg,
            "account",
            &user,
            "by limit",
            &definition("credit_limit"),
            false,
        )
        .await,
    );
    let missing = message(
        create_view(
            &mut **client,
            &env.plain_reg,
            "account",
            &user,
            "by limit",
            &definition("credit_limit"),
            false,
        )
        .await,
    );
    assert_eq!(hidden, missing);
    assert!(hidden.contains("unknown field"), "{hidden}");
    // A column on a hidden field is unknown too.
    let columns = ViewDefinition {
        json: json!({"filters": [], "sort": [], "columns": ["credit_limit"]}),
        query: ListQuery::default(),
    };
    assert_eq!(
        message(
            create_view(
                &mut **client,
                &env.reg,
                "account",
                &user,
                "cols",
                &columns,
                false
            )
            .await
        ),
        message(
            create_view(
                &mut **client,
                &env.plain_reg,
                "account",
                &user,
                "cols",
                &columns,
                false
            )
            .await
        )
    );
    // An Administrator may share a view on the hidden field; the User does not see it listed
    // (its definition would tell the field), the Manager does.
    create_view(
        &mut **client,
        &env.reg,
        "account",
        &admin(),
        "shared by limit",
        &definition("credit_limit"),
        true,
    )
    .await
    .expect("shared view");
    create_view(
        &mut **client,
        &env.reg,
        "account",
        &admin(),
        "shared by region",
        &region_definition,
        true,
    )
    .await
    .expect("shared view");
    let labels = |views: Vec<View>| -> Vec<String> { views.into_iter().map(|v| v.label).collect() };
    let for_user = list_views(&**client, &env.reg, "account", &user)
        .await
        .expect("list");
    assert_eq!(labels(for_user), ["shared by region"]);
    let for_manager = list_views(&**client, &env.reg, "account", &user_with("m", "Manager"))
        .await
        .expect("list");
    assert_eq!(labels(for_manager), ["shared by limit", "shared by region"]);
    // Running it as a list is refused for the User, like any unknown field.
    assert!(
        env.list(&user, &definition("credit_limit").query)
            .await
            .is_err()
    );
}

// ----- Leak review -------------------------------------------------------------------------

#[tokio::test]
async fn errors_about_hidden_fields_name_neither_the_field_nor_the_value() {
    let env = env().await;
    let user = user("u", &["User"]);

    // A value a hook sets that fails validation: the Administrator is told, the User is not.
    let bad = doc(&[("title", text("badhook"))]);
    let err = env
        .insert("account", bad.clone(), &admin())
        .await
        .expect_err("invalid");
    assert!(err.to_string().contains("credit_limit"), "{err}");
    let err = env
        .insert("account", bad, &user)
        .await
        .expect_err("invalid");
    match &err {
        DbError::Validation(errors) => assert_eq!(errors, &vec![ValidationError::Hidden]),
        other => panic!("{other:?}"),
    }
    let shown = err.to_string();
    assert!(
        !shown.contains("credit_limit") && !shown.contains("not a number"),
        "{shown}"
    );

    // A Link a hook sets to a missing document.
    let ghost = doc(&[("title", text("ghostlink"))]);
    let err = env
        .insert("account", ghost.clone(), &admin())
        .await
        .expect_err("missing");
    assert!(err.to_string().contains("partner"), "{err}");
    let err = env
        .insert("account", ghost, &user)
        .await
        .expect_err("missing");
    assert!(
        matches!(&err, DbError::Validation(e) if e == &vec![ValidationError::Hidden]),
        "{err}"
    );
    assert!(!err.to_string().contains("partner"));

    // A unique collision on a hidden field (only a hook can set it): generic for the User.
    env.insert("account", doc(&[("title", text("dup"))]), &admin())
        .await
        .expect("first");
    let err = env
        .insert("account", doc(&[("title", text("dup"))]), &admin())
        .await
        .expect_err("duplicate");
    assert!(err.to_string().contains("internal_ref"), "{err}");
    let err = env
        .insert("account", doc(&[("title", text("dup"))]), &user)
        .await
        .expect_err("duplicate");
    assert!(matches!(err, DbError::UniqueViolation { .. }));
    let shown = err.to_string();
    assert!(
        !shown.contains("internal_ref") && !shown.contains("same"),
        "{shown}"
    );

    // A stored hidden value that is not valid any more (the column was changed behind the
    // framework's back, or an option was removed later): the User updating another field gets a
    // generic failure, never the field or the value.
    let stored = env.full_account_ref("REF-2").await;
    env.db
        .exec(&format!(
            "UPDATE doc_account SET grade = 'zzz' WHERE name = '{}'",
            stored.name
        ))
        .await;
    let current = env
        .get("account", &stored.name, &sys())
        .await
        .expect("found");
    let err = env
        .update(
            "account",
            &current,
            &changes(&[("region", text("south"))]),
            &user,
        )
        .await
        .expect_err("stored value invalid");
    match &err {
        DbError::Validation(errors) => assert_eq!(errors, &vec![ValidationError::Hidden]),
        other => panic!("{other:?}"),
    }
    assert!(!err.to_string().contains("grade") && !err.to_string().contains("zzz"));
    // The Administrator sees what is wrong.
    let err = env
        .update(
            "account",
            &current,
            &changes(&[("region", text("south"))]),
            &admin(),
        )
        .await
        .expect_err("invalid");
    assert!(err.to_string().contains("grade"), "{err}");
}

// ----- Submit, cancel, amend and transitions ---------------------------------------------

#[tokio::test]
async fn lifecycle_calls_return_redacted_documents_and_amend_keeps_hidden_values() {
    let env = env().await;
    let manager = user_with("m", "Manager");
    let clerk = user("c", &["Clerk"]);
    let created = env
        .insert(
            "invoice",
            doc(&[("title", text("inv")), ("internal_note", text("secret"))]),
            &manager,
        )
        .await
        .expect("insert");
    assert_eq!(created.values["internal_note"], text("secret"));
    let mut client = env.db.pool.get().await.unwrap();
    let submitted = submit(
        &mut **client,
        &env.reg,
        "invoice",
        &created.name,
        created.version,
        &clerk,
    )
    .await;
    // The Manager has no submit right: use the clerk, who cannot read the note.
    let submitted = match submitted {
        Ok(doc) => doc,
        Err(error) => panic!("{error}"),
    };
    assert!(!submitted.values.contains_key("internal_note"));
    let cancelled = cancel(
        &mut **client,
        &env.reg,
        "invoice",
        &created.name,
        submitted.version,
        &clerk,
    )
    .await
    .expect("cancel");
    assert!(!cancelled.values.contains_key("internal_note"));
    let amended = amend(
        &mut **client,
        &env.reg,
        "invoice",
        &created.name,
        cancelled.version,
        &clerk,
    )
    .await
    .expect("amend");
    assert!(!amended.values.contains_key("internal_note"));
    // The copy is not caller input: it keeps the hidden value.
    let stored = get(&mut **client, &env.reg, "invoice", &amended.name, &admin())
        .await
        .expect("get")
        .expect("found");
    assert_eq!(stored.values["internal_note"], text("secret"));
}

#[tokio::test]
async fn a_transition_returns_a_redacted_document_and_takes_no_field_input() {
    let env = env().await;
    let manager = user_with("m", "Manager");
    let clerk = user("c", &["Clerk"]);
    let created = env
        .insert(
            "ticket",
            doc(&[("title", text("t")), ("secret_note", text("hush"))]),
            &manager,
        )
        .await
        .expect("insert");
    let mut client = env.db.pool.get().await.unwrap();
    let done = transition(
        &mut **client,
        &env.reg,
        "ticket",
        &created.name,
        "Finish",
        created.version,
        &clerk,
    )
    .await
    .expect("transition");
    assert_eq!(done.workflow_state.as_deref(), Some("Done"));
    assert!(!done.values.contains_key("secret_note"));
    let stored = get(&mut **client, &env.reg, "ticket", &created.name, &admin())
        .await
        .expect("get")
        .expect("found");
    assert_eq!(stored.values["secret_note"], text("hush"));
}
