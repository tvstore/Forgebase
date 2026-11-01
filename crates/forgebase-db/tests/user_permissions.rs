//! User permissions (M10.3, ADR 0025) against a real PostgreSQL: a user restricted to some
//! documents of a DocType sees, on every path, only those documents and the documents whose
//! Links to it are empty or allowed; a hidden document is not found (and not locked); a Link to a
//! hidden target is a missing one; and the management functions that the CLI and the API call.
//!
//! The DocTypes: `customer_group` (the restricted target), `customer` (two Links to it),
//! `sales_order` (submittable: a Link to the customer and to the group, a table whose rows Link
//! to the group, and a Link at field level 1) and `ticket` (a workflow with a Link to the group).

mod common;

use std::collections::BTreeMap;
use std::time::Duration;

use common::{TestDb, registry};
use forgebase_core::document::{Changes, Document, Row, ValidationError};
use forgebase_core::meta::Registry;
use forgebase_core::permission::{
    ADMINISTRATOR, Caller, MAX_USER_PERMISSIONS, Right, UserRestrictions,
};
use forgebase_core::value::Value;
use forgebase_db::auth::{self, AuthError};
use forgebase_db::user_permissions::{
    add_user_permission, list_user_permissions, remove_user_permission,
};
use forgebase_db::{
    DbError, Export, ExportQuery, Filter, FilterOp, ImportMode, ImportRow, ListQuery, Sort,
    StoredDocument, SummaryQuery, amend, begin_export, cancel, delete, get, import, insert,
    link_titles, list, submit, summary, transition, update,
};

const CUSTOMER_GROUP: &str = r#"
name = "customer_group"
label = "Customer Group"
naming = { field = "title" }
title_field = "title"
[[fields]]
name = "title"
type = "data"
required = true
unique = true
[[permissions]]
role = "Sales"
read = true
write = true
create = true
delete = true
export = true
import = true
[[permissions]]
role = "Mine"
read = true
write = true
"#;

const CUSTOMER: &str = r#"
name = "customer"
label = "Customer"
naming = { field = "title" }
title_field = "title"
[[fields]]
name = "title"
type = "data"
required = true
unique = true
[[fields]]
name = "customer_group"
type = "link"
target = "customer_group"
[[fields]]
name = "billing_group"
type = "link"
target = "customer_group"
[[permissions]]
role = "Sales"
read = true
write = true
create = true
delete = true
export = true
import = true
[[permissions]]
role = "Mine"
read = true
write = true
create = true
delete = true
if_owner = true
"#;

const SALES_ORDER: &str = r#"
name = "sales_order"
label = "Sales Order"
submittable = true
[[fields]]
name = "title"
type = "data"
[[fields]]
name = "customer"
type = "link"
target = "customer"
[[fields]]
name = "customer_group"
type = "link"
target = "customer_group"
[[fields]]
name = "partner_group"
type = "link"
target = "customer_group"
perm_level = 1
[[fields]]
name = "lines"
type = "table"
target = "order_line"
[[permissions]]
role = "Sales"
read = true
write = true
create = true
delete = true
submit = true
cancel = true
amend = true
export = true
[[permissions]]
role = "Mine"
read = true
write = true
create = true
delete = true
if_owner = true
[[permissions]]
role = "Manager"
level = 1
read = true
write = true
"#;

const ORDER_LINE: &str = r#"
name = "order_line"
label = "Order Line"
is_child = true
[[fields]]
name = "qty"
type = "int"
[[fields]]
name = "line_group"
type = "link"
target = "customer_group"
"#;

const TICKET: &str = r#"
name = "ticket"
label = "Ticket"
[[fields]]
name = "title"
type = "data"
[[fields]]
name = "customer_group"
type = "link"
target = "customer_group"
[[permissions]]
role = "Sales"
read = true
write = true
create = true
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
roles = ["Sales"]
"#;

fn reg() -> Registry {
    registry(&[CUSTOMER_GROUP, CUSTOMER, SALES_ORDER, ORDER_LINE, TICKET])
}

struct Env {
    db: TestDb,
    reg: Registry,
}

async fn env() -> Env {
    let reg = reg();
    let db = TestDb::migrated().await;
    db.sync(&reg).await.expect("sync");
    Env { db, reg }
}

fn roles(roles: &[&str]) -> Vec<String> {
    roles.iter().map(|r| (*r).to_owned()).collect()
}

fn restrictions(pairs: &[(&str, &str)]) -> UserRestrictions {
    UserRestrictions::from_pairs(pairs.iter().copied())
}

/// Ann: a salesperson restricted to the customer group `Retail`.
fn ann() -> Caller {
    Caller::user("ann", roles(&["Sales"]))
        .with_restrictions(restrictions(&[("customer_group", "Retail")]))
}

