//! The group-by summary (M8.3) against a real PostgreSQL (see `common`): counts and aggregates,
//! the read scope it shares with the list (`if_owner`), filters, null keys, the fields that are
//! refused, ordering and the 500-group cap.

mod common;

use std::collections::BTreeMap;

use common::{TestDb, registry};
use forgebase_core::document::Document;
use forgebase_core::meta::Registry;
use forgebase_core::permission::Caller;
use forgebase_core::value::Value;
use forgebase_db::{
    AggregateFn, DbError, Filter, FilterOp, Group, ListQuery, MAX_GROUPS, Summary, SummaryQuery,
    insert, list, summary,
};
use rust_decimal::Decimal;
use time::{Date, Month};

const SALE: &str = r#"
name = "sale"
label = "Sale"
submittable = true
[[fields]]
name = "title"
type = "data"
[[fields]]
name = "status"
type = "select"
options = ["draft", "open", "closed"]
[[fields]]
name = "customer"
type = "link"
target = "person"
[[fields]]
name = "amount"
type = "decimal"
precision = 10
scale = 2
[[fields]]
name = "qty"
type = "int"
[[fields]]
name = "due"
type = "date"
[[fields]]
name = "paid"
type = "check"
[[fields]]
name = "note"
type = "text"
[[fields]]
name = "seen"
type = "datetime"
[[fields]]
name = "lines"
type = "table"
target = "sale_line"

[[permissions]]
role = "Clerk"
read = true
create = true
[[permissions]]
role = "Own"
read = true
create = true
if_owner = true
"#;

const LINE: &str = r#"
name = "sale_line"
label = "Sale Line"
is_child = true
[[fields]]
name = "qty"
type = "int"
"#;

const PERSON: &str = r#"
name = "person"
label = "Person"
naming = { field = "email" }
[[fields]]
name = "email"
type = "data"
required = true
unique = true
[[permissions]]
role = "Clerk"
read = true
create = true
"#;

const TAG: &str = r#"
name = "tag"
label = "Tag"
[[fields]]
name = "label"
type = "data"
[[permissions]]
role = "Clerk"
read = true
create = true
"#;

async fn setup() -> (TestDb, Registry) {
    let db = TestDb::migrated().await;
    let reg = registry(&[SALE, LINE, PERSON, TAG]);
    db.sync(&reg).await.expect("sync");
    (db, reg)
}

fn clerk() -> Caller {
    Caller::user("clerk", vec!["Clerk".to_owned()])
}

fn own(name: &str) -> Caller {
    Caller::user(name, vec!["Own".to_owned()])
}

fn text(s: &str) -> Value {
    Value::Text(s.to_owned())
}

fn dec(s: &str) -> Value {
    Value::Decimal(s.parse::<Decimal>().unwrap())
}

fn date(day: u8) -> Value {
    Value::Date(Date::from_calendar_date(2026, Month::October, day).unwrap())
}

async fn add(
    db: &TestDb,
    reg: &Registry,
    owner: &Caller,
    doctype: &str,
    pairs: Vec<(&str, Value)>,
) {
    let mut client = db.pool.get().await.unwrap();
    let doc = Document {
        name: None,
        values: pairs.into_iter().map(|(k, v)| (k.to_owned(), v)).collect(),
        tables: BTreeMap::new(),
    };
    insert(&mut **client, reg, doctype, doc, owner)
        .await
        .unwrap();
}

async fn sale(db: &TestDb, reg: &Registry, owner: &Caller, pairs: Vec<(&str, Value)>) {
    add(db, reg, owner, "sale", pairs).await;
}

fn query(group_by: &str, aggregates: &[(AggregateFn, &str)]) -> SummaryQuery {
    SummaryQuery {
        filters: vec![],
        group_by: group_by.to_owned(),
        aggregates: aggregates
            .iter()
            .map(|(f, field)| (*f, (*field).to_owned()))
            .collect(),
    }
}

async fn run(
    db: &TestDb,
    reg: &Registry,
    caller: &Caller,
    doctype: &str,
    q: &SummaryQuery,
) -> Result<Summary, DbError> {
    let client = db.pool.get().await.unwrap();
    summary(&**client, reg, doctype, caller, q).await
}

fn keys(summary: &Summary) -> Vec<(Value, i64)> {
    summary
        .groups
        .iter()
        .map(|g| (g.key.clone(), g.count))
        .collect()
}