/// Pat: the same role, with no restriction.
fn pat() -> Caller {
    Caller::user("pat", roles(&["Sales"]))
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

fn change(pairs: &[(&str, Value)]) -> Changes {
    Changes {
        values: pairs
            .iter()
            .map(|(k, v)| ((*k).to_owned(), v.clone()))
            .collect(),
        tables: BTreeMap::new(),
    }
}

fn line(group: Option<&str>) -> Row {
    BTreeMap::from([
        ("qty".to_owned(), Value::Int(1)),
        ("line_group".to_owned(), group.map_or(Value::Null, text)),
    ])
}

fn order_doc(title: &str, group: Option<&str>, lines: Vec<Row>) -> Document {
    let mut d = doc(&[
        ("title", text(title)),
        ("customer_group", group.map_or(Value::Null, text)),
    ]);
    d.tables.insert("lines".to_owned(), lines);
    d
}

impl Env {
    /// Inserts a document as the system and returns it.
    async fn put(&self, doctype: &str, document: Document) -> StoredDocument {
        let mut client = self.db.pool.get().await.unwrap();
        insert(&mut **client, &self.reg, doctype, document, &sys())
            .await
            .unwrap_or_else(|e| panic!("insert {doctype}: {e}"))
    }

    async fn group(&self, name: &str) {
        self.put("customer_group", doc(&[("title", text(name))]))
            .await;
    }

    async fn customer(&self, name: &str, group: Option<&str>, billing: Option<&str>) {
        self.put(
            "customer",
            doc(&[
                ("title", text(name)),
                ("customer_group", group.map_or(Value::Null, text)),
                ("billing_group", billing.map_or(Value::Null, text)),
            ]),
        )
        .await;
    }

    async fn order(&self, title: &str, group: Option<&str>, lines: Vec<Row>) -> StoredDocument {
        self.put("sales_order", order_doc(title, group, lines))
            .await
    }

    /// The names a caller lists, in order.
    async fn names(&self, doctype: &str, who: &Caller) -> Vec<String> {
        let client = self.db.pool.get().await.unwrap();
        let found = list(
            &**client,
            &self.reg,
            doctype,
            who,
            &ListQuery {
                limit: Some(500),
                ..ListQuery::default()
            },
        )
        .await
        .unwrap();
        let mut labels: Vec<String> = found.iter().map(label).collect();
        // The random names of the orders would make the order of a list vary: sorted labels.
        labels.sort();
        labels
    }

    /// The name of the sales order with this title (its names are random).
    async fn order_name(&self, title: &str) -> String {
        let client = self.db.pool.get().await.unwrap();
        let query = ListQuery {
            filters: vec![Filter {
                field: "title".to_owned(),
                op: FilterOp::Eq(text(title)),
            }],
            ..ListQuery::default()
        };
        let found = list(&**client, &self.reg, "sales_order", &sys(), &query)
            .await
            .unwrap();
        assert_eq!(found.len(), 1, "one order titled {title}");
        found[0].name.clone()
    }

    /// `get` of a document; a sales order is given by its title.
    async fn get(&self, doctype: &str, name: &str, who: &Caller) -> Option<StoredDocument> {
        let name = if doctype == "sales_order" {
            self.order_name(name).await
        } else {
            name.to_owned()
        };
        let mut client = self.db.pool.get().await.unwrap();
        get(&mut **client, &self.reg, doctype, &name, who)
            .await
            .unwrap()
    }

    /// The names an export gives.
    async fn exported(&self, doctype: &str, who: &Caller) -> Result<Vec<String>, DbError> {
        let mut client = self.db.pool.get().await.unwrap();
        let tx = begin_export(&mut client).await?;
        let query = ExportQuery {
            doctype,
            filters: &[],
            sort: &[],
            limit: 1000,
            with_tables: true,
        };
        let mut export = Export::start(&tx, &self.reg, who, &query).await?;
        assert_eq!(
            export.count(),
            u64::try_from(self.names(doctype, who).await.len()).unwrap(),
            "the count of the export is the count of the list"
        );
        let mut names = Vec::new();
        while let Some(page) = export.next_page(&tx, &self.reg).await? {
            names.extend(page.iter().map(label));
        }
        names.sort();
        Ok(names)
    }
}

/// What a test calls a document: its title, or its name.
fn label(document: &StoredDocument) -> String {
    match document.values.get("title") {
        Some(Value::Text(title)) => title.clone(),
        _ => document.name.clone(),
    }
}

fn strs(items: &[&str]) -> Vec<String> {
    items.iter().map(|s| (*s).to_owned()).collect()
}

/// What `LinkNotFound` says: `field:target` and the name, also for a link in a row.
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

fn is_not_found(error: &DbError) -> bool {
    matches!(error, DbError::NotFound { .. })
}

fn is_denied(error: &DbError, right: Right) -> bool {
    matches!(error, DbError::PermissionDenied { right: r, .. } if *r == right)
}

/// The data of most tests: two groups, five customers and four orders.
///
/// Ann (restricted to `Retail`) sees: the group `Retail`; the customers `c_retail`, `c_none` and
/// `c_billing_retail`; every order whose own `customer_group` is empty or `Retail`.
async fn seed(env: &Env) {
    env.group("Retail").await;
    env.group("Wholesale").await;
    env.customer("c_retail", Some("Retail"), None).await;
    env.customer("c_wholesale", Some("Wholesale"), None).await;
    env.customer("c_none", None, None).await;
    env.customer("c_both", Some("Retail"), Some("Wholesale"))
        .await;
    env.customer("c_billing_retail", None, Some("Retail")).await;
    env.order("o_retail", Some("Retail"), vec![]).await;
    env.order("o_wholesale", Some("Wholesale"), vec![]).await;
    env.order("o_none", None, vec![]).await;
}

// ---- reads -------------------------------------------------------------------------------------

#[tokio::test]
async fn every_read_shows_only_what_the_restriction_allows() {
    let env = env().await;
    seed(&env).await;
    let (ann, pat) = (ann(), pat());

    // The restricted DocType itself: only the allowed name.
    assert_eq!(env.names("customer_group", &ann).await, strs(&["Retail"]));
    assert_eq!(
        env.names("customer_group", &pat).await,
        strs(&["Retail", "Wholesale"])
    );
    // A Link to the target: empty or allowed (both Links must hold, see the next test).
    assert_eq!(
        env.names("customer", &ann).await,
        strs(&["c_billing_retail", "c_none", "c_retail"])
    );
    assert_eq!(
        env.names("sales_order", &ann).await,
        strs(&["o_none", "o_retail"])
    );
    assert_eq!(env.names("sales_order", &pat).await.len(), 3);

    // get: the same.
    for (doctype, name, visible) in [
        ("customer_group", "Retail", true),
        ("customer_group", "Wholesale", false),
        ("customer", "c_retail", true),
        ("customer", "c_wholesale", false),
        ("customer", "c_none", true),
        ("sales_order", "o_wholesale", false),
        ("sales_order", "o_none", true),
        ("customer", "no such customer", false),
    ] {
        assert_eq!(
            env.get(doctype, name, &ann).await.is_some(),
            visible,
            "{doctype} {name}"
        );
    }

    // Filters, sorts and pages cannot reach a hidden row either.
    let client = env.db.pool.get().await.unwrap();
    let query = ListQuery {
        filters: vec![Filter {
            field: "customer_group".to_owned(),
            op: FilterOp::Eq(text("Wholesale")),
        }],
        ..ListQuery::default()
    };
    let rows = list(&**client, &env.reg, "customer", &ann, &query)
        .await
        .unwrap();
    assert!(
        rows.is_empty(),
        "a filter on the hidden group finds nothing"
    );
    let query = ListQuery {
        sort: vec![Sort::desc("title")],
        limit: Some(1),
        offset: Some(1),
        ..ListQuery::default()
    };
    let rows = list(&**client, &env.reg, "customer", &ann, &query)
        .await
        .unwrap();
    assert_eq!(rows[0].name, "c_none", "paging counts only visible rows");
    drop(client);

    // The summary counts only visible rows.
    let client = env.db.pool.get().await.unwrap();
    let summary_of = |who: Caller| {
        let reg = env.reg.clone();
        let client = &client;
        async move {
            let query = SummaryQuery {
                filters: vec![],
                group_by: "customer_group".to_owned(),
                aggregates: vec![],
            };
            summary(&***client, &reg, "customer", &who, &query)
                .await
                .unwrap()
                .groups
                .into_iter()
                .map(|g| (g.key, g.count))
                .collect::<Vec<_>>()
        }
    };
    assert_eq!(
        summary_of(ann.clone()).await,
        vec![(Value::Null, 2), (text("Retail"), 1)]
    );
    let all: i64 = summary_of(pat.clone()).await.iter().map(|g| g.1).sum();
    assert_eq!(all, 5);
    drop(client);

    // Export (and its count): the same documents as the list.
    assert_eq!(
        env.exported("customer_group", &ann).await.unwrap(),
        strs(&["Retail"])
    );
    assert_eq!(
        env.exported("customer", &ann).await.unwrap(),
        strs(&["c_billing_retail", "c_none", "c_retail"])
    );
    assert_eq!(
        env.exported("sales_order", &ann).await.unwrap(),
        strs(&["o_none", "o_retail"])
    );
    assert_eq!(env.exported("customer", &pat).await.unwrap().len(), 5);
}

#[tokio::test]
async fn a_document_with_two_links_to_the_target_needs_both_allowed_or_empty() {
    let env = env().await;
    seed(&env).await;
    let ann = ann();
    for (name, visible) in [
        ("c_retail", true),         // group allowed, billing empty
        ("c_both", false),          // group allowed, billing hidden
        ("c_wholesale", false),     // group hidden, billing empty
        ("c_none", true),           // both empty
        ("c_billing_retail", true), // group empty, billing allowed
    ] {
        assert_eq!(
            env.get("customer", name, &ann).await.is_some(),
            visible,
            "{name}"
        );
    }
    // More than one allowed name: allowed means any of them, per Link.
    let two = Caller::user("two", roles(&["Sales"])).with_restrictions(restrictions(&[
        ("customer_group", "Retail"),
        ("customer_group", "Wholesale"),
    ]));
    assert_eq!(env.names("customer", &two).await.len(), 5);
}

#[tokio::test]
async fn restrictions_on_two_targets_all_apply_and_an_unrelated_one_does_nothing() {
    let env = env().await;
    seed(&env).await;
    // Restricted on the group and on the customer: an order must have an allowed group, and the
    // customers are limited by name as well as by their group.
    let who = Caller::user("two", roles(&["Sales"])).with_restrictions(restrictions(&[
        ("customer_group", "Retail"),
        ("customer", "c_retail"),
    ]));
    assert_eq!(env.names("customer", &who).await, strs(&["c_retail"]));
    // A restriction on a DocType nothing links to, or that does not exist, changes nothing.
    let other = Caller::user("other", roles(&["Sales"]))
        .with_restrictions(restrictions(&[("ticket", "x"), ("ghost_doctype", "y")]));
    assert_eq!(env.names("customer", &other).await.len(), 5);
    assert_eq!(env.names("customer_group", &other).await.len(), 2);
    // A name that is no document any more restricts the user to nothing visible.
    let gone = Caller::user("gone", roles(&["Sales"]))
        .with_restrictions(restrictions(&[("customer_group", "Deleted")]));
    assert!(env.names("customer_group", &gone).await.is_empty());
    assert_eq!(
        env.names("customer", &gone).await,
        strs(&["c_none"]),
        "only documents without a group are left"
    );
}

#[tokio::test]
async fn names_with_quotes_commas_braces_and_backslashes_are_bound_not_interpreted() {
    let env = env().await;
    let odd = [
        r#"Odd "name", {x}\ NULL"#,
        "NULL",
        "plain",
        "ünï,cödé",
        "has space ",
    ];
    for name in odd {
        env.group(name).await;
    }
    env.group("Other").await;
    let who = Caller::user("odd", roles(&["Sales"])).with_restrictions(
        UserRestrictions::from_pairs(odd.iter().map(|n| ("customer_group", *n))),
    );
    let mut seen = env.names("customer_group", &who).await;
    seen.sort();
    let mut want: Vec<String> = odd.iter().map(|s| (*s).to_owned()).collect();
    want.sort();
    assert_eq!(seen, want);
    // A name that merely looks like an array element is not matched.
    let sneaky = Caller::user("sneaky", roles(&["Sales"]))
        .with_restrictions(restrictions(&[("customer_group", r#"Other","plain"#)]));
    assert!(env.names("customer_group", &sneaky).await.is_empty());
}

// ---- locking reads -----------------------------------------------------------------------------

/// What each locking operation of `who` says about a document: `NotFound` for a hidden one,
/// before the version is compared.
async fn hidden_results(
    env: &Env,
    who: &Caller,
    order: &StoredDocument,
) -> Vec<(&'static str, String)> {
    let mut client = env.db.pool.get().await.unwrap();
    let client = &mut **client;
    let name = order.name.as_str();
    let stale = order.version + 41;
    let mut out = Vec::new();
    let mut note = |op: &'static str, result: Result<(), DbError>| {
        out.push((op, result.err().map_or("ok".to_owned(), |e| e.to_string())));
    };
    note(
        "update",
        update(
            client,
            &env.reg,
            "sales_order",
            name,
            stale,
            &change(&[("title", text("x"))]),
            who,
        )
        .await
        .map(drop),
    );
    note(
        "delete",
        delete(client, &env.reg, "sales_order", name, stale, who).await,
    );
    note(
        "submit",
        submit(client, &env.reg, "sales_order", name, stale, who)
            .await
            .map(drop),
    );
    note(
        "cancel",
        cancel(client, &env.reg, "sales_order", name, stale, who)
            .await
            .map(drop),
    );
    note(
        "amend",
        amend(client, &env.reg, "sales_order", name, stale, who)
            .await
            .map(drop),
    );
    out
}

#[tokio::test]
async fn a_hidden_document_is_not_found_by_every_locking_operation_whatever_the_version() {
    let env = env().await;
    seed(&env).await;
    let hidden = env.get("sales_order", "o_wholesale", &pat()).await.unwrap();
    let missing = StoredDocument {
        name: "o_missing".to_owned(),
        ..hidden.clone()
    };
    let for_hidden = hidden_results(&env, &ann(), &hidden).await;
    let for_missing = hidden_results(&env, &ann(), &missing).await;
    // Every operation: not found, with the very message of a name that does not exist, apart
    // from the name itself.
    for ((op, a), (_, b)) in for_hidden.iter().zip(&for_missing) {
        assert!(a.contains("not found"), "{op}: {a}");
        assert_eq!(
            a.replace(hidden.name.as_str(), "N"),
            b.replace("o_missing", "N"),
            "{op}"
        );
    }
    // A visible document under the same stale version is a conflict instead.
    let visible = env.get("sales_order", "o_retail", &pat()).await.unwrap();
    for (op, message) in hidden_results(&env, &ann(), &visible).await {
        assert!(message.contains("version"), "{op}: {message}");
    }
    // Nothing changed.
    let still = env.get("sales_order", "o_wholesale", &pat()).await.unwrap();
    assert_eq!(still.version, hidden.version);
    assert_eq!(still.values["title"], text("o_wholesale"));
}

#[tokio::test]
async fn cancel_and_amend_of_a_hidden_document_are_not_found_too() {
    let env = env().await;
    seed(&env).await;
    let mut client = env.db.pool.get().await.unwrap();
    let client = &mut **client;
    // A submitted and a cancelled document in the hidden group, made by the system.
    let submitted = env.order("o_sub", Some("Wholesale"), vec![]).await;
    let v = submit(
        client,
        &env.reg,
        "sales_order",
        &submitted.name,
        submitted.version,
        &sys(),
    )
    .await
    .unwrap()
    .version;
    let error = cancel(client, &env.reg, "sales_order", &submitted.name, v, &ann())
        .await
        .unwrap_err();
    assert!(is_not_found(&error), "{error}");
    let v = cancel(client, &env.reg, "sales_order", &submitted.name, v, &sys())
        .await
        .unwrap()
        .version;
    let error = amend(client, &env.reg, "sales_order", &submitted.name, v, &ann())
        .await
        .unwrap_err();
    assert!(is_not_found(&error), "{error}");
    // A visible one goes through the whole cycle for Ann.
    let mine = env.order("o_mine", Some("Retail"), vec![]).await;
    let v = submit(
        client,
        &env.reg,
        "sales_order",
        &mine.name,
        mine.version,
        &ann(),
    )
    .await
    .unwrap()
    .version;
    let v = cancel(client, &env.reg, "sales_order", &mine.name, v, &ann())
        .await
        .unwrap()
        .version;
    let amended = amend(client, &env.reg, "sales_order", &mine.name, v, &ann())
        .await
        .unwrap();
    assert_eq!(amended.values["customer_group"], text("Retail"));
}

#[tokio::test]
async fn a_hidden_document_is_not_found_by_a_transition_and_a_visible_one_moves() {
    let env = env().await;
    env.group("Retail").await;
    env.group("Wholesale").await;
    let make = |title: &'static str, group: &'static str| {
        env.put(
            "ticket",
            doc(&[("title", text(title)), ("customer_group", text(group))]),
        )
    };
    let hidden = make("t_hidden", "Wholesale").await;
    let visible = make("t_visible", "Retail").await;
    let mut client = env.db.pool.get().await.unwrap();
    let client = &mut **client;
    let error = transition(
        client,
        &env.reg,
        "ticket",
        &hidden.name,
        "Finish",
        hidden.version,
        &ann(),
    )
    .await
    .unwrap_err();
    assert!(is_not_found(&error), "{error}");
    let error = transition(
        client,
        &env.reg,
        "ticket",
        &hidden.name,
        "Finish",
        99,
        &ann(),
    )
    .await
    .unwrap_err();
    assert!(
        is_not_found(&error),
        "a stale version on a hidden document: {error}"
    );
    let error =
        forgebase_db::available_transitions(client, &env.reg, "ticket", &hidden.name, &ann())
            .await
            .unwrap_err();
    assert!(is_not_found(&error), "{error}");
    let done = transition(
        client,
        &env.reg,
        "ticket",
        &visible.name,
        "Finish",
        visible.version,
        &ann(),
    )
    .await
    .unwrap();
    assert_eq!(done.workflow_state.as_deref(), Some("Done"));
    // The hidden ticket is still open.
    let still = get(client, &env.reg, "ticket", &hidden.name, &pat())
        .await
        .unwrap()
        .unwrap();
    assert_eq!(still.workflow_state.as_deref(), Some("Open"));
}

/// Holds `FOR UPDATE` on a row in another connection.
struct RowLock {
    client: deadpool_postgres::Object,
}

impl RowLock {
    async fn take(env: &Env, table: &str, name: &str) -> Self {
        let client = env.db.pool.get().await.unwrap();
        client.batch_execute("BEGIN").await.unwrap();
        client
            .execute(
                &format!("SELECT 1 FROM {table} WHERE name = $1 FOR UPDATE"),
                &[&name],
            )
            .await
            .unwrap();
        Self { client }
    }

    async fn release(self) {
        self.client.batch_execute("ROLLBACK").await.unwrap();
    }
}

#[tokio::test]
async fn a_hidden_row_is_not_even_locked_so_it_never_waits_for_a_lock_on_it() {
    let env = env().await;
    seed(&env).await;
    let target = env.order_name("o_wholesale").await;
    let lock = RowLock::take(&env, "doc_sales_order", &target).await;
    // The restricted caller gets NotFound at once, for every operation that takes a lock: the
    // row is outside the statement, so it is neither locked nor waited for.
    let short = Duration::from_millis(1500);
    let mut client = env.db.pool.get().await.unwrap();
    let client = &mut **client;
    let changes = change(&[("title", text("x"))]);
    let who = ann();
    let results = tokio::time::timeout(short, async {
        vec![
            update(client, &env.reg, "sales_order", &target, 1, &changes, &who)
                .await
                .map(drop),
            delete(client, &env.reg, "sales_order", &target, 1, &who).await,
            submit(client, &env.reg, "sales_order", &target, 1, &who)
                .await
                .map(drop),
        ]
    })
    .await
    .expect("did not wait for the lock");
    for result in results {
        assert!(is_not_found(&result.unwrap_err()));
    }

    // The control: an unrestricted caller does wait for the same lock, so the lock is real.
    let pool = env.db.pool.clone();
    let (reg, name) = (env.reg.clone(), target.clone());
    let waiting = tokio::spawn(async move {
        let mut client = pool.get().await.unwrap();
        update(
            &mut **client,
            &reg,
            "sales_order",
            &name,
            1,
            &change(&[("title", text("x"))]),
            &pat(),
        )
        .await
        .map(drop)
    });
    tokio::time::sleep(Duration::from_millis(600)).await;
    assert!(
        !waiting.is_finished(),
        "the unrestricted update waits for the lock"
    );
    lock.release().await;
    waiting
        .await
        .unwrap()
        .expect("the update goes through once the lock is gone");
}

// ---- link resolution ---------------------------------------------------------------------------

#[tokio::test]
async fn a_link_to_a_hidden_target_is_a_missing_one_in_a_field_and_in_a_row() {
    let env = env().await;
    seed(&env).await;
    let mut client = env.db.pool.get().await.unwrap();
    let client = &mut **client;
    let ann = ann();

    // A top-level Link to the restricted target, and to a customer that is hidden by its group.
    for (field, hidden, missing) in [
        ("customer_group", "Wholesale", "Nowhere"),
        ("customer", "c_wholesale", "c_nowhere"),
        ("customer", "c_both", "c_nowhere"),
    ] {
        let mut d = order_doc("x", None, vec![]);
        d.values.insert(field.to_owned(), text(hidden));
        let error = insert(client, &env.reg, "sales_order", d, &ann)
            .await
            .unwrap_err();
        let mut d = order_doc("x", None, vec![]);
        d.values.insert(field.to_owned(), text(missing));
        let reference = insert(client, &env.reg, "sales_order", d, &ann)
            .await
            .unwrap_err();
        let (a, b) = (link_not_found(&error), link_not_found(&reference));
        assert_eq!(a.len(), 1);
        assert_eq!(a[0].0, b[0].0, "{field}: the same field and target");
        assert_eq!(
            error.to_string().replace(hidden, "N"),
            reference.to_string().replace(missing, "N"),
            "{field}: the same message"
        );
    }
    // A Link in a row.
    let rows = vec![line(Some("Retail")), line(Some("Wholesale"))];
    let error = insert(
        client,
        &env.reg,
        "sales_order",
        order_doc("x", None, rows),
        &ann,
    )
    .await
    .unwrap_err();
    assert_eq!(
        link_not_found(&error),
        vec![(
            "lines[2].line_group:customer_group".to_owned(),
            "Wholesale".to_owned()
        )]
    );
    // The allowed target, an empty Link and a visible customer all resolve.
    let mut d = order_doc(
        "fine",
        Some("Retail"),
        vec![line(Some("Retail")), line(None)],
    );
    d.values
        .insert("customer".to_owned(), text("c_billing_retail"));
    let ok = insert(client, &env.reg, "sales_order", d, &ann)
        .await
        .unwrap();
    // The same target resolves for the unrestricted user.
    let mut d = order_doc("any", Some("Wholesale"), vec![line(Some("Wholesale"))]);
    d.values.insert("customer".to_owned(), text("c_wholesale"));
    insert(client, &env.reg, "sales_order", d, &pat())
        .await
        .unwrap();

    // update: a changed value is resolved, clearing a Link is fine.
    let error = update(
        client,
        &env.reg,
        "sales_order",
        &ok.name,
        ok.version,
        &change(&[("customer_group", text("Wholesale"))]),
        &ann,
    )
    .await
    .unwrap_err();
    assert_eq!(
        link_not_found(&error),
        vec![(
            "customer_group:customer_group".to_owned(),
            "Wholesale".to_owned()
        )]
    );
    let cleared = update(
        client,
        &env.reg,
        "sales_order",
        &ok.name,
        ok.version,
        &change(&[("customer_group", Value::Null)]),
        &ann,
    )
    .await
    .unwrap();
    assert_eq!(cleared.values["customer_group"], Value::Null);
}

#[tokio::test]
async fn a_stored_value_is_kept_with_full_access_but_a_new_one_is_not_allowed() {
    let env = env().await;
    seed(&env).await;
    // The system made an order that Ann can see, with a hidden Link in a row (a child row does
    // not decide the visibility of its parent).
    let stored = env
        .order(
            "o_rows",
            Some("Retail"),
            vec![line(Some("Wholesale")), line(Some("Retail"))],
        )
        .await;
    let mut client = env.db.pool.get().await.unwrap();
    let client = &mut **client;
    let ann = ann();
    let seen = get(client, &env.reg, "sales_order", &stored.name, &ann)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(
        seen.tables["lines"].len(),
        2,
        "the hidden row Link is part of a visible document"
    );

    // Updating another field keeps it (the table is not changed).
    let updated = update(
        client,
        &env.reg,
        "sales_order",
        &stored.name,
        stored.version,
        &change(&[("title", text("renamed"))]),
        &ann,
    )
    .await
    .unwrap();
    // Re-sending the same rows keeps the stored value, and adding it to a new row is refused.
    let mut keep = Changes::default();
    keep.tables.insert(
        "lines".to_owned(),
        vec![
            line(Some("Wholesale")),
            line(Some("Retail")),
            line(Some("Retail")),
        ],
    );
    let updated = update(
        client,
        &env.reg,
        "sales_order",
        &stored.name,
        updated.version,
        &keep,
        &ann,
    )
    .await
    .unwrap();
    assert_eq!(updated.tables["lines"].len(), 3);
    let mut more = Changes::default();
    more.tables.insert(
        "lines".to_owned(),
        vec![
            line(Some("Wholesale")),
            line(Some("Retail")),
            line(Some("Retail")),
        ]
        .into_iter()
        .chain([line(Some("Wholesale"))])
        .collect(),
    );
    // The value is among the stored values of the same field of the same table: kept.
    update(
        client,
        &env.reg,
        "sales_order",
        &stored.name,
        updated.version,
        &more,
        &ann,
    )
    .await
    .expect("a name already stored in this field is kept");
    // A hidden group that is NOT stored in the table is refused in a row.
    env.group("Third").await;
    let mut fresh = Changes::default();
    fresh
        .tables
        .insert("lines".to_owned(), vec![line(Some("Third"))]);
    let current = get(client, &env.reg, "sales_order", &stored.name, &sys())
        .await
        .unwrap()
        .unwrap();
    let error = update(
        client,
        &env.reg,
        "sales_order",
        &stored.name,
        current.version,
        &fresh,
        &ann,
    )
    .await
    .unwrap_err();
    assert_eq!(
        link_not_found(&error),
        vec![(
            "lines[1].line_group:customer_group".to_owned(),
            "Third".to_owned()
        )]
    );
    // Submitting keeps the stored hidden Link too, and amend (which never keeps) refuses it.
    let current = get(client, &env.reg, "sales_order", &stored.name, &sys())
        .await
        .unwrap()
        .unwrap();
    let submitted = submit(
        client,
        &env.reg,
        "sales_order",
        &stored.name,
        current.version,
        &ann,
    )
    .await
    .expect("submit keeps stored Links");
    let cancelled = cancel(
        client,
        &env.reg,
        "sales_order",
        &stored.name,
        submitted.version,
        &ann,
    )
    .await
    .unwrap();
    let error = amend(
        client,
        &env.reg,
        "sales_order",
        &stored.name,
        cancelled.version,
        &ann,
    )
    .await
    .unwrap_err();
    assert!(!link_not_found(&error).is_empty(), "{error}");
}

#[tokio::test]
async fn no_write_makes_a_document_the_caller_cannot_see_except_through_a_child_row() {
    let env = env().await;
    seed(&env).await;
    let mut client = env.db.pool.get().await.unwrap();
    let client = &mut **client;
    let ann = ann();
    // Whatever Ann inserts or updates, she can read back.
    let cases = [None, Some("Retail")];
    for group in cases {
        let made = insert(
            client,
            &env.reg,
            "sales_order",
            order_doc("w", group, vec![]),
            &ann,
        )
        .await
        .unwrap();
        assert!(
            get(client, &env.reg, "sales_order", &made.name, &ann)
                .await
                .unwrap()
                .is_some()
        );
        let to = match group {
            None => Some("Retail"),
            Some(_) => None,
        };
        let changed = update(
            client,
            &env.reg,
            "sales_order",
            &made.name,
            made.version,
            &change(&[("customer_group", to.map_or(Value::Null, text))]),
            &ann,
        )
        .await
        .unwrap();
        assert!(
            get(client, &env.reg, "sales_order", &changed.name, &ann)
                .await
                .unwrap()
                .is_some()
        );
    }
    // The Link at field level 1 (which Ann cannot read or write) is stored as the system set it,
    // and still decides the visibility.
    let mut with_partner = order_doc("p", None, vec![]);
    with_partner
        .values
        .insert("partner_group".to_owned(), text("Wholesale"));
    env.put("sales_order", with_partner).await;
    assert!(
        !env.names("sales_order", &ann)
            .await
            .contains(&"p".to_owned())
    );
}

// ---- creating in a restricted DocType ----------------------------------------------------------

#[tokio::test]
async fn a_restricted_user_cannot_create_or_amend_a_document_of_the_restricted_doctype() {
    let env = env().await;
    seed(&env).await;
    let mut client = env.db.pool.get().await.unwrap();
    let client = &mut **client;
    // Ann has the create right on `customer_group` (the same role as Pat, who can create).
    let error = insert(
        client,
        &env.reg,
        "customer_group",
        doc(&[("title", text("Retail 2"))]),
        &ann(),
    )
    .await
    .unwrap_err();
    assert!(is_denied(&error, Right::Create), "{error}");
    insert(
        client,
        &env.reg,
        "customer_group",
        doc(&[("title", text("Retail 2"))]),
        &pat(),
    )
    .await
    .unwrap();
    // The same for amend: a restriction on the submittable DocType itself.
    let order = env.order("o_a", None, vec![]).await;
    let v = submit(
        client,
        &env.reg,
        "sales_order",
        &order.name,
        order.version,
        &sys(),
    )
    .await
    .unwrap()
    .version;
    let v = cancel(client, &env.reg, "sales_order", &order.name, v, &sys())
        .await
        .unwrap()
        .version;
    let vic = Caller::user("vic", roles(&["Sales"]))
        .with_restrictions(restrictions(&[("sales_order", order.name.as_str())]));
    let error = amend(client, &env.reg, "sales_order", &order.name, v, &vic)
        .await
        .unwrap_err();
    assert!(is_denied(&error, Right::Amend), "{error}");
    let error = insert(
        client,
        &env.reg,
        "sales_order",
        order_doc("n", None, vec![]),
        &vic,
    )
    .await
    .unwrap_err();
    assert!(is_denied(&error, Right::Create), "{error}");
    // Reading and updating the allowed document is fine, and no other document is visible.
    assert_eq!(env.names("sales_order", &vic).await, strs(&["o_a"]));
}

// ---- titles, import, bypass, combinations ------------------------------------------------------

#[tokio::test]
async fn link_titles_leave_out_hidden_targets() {
    let env = env().await;
    seed(&env).await;
    let mut order = order_doc("t", Some("Retail"), vec![]);
    order.values.insert("customer".to_owned(), text("c_retail"));
    let visible = env.put("sales_order", order).await;
    let mut order = order_doc("h", Some("Wholesale"), vec![]);
    order
        .values
        .insert("customer".to_owned(), text("c_wholesale"));
    let hidden = env.put("sales_order", order).await;
    let docs = vec![visible, hidden];
    let client = env.db.pool.get().await.unwrap();
    let titles = link_titles(&**client, &env.reg, &ann(), "sales_order", &docs)
        .await
        .unwrap();
    assert_eq!(
        titles["customer_group"],
        BTreeMap::from([("Retail".to_owned(), "Retail".to_owned())])
    );
    assert_eq!(
        titles["customer"],
        BTreeMap::from([("c_retail".to_owned(), "c_retail".to_owned())])
    );
    let all = link_titles(&**client, &env.reg, &pat(), "sales_order", &docs)
        .await
        .unwrap();
    assert_eq!(all["customer_group"].len(), 2);
    assert_eq!(all["customer"].len(), 2);
}

#[tokio::test]
async fn an_import_applies_the_rules_to_every_row() {
    let env = env().await;
    seed(&env).await;
    let mut client = env.db.pool.get().await.unwrap();
    let client = &mut **client;
    let row = |n: usize, title: &str, group: &str| ImportRow {
        row: n,
        values: BTreeMap::from([
            ("title".to_owned(), text(title)),
            ("customer_group".to_owned(), text(group)),
        ]),
        ..ImportRow::default()
    };
    let rows = [
        row(1, "i_ok", "Retail"),
        row(2, "i_hidden", "Wholesale"),
        row(3, "i_missing", "Nowhere"),
    ];
    let outcome = import(
        client,
        &env.reg,
        "customer",
        &ann(),
        &rows,
        ImportMode::Insert,
        false,
    )
    .await
    .unwrap();
    assert_eq!((outcome.inserted, outcome.error_count), (1, 2));
    let messages: Vec<String> = outcome.errors.iter().map(|e| e.error.to_string()).collect();
    assert_eq!(
        messages[0].replace("Wholesale", "N"),
        messages[1].replace("Nowhere", "N"),
        "a hidden target is reported as a missing one"
    );
    // An upsert of a hidden document is not found, so it would be an insert that collides (the
    // known limit: a unique name tells that the document exists), and never an update.
    let upsert = [ImportRow {
        row: 1,
        name: Some("c_wholesale".to_owned()),
        values: BTreeMap::from([("title".to_owned(), text("c_wholesale"))]),
        ..ImportRow::default()
    }];
    let outcome = import(
        client,
        &env.reg,
        "customer",
        &ann(),
        &upsert,
        ImportMode::Upsert,
        false,
    )
    .await
    .unwrap();
    assert_eq!(outcome.updated, 0);
    assert_eq!(outcome.error_count, 1);
    // Importing the restricted DocType itself is refused row by row (creating is refused).
    let groups = [ImportRow {
        row: 1,
        values: BTreeMap::from([("title".to_owned(), text("New"))]),
        ..ImportRow::default()
    }];
    let outcome = import(
        client,
        &env.reg,
        "customer_group",
        &ann(),
        &groups,
        ImportMode::Insert,
        false,
    )
    .await;
    match outcome {
        Err(error) => assert!(is_denied(&error, Right::Create), "{error}"),
        Ok(outcome) => {
            assert_eq!((outcome.inserted, outcome.error_count), (0, 1));
            assert!(is_denied(&outcome.errors[0].error, Right::Create));
        }
    }
}

#[tokio::test]
async fn administrator_and_system_see_everything_whatever_rows_exist() {
    let env = env().await;
    seed(&env).await;
    let pairs = restrictions(&[("customer_group", "Retail"), ("customer", "c_retail")]);
    let admin = Caller::user("root", roles(&[ADMINISTRATOR])).with_restrictions(pairs.clone());
    let system = Caller::system("sys").with_restrictions(pairs);
    for who in [&admin, &system] {
        assert_eq!(env.names("customer_group", who).await.len(), 2);
        assert_eq!(env.names("customer", who).await.len(), 5);
        assert_eq!(env.names("sales_order", who).await.len(), 3);
        assert_eq!(env.exported("customer", &admin).await.unwrap().len(), 5);
        assert!(env.get("customer", "c_wholesale", who).await.is_some());
    }
    let mut client = env.db.pool.get().await.unwrap();
    let client = &mut **client;
    insert(
        client,
        &env.reg,
        "customer_group",
        doc(&[("title", text("Admin made"))]),
        &admin,
    )
    .await
    .unwrap();
    let mut d = order_doc("a", Some("Wholesale"), vec![line(Some("Wholesale"))]);
    d.values.insert("customer".to_owned(), text("c_wholesale"));
    insert(client, &env.reg, "sales_order", d, &admin)
        .await
        .unwrap();
}

#[tokio::test]
async fn no_user_permissions_means_no_change() {
    let env = env().await;
    seed(&env).await;
    // The user has restrictions, but not on anything these DocTypes touch: the same as none.
    let plain = Caller::user("plain", roles(&["Sales"]));
    let elsewhere = Caller::user("elsewhere", roles(&["Sales"]))
        .with_restrictions(restrictions(&[("ticket", "t")]));
    for doctype in ["customer_group", "customer", "sales_order"] {
        assert_eq!(
            env.names(doctype, &plain).await,
            env.names(doctype, &elsewhere).await
        );
    }
    assert_eq!(env.names("sales_order", &plain).await.len(), 3);
}

#[tokio::test]
async fn the_restriction_and_if_owner_both_apply() {
    let env = env().await;
    seed(&env).await;
    // Mia owns two customers: one in Retail, one in Wholesale. Bob owns one in Retail.
    let mia = Caller::user("mia", roles(&["Mine"]))
        .with_restrictions(restrictions(&[("customer_group", "Retail")]));
    let bob = Caller::user("bob", roles(&["Mine"]));
    let mut client = env.db.pool.get().await.unwrap();
    let client = &mut **client;
    for (who, title, group) in [
        (&bob, "m_wholesale_bob", "Wholesale"),
        (&bob, "m_retail_bob", "Retail"),
        (&bob, "m_none_bob", ""),
    ] {
        let mut d = doc(&[("title", text(title))]);
        if !group.is_empty() {
            d.values.insert("customer_group".to_owned(), text(group));
        }
        insert(client, &env.reg, "customer", d, who).await.unwrap();
    }
    // Mia (restricted) creates Retail customers of her own; Bob has no restriction.
    insert(
        client,
        &env.reg,
        "customer",
        doc(&[
            ("title", text("m_retail_mia")),
            ("customer_group", text("Retail")),
        ]),
        &mia,
    )
    .await
    .unwrap();
    // `Mine` reads own documents only, so Mia sees her own document, and none of Bob's, even the
    // ones in scope; the group is Mia's group scope on top (she only has an `if_owner` read).
    assert_eq!(env.names("customer", &mia).await, strs(&["m_retail_mia"]));
    assert!(env.get("customer", "m_retail_bob", &mia).await.is_none());
    // Bob sees his three (no restriction).
    assert_eq!(env.names("customer", &bob).await.len(), 3);
    // A document of her own that left the scope is hidden from her: the system moves it.
    let own = get(client, &env.reg, "customer", "m_retail_mia", &sys())
        .await
        .unwrap()
        .unwrap();
    update(
        client,
        &env.reg,
        "customer",
        "m_retail_mia",
        own.version,
        &change(&[("customer_group", text("Wholesale"))]),
        &sys(),
    )
    .await
    .unwrap();
    assert!(env.names("customer", &mia).await.is_empty());
}

#[tokio::test]
async fn a_caller_without_the_read_right_is_denied_before_any_scope() {
    let env = env().await;
    seed(&env).await;
    let nobody = Caller::user("nobody", roles(&["Stranger"]))
        .with_restrictions(restrictions(&[("customer_group", "Retail")]));
    let mut client = env.db.pool.get().await.unwrap();
    let error = get(&mut **client, &env.reg, "customer", "c_retail", &nobody)
        .await
        .unwrap_err();
    assert!(is_denied(&error, Right::Read), "{error}");
    let error = list(
        &**client,
        &env.reg,
        "customer",
        &nobody,
        &ListQuery::default(),
    )
    .await
    .unwrap_err();
    assert!(is_denied(&error, Right::Read), "{error}");
    let error = update(
        &mut **client,
        &env.reg,
        "customer",
        "c_retail",
        1,
        &change(&[("title", text("x"))]),
        &nobody,
    )
    .await
    .unwrap_err();
    assert!(is_denied(&error, Right::Write), "{error}");
}

#[tokio::test]
async fn a_hidden_link_field_still_restricts_and_its_value_is_not_revealed() {
    let env = env().await;
    seed(&env).await;
    let mut hidden = order_doc("lvl_hidden", None, vec![]);
    hidden
        .values
        .insert("partner_group".to_owned(), text("Wholesale"));
    env.put("sales_order", hidden).await;
    let mut allowed = order_doc("lvl_allowed", None, vec![]);
    allowed
        .values
        .insert("partner_group".to_owned(), text("Retail"));
    env.put("sales_order", allowed).await;
    let ann = ann();
    // Ann cannot read the field (level 1) but it decides: the order of Wholesale is hidden, the
    // order of Retail is shown, without the value.
    let names = env.names("sales_order", &ann).await;
    assert!(names.contains(&"lvl_allowed".to_owned()));
    assert!(!names.contains(&"lvl_hidden".to_owned()));
    let shown = env.get("sales_order", "lvl_allowed", &ann).await.unwrap();
    assert!(!shown.values.contains_key("partner_group"));
    // It is unknown to a filter, as any hidden field.
    let client = env.db.pool.get().await.unwrap();
    let query = ListQuery {
        filters: vec![Filter {
            field: "partner_group".to_owned(),
            op: FilterOp::Eq(text("Retail")),
        }],
        ..ListQuery::default()
    };
    let error = list(&**client, &env.reg, "sales_order", &ann, &query)
        .await
        .unwrap_err();
    assert!(matches!(error, DbError::InvalidQuery(_)), "{error}");
    // A manager with the level reads it, and is restricted all the same.
    let manager = Caller::user("mgr", roles(&["Sales", "Manager"]))
        .with_restrictions(restrictions(&[("customer_group", "Retail")]));
    let shown = env
        .get("sales_order", "lvl_allowed", &manager)
        .await
        .unwrap();
    assert_eq!(shown.values["partner_group"], text("Retail"));
    assert!(
        env.get("sales_order", "lvl_hidden", &manager)
            .await
            .is_none()
    );
}

// ---- loading and managing ----------------------------------------------------------------------

async fn user_with(env: &Env, name: &str, roles: &[&str]) {
    let mut client = env.db.pool.get().await.unwrap();
    auth::create_user(&mut **client, name, "correct horse battery", roles)
        .await
        .unwrap();
}

async fn restrictions_of(env: &Env, name: &str) -> Caller {
    let mut client = env.db.pool.get().await.unwrap();
    let session = auth::login(&mut **client, name, "correct horse battery")
        .await
        .unwrap();
    let user = auth::validate_session(&**client, &session.token)
        .await
        .unwrap()
        .unwrap();
    user.caller()
}

#[tokio::test]
async fn the_permissions_are_loaded_with_every_authenticated_request() {
    let env = env().await;
    seed(&env).await;
    user_with(&env, "ann", &["Sales"]).await;
    let mut client = env.db.pool.get().await.unwrap();
    let client = &mut **client;
    assert_eq!(
        restrictions_of(&env, "ann").await,
        Caller::user("ann", roles(&["Sales"]))
    );
    assert!(
        add_user_permission(client, &env.reg, "ann", "customer_group", "Retail")
            .await
            .unwrap()
    );
    assert!(
        add_user_permission(client, &env.reg, "ann", "customer_group", "Wholesale")
            .await
            .unwrap()
    );
    assert!(
        add_user_permission(client, &env.reg, "ann", "customer", "c_none")
            .await
            .unwrap()
    );
    let caller = restrictions_of(&env, "ann").await;
    assert_eq!(
        caller,
        Caller::user("ann", roles(&["Sales"])).with_restrictions(restrictions(&[
            ("customer_group", "Retail"),
            ("customer_group", "Wholesale"),
            ("customer", "c_none"),
        ]))
    );
    // It applies at once: the next call as that caller sees the rows.
    assert_eq!(env.names("customer", &caller).await, strs(&["c_none"]));
    assert!(
        remove_user_permission(client, "ann", "customer", "c_none")
            .await
            .unwrap()
    );
    assert!(
        !remove_user_permission(client, "ann", "customer", "c_none")
            .await
            .unwrap()
    );
    let caller = restrictions_of(&env, "ann").await;
    assert_eq!(env.names("customer", &caller).await.len(), 5);
}

#[tokio::test]
async fn adding_checks_the_user_the_doctype_the_value_and_the_document() {
    let env = env().await;
    seed(&env).await;
    user_with(&env, "ann", &["Sales"]).await;
    let mut client = env.db.pool.get().await.unwrap();
    let client = &mut **client;
    let invalid = |r: Result<bool, AuthError>| match r {
        Err(AuthError::InvalidUserPermission(m)) => m,
        other => panic!("expected an invalid user permission, got {other:?}"),
    };
    assert!(matches!(
        add_user_permission(client, &env.reg, "nobody", "customer_group", "Retail").await,
        Err(AuthError::UserNotFound(_))
    ));
    assert!(matches!(
        add_user_permission(client, &env.reg, "bad name", "customer_group", "Retail").await,
        Err(AuthError::InvalidUsername(_))
    ));
    let m = invalid(add_user_permission(client, &env.reg, "ann", "ghost", "x").await);
    assert!(m.contains("unknown DocType"), "{m}");
    let m = invalid(add_user_permission(client, &env.reg, "ann", "order_line", "x").await);
    assert!(m.contains("child DocType"), "{m}");
    let m = invalid(add_user_permission(client, &env.reg, "ann", "customer_group", "").await);
    assert!(m.contains("not empty"), "{m}");
    let m = invalid(add_user_permission(client, &env.reg, "ann", "customer_group", "a\0b").await);
    assert!(m.contains("NUL"), "{m}");
    let m =
        invalid(add_user_permission(client, &env.reg, "ann", "customer_group", "Nowhere").await);
    assert!(
        m.contains("no `customer_group` document named `Nowhere`"),
        "{m}"
    );
    // Nothing was written, and adding a row twice is not an error.
    assert!(
        list_user_permissions(&*client, None, None)
            .await
            .unwrap()
            .is_empty()
    );
    assert!(
        add_user_permission(client, &env.reg, "ann", "customer_group", "Retail")
            .await
            .unwrap()
    );
    assert!(
        !add_user_permission(client, &env.reg, "ann", "customer_group", "Retail")
            .await
            .unwrap()
    );
    assert_eq!(
        list_user_permissions(&*client, None, None)
            .await
            .unwrap()
            .len(),
        1
    );
    // Removing a row of an unknown user is an error; of a document that is gone it works.
    assert!(matches!(
        remove_user_permission(client, "nobody", "customer_group", "Retail").await,
        Err(AuthError::UserNotFound(_))
    ));
    // A row of a document that is gone can still be removed.
    env.group("Temp").await;
    assert!(
        add_user_permission(client, &env.reg, "ann", "customer_group", "Temp")
            .await
            .unwrap()
    );
    let temp = get(client, &env.reg, "customer_group", "Temp", &sys())
        .await
        .unwrap()
        .unwrap();
    delete(
        client,
        &env.reg,
        "customer_group",
        "Temp",
        temp.version,
        &sys(),
    )
    .await
    .unwrap();
    assert!(
        remove_user_permission(client, "ann", "customer_group", "Temp")
            .await
            .unwrap()
    );
}

#[tokio::test]
async fn the_list_is_sorted_and_filtered() {
    let env = env().await;
    seed(&env).await;
    user_with(&env, "zoe", &["Sales"]).await;
    user_with(&env, "ann", &["Sales"]).await;
    let mut client = env.db.pool.get().await.unwrap();
    let client = &mut **client;
    for (user, doctype, value) in [
        ("zoe", "customer_group", "Retail"),
        ("ann", "customer", "c_none"),
        ("ann", "customer_group", "Wholesale"),
        ("ann", "customer_group", "Retail"),
    ] {
        add_user_permission(client, &env.reg, user, doctype, value)
            .await
            .unwrap();
    }
    let rows = |r: Vec<forgebase_db::user_permissions::UserPermission>| {
        r.into_iter()
            .map(|p| format!("{} {} {}", p.user, p.doctype, p.value))
            .collect::<Vec<_>>()
    };
    assert_eq!(
        rows(list_user_permissions(&*client, None, None).await.unwrap()),
        [
            "ann customer c_none",
            "ann customer_group Retail",
            "ann customer_group Wholesale",
            "zoe customer_group Retail"
        ]
    );
    assert_eq!(
        rows(
            list_user_permissions(&*client, Some("ann"), Some("customer_group"))
                .await
                .unwrap()
        ),
        ["ann customer_group Retail", "ann customer_group Wholesale"]
    );
    assert_eq!(
        rows(
            list_user_permissions(&*client, None, Some("customer"))
                .await
                .unwrap()
        ),
        ["ann customer c_none"]
    );
    assert!(matches!(
        list_user_permissions(&*client, Some("nobody"), None).await,
        Err(AuthError::UserNotFound(_))
    ));
    // A row for a DocType the site does not have is listed (so it can be removed) and ignored.
    env.db
        .exec("INSERT INTO fb_user_permission (user_id, doctype, value) SELECT id, 'old_doctype', 'x' FROM fb_user WHERE username = 'ann'")
        .await;
    assert_eq!(
        list_user_permissions(&*client, Some("ann"), None)
            .await
            .unwrap()
            .len(),
        4
    );
    assert!(
        remove_user_permission(client, "ann", "old_doctype", "x")
            .await
            .unwrap()
    );
}

#[tokio::test]
async fn a_user_has_at_most_a_thousand_documents_per_doctype() {
    let env = env().await;
    seed(&env).await;
    user_with(&env, "ann", &["Sales"]).await;
    // MAX_USER_PERMISSIONS - 1 rows by SQL (rows for names that need not exist), then the
    // limit is reached by an add and the next one is refused. Another DocType has its own limit.
    env.db
        .exec(&format!(
            "INSERT INTO fb_user_permission (user_id, doctype, value) \
             SELECT u.id, 'customer_group', 'n' || g FROM fb_user u, generate_series(1, {}) g \
             WHERE u.username = 'ann'",
            MAX_USER_PERMISSIONS - 1
        ))
        .await;
    let mut client = env.db.pool.get().await.unwrap();
    let client = &mut **client;
    assert!(
        add_user_permission(client, &env.reg, "ann", "customer_group", "Retail")
            .await
            .unwrap()
    );
    let error = add_user_permission(client, &env.reg, "ann", "customer_group", "Wholesale")
        .await
        .unwrap_err();
    let AuthError::InvalidUserPermission(message) = error else {
        panic!("{error}");
    };
    assert!(
        message.contains("1000") && message.contains("customer_group"),
        "{message}"
    );
    // Adding the same row again is still fine, and so is another DocType.
    assert!(
        !add_user_permission(client, &env.reg, "ann", "customer_group", "Retail")
            .await
            .unwrap()
    );
    assert!(
        add_user_permission(client, &env.reg, "ann", "customer", "c_none")
            .await
            .unwrap()
    );
    // The user still works with 1000 names bound.
    let caller = restrictions_of(&env, "ann").await;
    assert_eq!(
        env.names("customer_group", &caller).await,
        strs(&["Retail"])
    );
}

#[tokio::test]
async fn two_sites_keep_their_own_permissions() {
    let site_a = env().await;
    let site_b = env().await;
    for env in [&site_a, &site_b] {
        seed(env).await;
        user_with(env, "ann", &["Sales"]).await;
    }
    let mut client = site_a.db.pool.get().await.unwrap();
    add_user_permission(
        &mut **client,
        &site_a.reg,
        "ann",
        "customer_group",
        "Retail",
    )
    .await
    .unwrap();
    drop(client);
    let on_a = restrictions_of(&site_a, "ann").await;
    let on_b = restrictions_of(&site_b, "ann").await;
    assert_eq!(
        site_a.names("customer_group", &on_a).await,
        strs(&["Retail"])
    );
    assert_eq!(site_b.names("customer_group", &on_b).await.len(), 2);
}

#[tokio::test]
async fn a_database_without_the_table_still_serves_its_users() {
    // A binary that is newer than the last `migrate`: no `fb_user_permission` yet, so nobody has
    // any permission and every request works as before.
    let db = TestDb::new().await;
    {
        let mut client = db.pool.get().await.unwrap();
        forgebase_db::migrate_framework(&mut client, false)
            .await
            .unwrap();
        client
            .batch_execute("DROP TABLE fb_user_permission")
            .await
            .unwrap();
        auth::create_user(&mut **client, "ann", "correct horse battery", &["Sales"])
            .await
            .unwrap();
        let session = auth::login(&mut **client, "ann", "correct horse battery")
            .await
            .unwrap();
        let user = auth::validate_session(&**client, &session.token)
            .await
            .unwrap()
            .unwrap();
        assert!(user.restrictions.is_empty());
    }
}