fn refused(result: Result<Summary, DbError>) -> String {
    match result {
        Err(DbError::InvalidQuery(message)) => message,
        other => panic!("expected InvalidQuery, got {other:?}"),
    }
}

/// draft: 3 sales (10.00, 20.50, 5.00; qty 1, 2, 4), open: 2 (100.00, 50.00; qty 10, 20),
/// closed: 1 (no amount, no qty), no status: 1 (7.00).
async fn seed(db: &TestDb, reg: &Registry) {
    let c = clerk();
    for (status, amount, qty, due) in [
        ("draft", Some("10.00"), Some(1), 3),
        ("draft", Some("20.50"), Some(2), 1),
        ("draft", Some("5.00"), Some(4), 2),
        ("open", Some("100.00"), Some(10), 9),
        ("open", Some("50.00"), Some(20), 8),
        ("closed", None, None, 7),
    ] {
        let mut pairs = vec![("status", text(status)), ("due", date(due))];
        if let Some(a) = amount {
            pairs.push(("amount", dec(a)));
        }
        if let Some(q) = qty {
            pairs.push(("qty", Value::Int(q)));
        }
        sale(db, reg, &c, pairs).await;
    }
    sale(db, reg, &c, vec![("amount", dec("7.00"))]).await;
}

#[tokio::test]
async fn counts_and_aggregates_on_known_data() {
    let (db, reg) = setup().await;
    seed(&db, &reg).await;
    let q = query(
        "status",
        &[
            (AggregateFn::Sum, "amount"),
            (AggregateFn::Avg, "amount"),
            (AggregateFn::Min, "amount"),
            (AggregateFn::Max, "amount"),
            (AggregateFn::Sum, "qty"),
            (AggregateFn::Avg, "qty"),
        ],
    );
    let result = run(&db, &reg, &clerk(), "sale", &q).await.unwrap();
    assert!(!result.truncated);
    // Largest first; the ties (one each) in byte order with the null key last.
    assert_eq!(
        keys(&result),
        vec![
            (text("draft"), 3),
            (text("open"), 2),
            (text("closed"), 1),
            (Value::Null, 1),
        ]
    );
    let draft = &result.groups[0].values;
    assert_eq!(draft["sum_amount"], dec("35.50"));
    // The average of a decimal has the field's scale plus 2: 11.8333... at 4 places.
    assert_eq!(draft["avg_amount"], dec("11.8333"));
    assert_eq!(
        draft["avg_amount"].clone(),
        Value::Decimal("11.8333".parse().unwrap())
    );
    assert_eq!(draft["min_amount"], dec("5.00"));
    assert_eq!(draft["max_amount"], dec("20.50"));
    // An Int sums to an Int, averages to a decimal of 6 places.
    assert_eq!(draft["sum_qty"], Value::Int(7));
    assert_eq!(draft["avg_qty"], dec("2.333333"));
    let dates = query(
        "status",
        &[(AggregateFn::Min, "due"), (AggregateFn::Max, "due")],
    );
    let dates = run(&db, &reg, &clerk(), "sale", &dates).await.unwrap();
    assert_eq!(dates.groups[0].values["min_due"], date(1));
    assert_eq!(dates.groups[0].values["max_due"], date(3));
    // Every value null: null aggregates; the count is still there.
    let closed = &result.groups[2].values;
    assert_eq!(closed["sum_amount"], Value::Null);
    assert_eq!(closed["avg_qty"], Value::Null);
    assert_eq!(closed["min_amount"], Value::Null);
    assert_eq!(closed["sum_qty"], Value::Null);
    // The null group has its own amount.
    assert_eq!(result.groups[3].values["sum_amount"], dec("7.00"));
    // Only what was asked for.
    let plain = run(&db, &reg, &clerk(), "sale", &query("status", &[]))
        .await
        .unwrap();
    assert!(plain.groups.iter().all(|g| g.values.is_empty()));
    assert_eq!(plain.groups[0].count, 3);
}

#[tokio::test]
async fn the_read_scope_is_the_lists_if_owner_included() {
    let (db, reg) = setup().await;
    let (ann, bob) = (own("ann"), own("bob"));
    for status in ["draft", "draft", "open"] {
        sale(
            &db,
            &reg,
            &ann,
            vec![("status", text(status)), ("qty", Value::Int(1))],
        )
        .await;
    }
    for status in ["draft", "closed", "closed", "closed"] {
        sale(
            &db,
            &reg,
            &bob,
            vec![("status", text(status)), ("qty", Value::Int(10))],
        )
        .await;
    }
    let q = query("status", &[(AggregateFn::Sum, "qty")]);
    let mine = run(&db, &reg, &ann, "sale", &q).await.unwrap();
    assert_eq!(
        keys(&mine),
        vec![(text("draft"), 2), (text("open"), 1)],
        "only ann's own documents"
    );
    assert_eq!(mine.groups[0].values["sum_qty"], Value::Int(2));
    let theirs = run(&db, &reg, &bob, "sale", &q).await.unwrap();
    assert_eq!(keys(&theirs), vec![(text("closed"), 3), (text("draft"), 1)]);
    assert_eq!(theirs.groups[0].values["sum_qty"], Value::Int(30));
    let all = run(&db, &reg, &clerk(), "sale", &q).await.unwrap();
    assert_eq!(
        keys(&all),
        vec![(text("closed"), 3), (text("draft"), 3), (text("open"), 1)]
    );
    // The counts agree with a list of the same scope and filters.
    let client = db.pool.get().await.unwrap();
    let listed = list(&**client, &reg, "sale", &ann, &ListQuery::default())
        .await
        .unwrap();
    assert_eq!(
        listed.len() as i64,
        mine.groups.iter().map(|g| g.count).sum::<i64>()
    );
    // Grouping by owner shows the caller's own name only, with the caller's own count.
    let by_owner = run(&db, &reg, &ann, "sale", &query("owner", &[]))
        .await
        .unwrap();
    assert_eq!(keys(&by_owner), vec![(text("ann"), 3)]);
    // A filter on someone else's value does not reveal their rows.
    let mut filtered = query("status", &[]);
    filtered.filters = vec![Filter::new("owner", FilterOp::Eq(text("bob")))];
    assert!(
        run(&db, &reg, &ann, "sale", &filtered)
            .await
            .unwrap()
            .groups
            .is_empty()
    );
}

#[tokio::test]
async fn without_the_read_right_it_is_denied_and_a_child_doctype_is_refused() {
    let (db, reg) = setup().await;
    let nobody = Caller::user("zed", vec!["Nobody".to_owned()]);
    let result = run(&db, &reg, &nobody, "sale", &query("status", &[])).await;
    assert!(
        matches!(result, Err(DbError::PermissionDenied { .. })),
        "{result:?}"
    );
    // Denied before the query is looked at, so even an invalid one says nothing.
    let result = run(&db, &reg, &nobody, "sale", &query("nope", &[])).await;
    assert!(
        matches!(result, Err(DbError::PermissionDenied { .. })),
        "{result:?}"
    );
    let result = run(&db, &reg, &clerk(), "sale_line", &query("qty", &[])).await;
    assert!(
        matches!(result, Err(DbError::ChildDocType { .. })),
        "{result:?}"
    );
    let result = run(&db, &reg, &clerk(), "nope", &query("qty", &[])).await;
    assert!(
        matches!(result, Err(DbError::UnknownDocType { .. })),
        "{result:?}"
    );
}

#[tokio::test]
async fn filters_combine_with_the_group_by() {
    let (db, reg) = setup().await;
    seed(&db, &reg).await;
    let mut q = query("status", &[(AggregateFn::Sum, "amount")]);
    q.filters = vec![
        Filter::new("amount", FilterOp::Ge(dec("10.00"))),
        Filter::new("status", FilterOp::Ne(text("open"))),
    ];
    let result = run(&db, &reg, &clerk(), "sale", &q).await.unwrap();
    // Ne leaves out the null status, and the amount filter leaves out the 5.00 and the nulls.
    assert_eq!(keys(&result), vec![(text("draft"), 2)]);
    assert_eq!(result.groups[0].values["sum_amount"], dec("30.50"));
    // A filter is validated as in a list.
    let mut bad = query("status", &[]);
    bad.filters = vec![Filter::new("nope", FilterOp::IsNull(true))];
    assert!(refused(run(&db, &reg, &clerk(), "sale", &bad).await).contains("unknown field"));
    bad.filters = vec![Filter::new("qty", FilterOp::Like("1%".into()))];
    assert!(refused(run(&db, &reg, &clerk(), "sale", &bad).await).contains("text fields only"));
}

#[tokio::test]
async fn every_kind_of_group_key_works() {
    let (db, reg) = setup().await;
    let c = clerk();
    add(
        &db,
        &reg,
        &c,
        "person",
        vec![("email", text("zoe@example.com"))],
    )
    .await;
    add(
        &db,
        &reg,
        &c,
        "person",
        vec![("email", text("amy@example.com"))],
    )
    .await;
    sale(
        &db,
        &reg,
        &c,
        vec![
            ("customer", text("zoe@example.com")),
            ("paid", Value::Bool(true)),
            ("qty", Value::Int(2)),
            ("title", text("x")),
            ("due", date(5)),
        ],
    )
    .await;
    sale(
        &db,
        &reg,
        &c,
        vec![
            ("customer", text("zoe@example.com")),
            ("paid", Value::Bool(false)),
            ("qty", Value::Int(2)),
            ("title", text("x")),
            ("due", date(5)),
        ],
    )
    .await;
    sale(
        &db,
        &reg,
        &c,
        vec![
            ("customer", text("amy@example.com")),
            ("qty", Value::Int(3)),
            ("title", text("y")),
            ("due", date(6)),
        ],
    )
    .await;
    let by = |field: &str| query(field, &[]);
    // A link groups by the target's name (a tie in byte order).
    assert_eq!(
        keys(&run(&db, &reg, &c, "sale", &by("customer")).await.unwrap()),
        vec![(text("zoe@example.com"), 2), (text("amy@example.com"), 1)]
    );
    assert_eq!(
        keys(&run(&db, &reg, &c, "sale", &by("paid")).await.unwrap()),
        vec![
            (Value::Bool(false), 1),
            (Value::Bool(true), 1),
            (Value::Null, 1)
        ]
    );
    assert_eq!(
        keys(&run(&db, &reg, &c, "sale", &by("qty")).await.unwrap()),
        vec![(Value::Int(2), 2), (Value::Int(3), 1)]
    );
    assert_eq!(
        keys(&run(&db, &reg, &c, "sale", &by("due")).await.unwrap()),
        vec![(date(5), 2), (date(6), 1)]
    );
    assert_eq!(
        keys(&run(&db, &reg, &c, "sale", &by("title")).await.unwrap()),
        vec![(text("x"), 2), (text("y"), 1)]
    );
    // docstatus is an Int on a submittable DocType; modified_by is text.
    assert_eq!(
        keys(&run(&db, &reg, &c, "sale", &by("docstatus")).await.unwrap()),
        vec![(Value::Int(0), 3)]
    );
    assert_eq!(
        keys(
            &run(&db, &reg, &c, "sale", &by("modified_by"))
                .await
                .unwrap()
        ),
        vec![(text("clerk"), 3)]
    );
}

#[tokio::test]
async fn the_group_by_and_the_aggregates_are_validated() {
    let (db, reg) = setup().await;
    seed(&db, &reg).await;
    let check = |q: SummaryQuery, needle: &'static str| {
        let (db, reg) = (&db, &reg);
        async move {
            let message = refused(run(db, reg, &clerk(), "sale", &q).await);
            assert!(message.contains(needle), "{q:?}: {message}");
        }
    };
    for (field, needle) in [
        ("note", "text field"),
        ("lines", "table field"),
        ("amount", "decimal field"),
        ("seen", "datetime field"),
        ("name", "cannot group by"),
        ("created_at", "cannot group by"),
        ("modified_at", "cannot group by"),
        ("version", "cannot group by"),
        ("workflow_state", "unknown field"),
        ("nope", "unknown field"),
        ("", "unknown field"),
        ("id", "unknown field"),
        ("status\" ; drop table doc_sale; --", "unknown field"),
    ] {
        check(query(field, &[]), needle).await;
    }
    for (aggregate, needle) in [
        ((AggregateFn::Sum, "title"), "data field"),
        ((AggregateFn::Avg, "due"), "date field"),
        ((AggregateFn::Sum, "due"), "date field"),
        ((AggregateFn::Min, "title"), "data field"),
        ((AggregateFn::Max, "paid"), "check field"),
        ((AggregateFn::Sum, "note"), "text field"),
        ((AggregateFn::Sum, "customer"), "link field"),
        ((AggregateFn::Sum, "lines"), "table field"),
        // Standard columns are not aggregated.
        ((AggregateFn::Sum, "version"), "unknown field"),
        ((AggregateFn::Max, "docstatus"), "unknown field"),
        ((AggregateFn::Sum, "nope"), "unknown field"),
    ] {
        check(query("status", &[aggregate]), needle).await;
    }
    check(
        query(
            "status",
            &[(AggregateFn::Sum, "qty"), (AggregateFn::Sum, "qty")],
        ),
        "twice",
    )
    .await;
    let seven = [
        (AggregateFn::Sum, "qty"),
        (AggregateFn::Avg, "qty"),
        (AggregateFn::Min, "qty"),
        (AggregateFn::Max, "qty"),
        (AggregateFn::Sum, "amount"),
        (AggregateFn::Avg, "amount"),
        (AggregateFn::Min, "amount"),
    ];
    check(query("status", &seven), "at most 6").await;
    // Six are fine.
    assert!(
        run(&db, &reg, &clerk(), "sale", &query("status", &seven[..6]))
            .await
            .is_ok()
    );
    // Min and max take dates, sum and avg take neither dates nor text.
    assert!(
        run(
            &db,
            &reg,
            &clerk(),
            "sale",
            &query("status", &[(AggregateFn::Max, "due")])
        )
        .await
        .is_ok()
    );
}

#[tokio::test]
async fn workflow_state_is_a_group_key_only_with_a_workflow() {
    // The check above covers a DocType without a workflow; a DocType without `submittable`
    // has no docstatus either.
    let (db, reg) = setup().await;
    let message = refused(run(&db, &reg, &clerk(), "tag", &query("docstatus", &[])).await);
    assert!(message.contains("unknown field"), "{message}");
}

#[tokio::test]
async fn text_keys_are_ordered_by_bytes_after_the_count() {
    let (db, reg) = setup().await;
    let c = clerk();
    for label in ["b", "B", "a", "_", "Z", "b"] {
        add(&db, &reg, &c, "tag", vec![("label", text(label))]).await;
    }
    let result = run(&db, &reg, &c, "tag", &query("label", &[]))
        .await
        .unwrap();
    // `b` twice first; then the ties by byte order, not by locale.
    assert_eq!(
        keys(&result),
        vec![
            (text("b"), 2),
            (text("B"), 1),
            (text("Z"), 1),
            (text("_"), 1),
            (text("a"), 1),
        ]
    );
}

#[tokio::test]
async fn more_than_500_groups_are_cut_and_marked() {
    let (db, reg) = setup().await;
    let insert = |count: usize| {
        format!(
            "INSERT INTO doc_tag (id, name, owner, created_at, modified_at, modified_by, label) \
             SELECT gen_random_uuid(), 'tag-' || n, 'sys', now(), now(), 'sys', 'label-' || lpad(n::text, 4, '0') \
             FROM generate_series(1, {count}) n"
        )
    };
    db.exec(&insert(MAX_GROUPS)).await;
    let result = run(&db, &reg, &clerk(), "tag", &query("label", &[]))
        .await
        .unwrap();
    assert_eq!(result.groups.len(), MAX_GROUPS);
    assert!(!result.truncated, "exactly 500 groups are not truncated");
    db.exec(
        "INSERT INTO doc_tag (id, name, owner, created_at, modified_at, modified_by, label) \
             VALUES (gen_random_uuid(), 'tag-x', 'sys', now(), now(), 'sys', 'label-0001')",
    )
    .await;
    db.exec(
        "INSERT INTO doc_tag (id, name, owner, created_at, modified_at, modified_by, label) \
             VALUES (gen_random_uuid(), 'tag-y', 'sys', now(), now(), 'sys', 'label-9999')",
    )
    .await;
    let result = run(&db, &reg, &clerk(), "tag", &query("label", &[]))
        .await
        .unwrap();
    assert!(result.truncated);
    assert_eq!(result.groups.len(), MAX_GROUPS);
    // The group with two documents is first; the cut takes the last keys.
    let Group { key, count, .. } = &result.groups[0];
    assert_eq!((key, *count), (&text("label-0001"), 2));
    assert_eq!(result.groups.last().unwrap().key, text("label-0500"));
}
