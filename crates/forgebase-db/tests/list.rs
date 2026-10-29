//! The list query (M3.2, ADR 0011) against a real PostgreSQL (see `common`): every operator,
//! sorting and paging, limits, hostile input, and that unreadable rows never come back whatever
//! the filter, sort or page.

mod common;

use std::collections::BTreeMap;

use common::{TestDb, registry};
use forgebase_core::document::{Document, Row};
use forgebase_core::meta::Registry;
use forgebase_core::permission::{ADMINISTRATOR, Caller};
use forgebase_core::value::Value;
use forgebase_db::{
    DEFAULT_LIMIT, DbError, Filter, FilterOp, ListQuery, MAX_LIMIT, Sort, StoredDocument, cancel,
    delete, get, insert, list, submit,
};
use rust_decimal::Decimal;
use time::{Date, Month, OffsetDateTime};

const GROUP: &str = r#"
name = "grp"
label = "Group"
naming = { field = "code" }
[[fields]]
name = "code"
type = "data"
required = true
unique = true

[[permissions]]
role = "Clerk"
read = true
create = true
[[permissions]]
role = "Own"
read = true
"#;

const LINE: &str = r#"
name = "item_line"
label = "Item Line"
is_child = true
[[fields]]
name = "qty"
type = "int"
"#;

/// Every field type; `Clerk` reads and writes everything, `Own` only its own documents.
const ITEM: &str = r#"
name = "item"
label = "Item"
[[fields]]
name = "title"
type = "data"
[[fields]]
name = "qty"
type = "int"
[[fields]]
name = "price"
type = "decimal"
precision = 10
scale = 2
[[fields]]
name = "done"
type = "check"
[[fields]]
name = "due"
type = "date"
[[fields]]
name = "seen"
type = "datetime"
[[fields]]
name = "kind"
type = "select"
options = ["a", "b"]
[[fields]]
name = "note"
type = "text"
[[fields]]
name = "grp"
type = "link"
target = "grp"
[[fields]]
name = "lines"
type = "table"
target = "item_line"

[[permissions]]
role = "Clerk"
read = true
write = true
create = true
delete = true
[[permissions]]
role = "Own"
read = true
create = true
if_owner = true
"#;

const QUOTE: &str = r#"
name = "quote"
label = "Quote"
submittable = true
[[fields]]
name = "title"
type = "data"

[[permissions]]
role = "Clerk"
read = true
create = true
submit = true
cancel = true
delete = true
"#;

async fn setup() -> (TestDb, Registry) {
    let db = TestDb::migrated().await;
    let reg = registry(&[GROUP, LINE, ITEM, QUOTE]);
    db.sync(&reg).await.expect("sync");
    (db, reg)
}

fn sys() -> Caller {
    Caller::system("sys")
}

fn user(name: &str, roles: &[&str]) -> Caller {
    Caller::user(name, roles.iter().map(|r| (*r).to_owned()).collect())
}

fn text(s: &str) -> Value {
    Value::Text(s.to_owned())
}

fn dec(s: &str) -> Value {
    Value::Decimal(s.parse::<Decimal>().unwrap())
}

fn date(d: u8) -> Value {
    Value::Date(Date::from_calendar_date(2026, Month::October, d).unwrap())
}

fn at(day: i64) -> Value {
    Value::Datetime(OffsetDateTime::UNIX_EPOCH + time::Duration::days(day))
}

fn doc(pairs: Vec<(&str, Value)>) -> Document {
    Document {
        name: None,
        values: pairs.into_iter().map(|(k, v)| (k.to_owned(), v)).collect(),
        tables: BTreeMap::new(),
    }
}

async fn add(db: &TestDb, reg: &Registry, owner: &Caller, pairs: Vec<(&str, Value)>) -> String {
    let mut client = db.pool.get().await.unwrap();
    insert(&mut **client, reg, "item", doc(pairs), owner)
        .await
        .unwrap()
        .name
}

async fn titles(db: &TestDb, reg: &Registry, caller: &Caller, query: &ListQuery) -> Vec<String> {
    let client = db.pool.get().await.unwrap();
    list(&**client, reg, "item", caller, query)
        .await
        .unwrap()
        .iter()
        .map(|d| match &d.values["title"] {
            Value::Text(t) => t.clone(),
            other => panic!("{other:?}"),
        })
        .collect()
}

fn q(filters: Vec<Filter>) -> ListQuery {
    ListQuery {
        filters,
        ..ListQuery::default()
    }
}

fn f(field: &str, op: FilterOp) -> Filter {
    Filter::new(field, op)
}

fn sorted(query: Vec<Filter>, sort: Vec<Sort>) -> ListQuery {
    ListQuery {
        filters: query,
        sort,
        ..ListQuery::default()
    }
}

/// Three groups and five items with varied values.
async fn seed(db: &TestDb, reg: &Registry) {
    let mut client = db.pool.get().await.unwrap();
    for code in ["g1", "g2"] {
        insert(
            &mut **client,
            reg,
            "grp",
            doc(vec![("code", text(code))]),
            &sys(),
        )
        .await
        .unwrap();
    }
    drop(client);
    let rows = [
        ("a", 1, "10.00", true, 1, 1, "a", Some("g1")),
        ("b", 2, "20.50", false, 2, 2, "b", Some("g1")),
        ("c", 3, "30.00", true, 3, 3, "a", Some("g2")),
        ("d", 4, "5.25", false, 4, 4, "b", None),
        ("e", 5, "40.00", true, 5, 5, "a", Some("g2")),
    ];
    for (title, qty, price, done, due, seen, kind, grp) in rows {
        let mut pairs = vec![
            ("title", text(title)),
            ("qty", Value::Int(qty)),
            ("price", dec(price)),
            ("done", Value::Bool(done)),
            ("due", date(due)),
            ("seen", at(seen)),
            ("kind", text(kind)),
        ];
        if let Some(grp) = grp {
            pairs.push(("grp", text(grp)));
        }
        add(db, reg, &sys(), pairs).await;
    }
}

#[tokio::test]
async fn every_operator_on_every_field_type() {
    let (db, reg) = setup().await;
    seed(&db, &reg).await;
    let by_title = |filters| sorted(filters, vec![Sort::asc("title")]);
    let check = |filters: Vec<Filter>, expected: &'static [&'static str]| {
        let (db, reg, query) = (&db, &reg, by_title(filters));
        async move {
            assert_eq!(titles(db, reg, &sys(), &query).await, expected, "{query:?}");
        }
    };
    // Int
    check(vec![f("qty", FilterOp::Eq(Value::Int(3)))], &["c"]).await;
    check(
        vec![f("qty", FilterOp::Ne(Value::Int(3)))],
        &["a", "b", "d", "e"],
    )
    .await;
    check(vec![f("qty", FilterOp::Lt(Value::Int(3)))], &["a", "b"]).await;
    check(
        vec![f("qty", FilterOp::Le(Value::Int(3)))],
        &["a", "b", "c"],
    )
    .await;
    check(vec![f("qty", FilterOp::Gt(Value::Int(3)))], &["d", "e"]).await;
    check(
        vec![f("qty", FilterOp::Ge(Value::Int(3)))],
        &["c", "d", "e"],
    )
    .await;
    check(
        vec![f(
            "qty",
            FilterOp::In(vec![Value::Int(1), Value::Int(5), Value::Int(9)]),
        )],
        &["a", "e"],
    )
    .await;
    // Decimal, with exact comparison
    check(vec![f("price", FilterOp::Eq(dec("20.5")))], &["b"]).await;
    check(vec![f("price", FilterOp::Lt(dec("10.00")))], &["d"]).await;
    check(vec![f("price", FilterOp::Ge(dec("30")))], &["c", "e"]).await;
    // Check
    check(
        vec![f("done", FilterOp::Eq(Value::Bool(true)))],
        &["a", "c", "e"],
    )
    .await;
    check(
        vec![f("done", FilterOp::Ne(Value::Bool(true)))],
        &["b", "d"],
    )
    .await;
    // Date and datetime
    check(vec![f("due", FilterOp::Gt(date(3)))], &["d", "e"]).await;
    check(
        vec![f("due", FilterOp::In(vec![date(1), date(2)]))],
        &["a", "b"],
    )
    .await;
    check(vec![f("seen", FilterOp::Le(at(2)))], &["a", "b"]).await;
    // Text and Select
    check(vec![f("title", FilterOp::Eq(text("b")))], &["b"]).await;
    check(vec![f("title", FilterOp::Gt(text("c")))], &["d", "e"]).await;
    check(vec![f("kind", FilterOp::Eq(text("b")))], &["b", "d"]).await;
    check(
        vec![f("kind", FilterOp::In(vec![text("a"), text("zzz")]))],
        &["a", "c", "e"],
    )
    .await;
    // Several filters are ANDed.
    check(
        vec![
            f("kind", FilterOp::Eq(text("a"))),
            f("qty", FilterOp::Gt(Value::Int(1))),
        ],
        &["c", "e"],
    )
    .await;
    // Null checks (`<>` leaves out nulls, as SQL does)
    check(vec![f("grp", FilterOp::IsNull(true))], &["d"]).await;
    check(
        vec![f("grp", FilterOp::IsNull(false))],
        &["a", "b", "c", "e"],
    )
    .await;
    check(
        vec![f("note", FilterOp::IsNull(true))],
        &["a", "b", "c", "d", "e"],
    )
    .await;
    check(vec![f("note", FilterOp::IsNull(false))], &[]).await;
    check(vec![f("note", FilterOp::Ne(text("x")))], &[]).await;
    // Standard columns
    check(
        vec![f("version", FilterOp::Eq(Value::Int(1)))],
        &["a", "b", "c", "d", "e"],
    )
    .await;
    check(
        vec![f("owner", FilterOp::Eq(text("sys")))],
        &["a", "b", "c", "d", "e"],
    )
    .await;
    check(vec![f("owner", FilterOp::Eq(text("nobody")))], &[]).await;
    check(
        vec![f("modified_by", FilterOp::Like("s%".into()))],
        &["a", "b", "c", "d", "e"],
    )
    .await;
    check(
        vec![f("created_at", FilterOp::Gt(at(-1)))],
        &["a", "b", "c", "d", "e"],
    )
    .await;
    check(vec![f("modified_at", FilterOp::Lt(at(-1)))], &[]).await;
    // Filtering by `name`
    let client = db.pool.get().await.unwrap();
    let all = list(&**client, &reg, "item", &sys(), &ListQuery::default())
        .await
        .unwrap();
    let one = all[2].name.clone();
    let only = list(
        &**client,
        &reg,
        "item",
        &sys(),
        &q(vec![f("name", FilterOp::Eq(text(&one)))]),
    )
    .await
    .unwrap();
    assert_eq!(only.len(), 1);
    assert_eq!(only[0].name, one);
}

#[tokio::test]
async fn like_matches_patterns_on_text_fields_only() {
    let (db, reg) = setup().await;
    for t in ["apple", "apricot", "banana", "100%", "a_b", "AX"] {
        add(&db, &reg, &sys(), vec![("title", text(t))]).await;
    }
    let by = |p: &str| {
        sorted(
            vec![f("title", FilterOp::Like(p.into()))],
            vec![Sort::asc("title")],
        )
    };
    assert_eq!(
        titles(&db, &reg, &sys(), &by("ap%")).await,
        ["apple", "apricot"]
    );
    assert_eq!(titles(&db, &reg, &sys(), &by("%an%")).await, ["banana"]);
    assert_eq!(titles(&db, &reg, &sys(), &by("a_b")).await, ["a_b"]);
    // Case-sensitive, and `\` escapes a wildcard.
    assert_eq!(
        titles(&db, &reg, &sys(), &by("ax")).await,
        Vec::<String>::new()
    );
    assert_eq!(titles(&db, &reg, &sys(), &by("100\\%")).await, ["100%"]);
    // A SQL fragment in the pattern is just a pattern.
    assert_eq!(
        titles(&db, &reg, &sys(), &by("x' OR '1'='1")).await,
        Vec::<String>::new()
    );
    // `like` on `kind` (select) and `note` (text) is fine; on anything else it is rejected.
    let client = db.pool.get().await.unwrap();
    for field in [
        "qty",
        "price",
        "done",
        "due",
        "seen",
        "grp",
        "version",
        "created_at",
    ] {
        let error = list(
            &**client,
            &reg,
            "item",
            &sys(),
            &q(vec![f(field, FilterOp::Like("%".into()))]),
        )
        .await
        .unwrap_err();
        assert!(
            matches!(&error, DbError::InvalidQuery(m) if m.contains("text fields only")),
            "{field}: {error:?}"
        );
    }
    for field in ["kind", "note", "name", "owner", "modified_by"] {
        list(
            &**client,
            &reg,
            "item",
            &sys(),
            &q(vec![f(field, FilterOp::Like("%".into()))]),
        )
        .await
        .unwrap();
    }
}

#[tokio::test]
async fn link_fields_filter_and_sort_by_the_target_name() {
    let (db, reg) = setup().await;
    seed(&db, &reg).await;
    let by = |filters| sorted(filters, vec![Sort::asc("title")]);
    let eq = by(vec![f("grp", FilterOp::Eq(text("g1")))]);
    assert_eq!(titles(&db, &reg, &sys(), &eq).await, ["a", "b"]);
    let ne = by(vec![f("grp", FilterOp::Ne(text("g1")))]);
    assert_eq!(titles(&db, &reg, &sys(), &ne).await, ["c", "e"]);
    let both = by(vec![f("grp", FilterOp::In(vec![text("g1"), text("g2")]))]);
    assert_eq!(titles(&db, &reg, &sys(), &both).await, ["a", "b", "c", "e"]);
    // A target that does not exist matches nothing (`=`, `in`) or every linked row (`<>`).
    let ghost = by(vec![f("grp", FilterOp::Eq(text("ghost")))]);
    assert_eq!(
        titles(&db, &reg, &sys(), &ghost).await,
        Vec::<String>::new()
    );
    let not_ghost = by(vec![f("grp", FilterOp::Ne(text("ghost")))]);
    assert_eq!(
        titles(&db, &reg, &sys(), &not_ghost).await,
        ["a", "b", "c", "e"]
    );
    // Sorting by the link orders by the target's name, then the tie-breaker.
    let desc = sorted(vec![], vec![Sort::desc("grp")]);
    // Descending puts nulls first, then g2, g1; ties by name (random, so compare as groups).
    let order = titles(&db, &reg, &sys(), &desc).await;
    assert_eq!(order[0], "d");
    let mut g2 = order[1..3].to_vec();
    g2.sort();
    assert_eq!(g2, ["c", "e"]);
    // The target value is returned as a name.
    let client = db.pool.get().await.unwrap();
    let listed = list(
        &**client,
        &reg,
        "item",
        &sys(),
        &q(vec![f("title", FilterOp::Eq(text("a")))]),
    )
    .await
    .unwrap();
    assert_eq!(listed[0].values["grp"], text("g1"));
    // Only equality-like operators and null checks apply to links.
    for op in [
        FilterOp::Gt(text("a")),
        FilterOp::Lt(text("a")),
        FilterOp::Like("g%".into()),
    ] {
        let error = list(&**client, &reg, "item", &sys(), &q(vec![f("grp", op)]))
            .await
            .unwrap_err();
        assert!(matches!(error, DbError::InvalidQuery(_)), "{error:?}");
    }
}

#[tokio::test]
async fn type_mismatches_and_bad_operands_are_rejected() {
    let (db, reg) = setup().await;
    let client = db.pool.get().await.unwrap();
    let bad = [
        f("qty", FilterOp::Eq(text("1"))),
        f("qty", FilterOp::Eq(dec("1"))),
        f("price", FilterOp::Eq(Value::Int(1))),
        f("title", FilterOp::Eq(Value::Int(1))),
        f("done", FilterOp::Eq(text("true"))),
        f("due", FilterOp::Eq(text("2026-10-01"))),
        f("seen", FilterOp::Eq(date(1))),
        f("grp", FilterOp::Eq(Value::Int(1))),
        f("version", FilterOp::Eq(text("1"))),
        f("created_at", FilterOp::Eq(Value::Int(1))),
        f("qty", FilterOp::Eq(Value::Null)),
        f("qty", FilterOp::In(vec![Value::Int(1), text("2")])),
        f("qty", FilterOp::In(vec![])),
        f("done", FilterOp::Lt(Value::Bool(true))),
        f("title", FilterOp::Eq(text("a\0b"))),
        f("lines", FilterOp::IsNull(true)),
        f("nope", FilterOp::IsNull(true)),
        f("docstatus", FilterOp::IsNull(true)), // not submittable
    ];
    for filter in bad {
        let error = list(&**client, &reg, "item", &sys(), &q(vec![filter.clone()]))
            .await
            .unwrap_err();
        assert!(
            matches!(error, DbError::InvalidQuery(_)),
            "{filter:?}: {error:?}"
        );
    }
}

#[tokio::test]
async fn sorting_is_multi_field_and_paging_is_stable() {
    let (db, reg) = setup().await;
    seed(&db, &reg).await;
    // kind desc, then qty desc.
    let query = sorted(vec![], vec![Sort::desc("kind"), Sort::desc("qty")]);
    assert_eq!(
        titles(&db, &reg, &sys(), &query).await,
        ["d", "b", "e", "c", "a"]
    );
    let query = sorted(vec![], vec![Sort::asc("price")]);
    assert_eq!(
        titles(&db, &reg, &sys(), &query).await,
        ["d", "a", "b", "c", "e"]
    );
    let query = sorted(vec![], vec![Sort::asc("due"), Sort::desc("title")]);
    assert_eq!(
        titles(&db, &reg, &sys(), &query).await,
        ["a", "b", "c", "d", "e"]
    );
    let query = sorted(vec![], vec![Sort::desc("seen")]);
    assert_eq!(
        titles(&db, &reg, &sys(), &query).await,
        ["e", "d", "c", "b", "a"]
    );
    // All five tie on `done`'s group of two values; paging by 2 sees every row exactly once, in
    // the same order as one big page, because `name` is the final tie-breaker.
    let page = |offset| ListQuery {
        sort: vec![Sort::asc("done")],
        limit: Some(2),
        offset: Some(offset),
        ..ListQuery::default()
    };
    let client = db.pool.get().await.unwrap();
    let whole = list(
        &**client,
        &reg,
        "item",
        &sys(),
        &ListQuery {
            sort: vec![Sort::asc("done")],
            ..ListQuery::default()
        },
    )
    .await
    .unwrap();
    let mut paged = Vec::new();
    for offset in [0, 2, 4, 6] {
        paged.extend(
            list(&**client, &reg, "item", &sys(), &page(offset))
                .await
                .unwrap(),
        );
    }
    assert_eq!(paged.len(), 5);
    assert_eq!(
        paged.iter().map(|d| &d.name).collect::<Vec<_>>(),
        whole.iter().map(|d| &d.name).collect::<Vec<_>>()
    );
    // Within equal sort values the order is by name.
    let falses: Vec<&String> = whole
        .iter()
        .filter(|d| d.values["done"] == Value::Bool(false))
        .map(|d| &d.name)
        .collect();
    let mut expected = falses.clone();
    expected.sort();
    assert_eq!(falses, expected);
}

#[tokio::test]
async fn paging_limits() {
    let (db, reg) = setup().await;
    {
        let mut client = db.pool.get().await.unwrap();
        for i in 0..25 {
            insert(
                &mut **client,
                &reg,
                "item",
                doc(vec![
                    ("title", text(&format!("t{i:02}"))),
                    ("qty", Value::Int(i)),
                ]),
                &sys(),
            )
            .await
            .unwrap();
        }
    }
    let client = db.pool.get().await.unwrap();
    let run = |limit, offset| {
        let query = ListQuery {
            sort: vec![Sort::asc("qty")],
            limit,
            offset,
            ..ListQuery::default()
        };
        let (reg, client) = (&reg, &client);
        async move { list(&***client, reg, "item", &sys(), &query).await }
    };
    assert_eq!(DEFAULT_LIMIT, 20);
    assert_eq!(MAX_LIMIT, 500);
    assert_eq!(run(None, None).await.unwrap().len(), 20);
    assert_eq!(run(Some(500), None).await.unwrap().len(), 25);
    assert_eq!(run(Some(3), Some(22)).await.unwrap().len(), 3);
    assert_eq!(run(Some(10), Some(20)).await.unwrap().len(), 5);
    assert_eq!(run(Some(10), Some(25)).await.unwrap().len(), 0);
    assert_eq!(run(Some(10), Some(100_000)).await.unwrap().len(), 0);
    assert_eq!(
        run(Some(1), Some(0)).await.unwrap()[0].values["qty"],
        Value::Int(0)
    );
    for (limit, offset) in [
        (Some(501), None),
        (Some(0), None),
        (Some(u32::MAX), None),
        (None, Some(u64::MAX)),
    ] {
        let error = run(limit, offset).await.unwrap_err();
        assert!(
            matches!(error, DbError::InvalidQuery(_)),
            "{limit:?} {offset:?}: {error:?}"
        );
    }
}

#[tokio::test]
async fn submittable_doctypes_have_docstatus_and_amended_from_is_not_a_filter() {
    let (db, reg) = setup().await;
    let mut client = db.pool.get().await.unwrap();
    let client = &mut **client;
    let mut names = Vec::new();
    for t in ["one", "two", "three"] {
        names.push(
            insert(client, &reg, "quote", doc(vec![("title", text(t))]), &sys())
                .await
                .unwrap(),
        );
    }
    let v = submit(client, &reg, "quote", &names[1].name, 1, &sys())
        .await
        .unwrap()
        .version;
    submit(client, &reg, "quote", &names[2].name, 1, &sys())
        .await
        .unwrap();
    cancel(client, &reg, "quote", &names[1].name, v, &sys())
        .await
        .unwrap();
    let count = |status: i64| {
        let query = q(vec![f("docstatus", FilterOp::Eq(Value::Int(status)))]);
        let reg = &reg;
        let client = &*client;
        let query = query.clone();
        async move {
            list(client, reg, "quote", &sys(), &query)
                .await
                .unwrap()
                .len()
        }
    };
    assert_eq!(count(0).await, 1);
    assert_eq!(count(1).await, 1);
    assert_eq!(count(2).await, 1);
    let ge1 = q(vec![f("docstatus", FilterOp::Ge(Value::Int(1)))]);
    assert_eq!(
        list(&*client, &reg, "quote", &sys(), &ge1)
            .await
            .unwrap()
            .len(),
        2
    );
    let error = list(
        &*client,
        &reg,
        "quote",
        &sys(),
        &q(vec![f("amended_from", FilterOp::IsNull(true))]),
    )
    .await
    .unwrap_err();
    assert!(matches!(error, DbError::InvalidQuery(_)));
}

#[tokio::test]
async fn list_rows_match_get_and_have_no_tables() {
    let (db, reg) = setup().await;
    seed(&db, &reg).await;
    let mut client = db.pool.get().await.unwrap();
    // An item with child rows.
    let mut with_rows = doc(vec![("title", text("rows")), ("grp", text("g1"))]);
    let row: Row = BTreeMap::from([("qty".to_owned(), Value::Int(1))]);
    with_rows
        .tables
        .insert("lines".into(), vec![row.clone(), row]);
    let made = insert(&mut **client, &reg, "item", with_rows, &sys())
        .await
        .unwrap();
    assert_eq!(made.tables["lines"].len(), 2);
    let listed = list(
        &**client,
        &reg,
        "item",
        &sys(),
        &q(vec![f("title", FilterOp::Eq(text("rows")))]),
    )
    .await
    .unwrap();
    assert_eq!(listed.len(), 1);
    let mut got: StoredDocument = get(&mut **client, &reg, "item", &made.name, &sys())
        .await
        .unwrap()
        .unwrap();
    assert!(listed[0].tables.is_empty());
    got.tables.clear();
    assert_eq!(listed[0], got);
    assert_eq!(listed[0].values["grp"], text("g1"));
}

// ---- Permissions ---------------------------------------------------------------------------

#[tokio::test]
async fn a_list_never_returns_rows_the_caller_cannot_read() {
    let (db, reg) = setup().await;
    {
        let mut client = db.pool.get().await.unwrap();
        insert(
            &mut **client,
            &reg,
            "grp",
            doc(vec![("code", text("g"))]),
            &sys(),
        )
        .await
        .unwrap();
    }
    let ann = user("ann", &["Own"]);
    let bob = user("bob", &["Own"]);
    let mut bobs = Vec::new();
    for i in 0..30 {
        let owner = if i % 3 == 0 { &ann } else { &bob };
        let title = format!("{}{i:02}", if i % 3 == 0 { "ann" } else { "bob" });
        let name = add(
            &db,
            &reg,
            owner,
            vec![
                ("title", text(&title)),
                ("qty", Value::Int(i)),
                ("grp", text("g")),
            ],
        )
        .await;
        if i % 3 != 0 {
            bobs.push((title, name));
        }
    }
    let anns = titles(&db, &reg, &ann, &sorted(vec![], vec![Sort::asc("qty")])).await;
    assert_eq!(anns.len(), 10);
    assert!(anns.iter().all(|t| t.starts_with("ann")), "{anns:?}");

    // Filters cannot reach them: by owner, by name, by title, by `in`, by negation, by link,
    // by `like`, by null checks ...
    let bob_name = bobs[0].1.clone();
    let attempts = vec![
        vec![f("owner", FilterOp::Eq(text("bob")))],
        vec![f("owner", FilterOp::Ne(text("ann")))],
        vec![f("owner", FilterOp::Like("b%".into()))],
        vec![f("name", FilterOp::Eq(text(&bob_name)))],
        vec![f(
            "name",
            FilterOp::In(bobs.iter().map(|(_, n)| text(n)).collect()),
        )],
        vec![f("title", FilterOp::Like("bob%".into()))],
        vec![f("title", FilterOp::Eq(text(&bobs[1].0)))],
        vec![f("modified_by", FilterOp::Eq(text("bob")))],
        vec![f("qty", FilterOp::Eq(Value::Int(1)))],
    ];
    for filters in attempts {
        let result = titles(&db, &reg, &ann, &q(filters.clone())).await;
        assert!(result.is_empty(), "{filters:?} -> {result:?}");
    }
    // ... and `get` agrees.
    let mut client = db.pool.get().await.unwrap();
    assert!(
        get(&mut **client, &reg, "item", &bob_name, &ann)
            .await
            .unwrap()
            .is_none()
    );
    // A filter that matches both sides only returns her own.
    let linked = titles(&db, &reg, &ann, &q(vec![f("grp", FilterOp::Eq(text("g")))])).await;
    assert_eq!(linked.len(), 10);
    let nulls = titles(&db, &reg, &ann, &q(vec![f("note", FilterOp::IsNull(true))])).await;
    assert_eq!(nulls.len(), 10);

    // Sorting and paging cannot reach them either: every page of her list is hers, and a page
    // past her own rows is empty although others' rows exist there.
    let mut seen = Vec::new();
    for offset in 0..6 {
        let query = ListQuery {
            sort: vec![Sort::desc("owner"), Sort::asc("qty")],
            limit: Some(2),
            offset: Some(offset * 2),
            ..ListQuery::default()
        };
        seen.extend(titles(&db, &reg, &ann, &query).await);
    }
    assert_eq!(seen.len(), 10);
    assert!(seen.iter().all(|t| t.starts_with("ann")));
    let past = ListQuery {
        limit: Some(500),
        offset: Some(10),
        ..ListQuery::default()
    };
    assert!(titles(&db, &reg, &ann, &past).await.is_empty());
    // Bob sees his twenty, an administrator and the system see all thirty.
    assert_eq!(
        titles(
            &db,
            &reg,
            &bob,
            &ListQuery {
                limit: Some(500),
                ..ListQuery::default()
            }
        )
        .await
        .len(),
        20
    );
    let all = ListQuery {
        limit: Some(500),
        ..ListQuery::default()
    };
    assert_eq!(
        titles(&db, &reg, &user("root", &[ADMINISTRATOR]), &all)
            .await
            .len(),
        30
    );
    assert_eq!(titles(&db, &reg, &sys(), &all).await.len(), 30);
    // `Clerk` reads everything (no `if_owner`); a union of roles takes the wider access.
    assert_eq!(
        titles(&db, &reg, &user("c", &["Clerk"]), &all).await.len(),
        30
    );
    assert_eq!(
        titles(&db, &reg, &user("c", &["Own", "Clerk"]), &all)
            .await
            .len(),
        30
    );
}

#[tokio::test]
async fn a_caller_without_read_access_cannot_list() {
    let (db, reg) = setup().await;
    let client = db.pool.get().await.unwrap();
    for roles in [&[][..], &["Nobody"], &["own"]] {
        let error = list(
            &**client,
            &reg,
            "item",
            &user("u", roles),
            &ListQuery::default(),
        )
        .await
        .unwrap_err();
        assert!(
            matches!(&error, DbError::PermissionDenied { doctype, .. } if doctype == "item"),
            "{error:?}"
        );
    }
    // `quote` lets `Clerk` read but `Own` has no rule at all.
    let error = list(
        &**client,
        &reg,
        "quote",
        &user("u", &["Own"]),
        &ListQuery::default(),
    )
    .await
    .unwrap_err();
    assert!(
        matches!(error, DbError::PermissionDenied { .. }),
        "{error:?}"
    );
}

// ---- Hostile input -------------------------------------------------------------------------

#[tokio::test]
async fn hostile_names_never_reach_the_sql() {
    let (db, reg) = setup().await;
    seed(&db, &reg).await;
    let client = db.pool.get().await.unwrap();
    let nasty = [
        "name; drop table doc_item",
        "title\"; DROP TABLE doc_item; --",
        "title --",
        "'; select 1; --",
        "qty, (select 1)",
        "doc_item.qty",
        "\"qty\"",
        "id",
        "amended_from",
        "TITLE",
        " title",
        "title ",
        "",
        "\0",
        "ti\ntle",
    ];
    for bad in nasty {
        for query in [
            q(vec![f(bad, FilterOp::IsNull(true))]),
            q(vec![f(bad, FilterOp::Eq(Value::Int(1)))]),
            sorted(vec![], vec![Sort::asc(bad)]),
            sorted(vec![], vec![Sort::desc(bad)]),
        ] {
            let error = list(&**client, &reg, "item", &sys(), &query)
                .await
                .unwrap_err();
            assert!(
                matches!(&error, DbError::InvalidQuery(m) if m.contains("unknown field")),
                "{bad:?}: {error:?}"
            );
        }
    }
    // Hostile DocType names: nothing is looked up or run.
    for bad in [
        "item; drop table doc_item",
        "item\"",
        "doc_item",
        "ITEM",
        "",
        "item --",
    ] {
        let error = list(&**client, &reg, bad, &sys(), &ListQuery::default())
            .await
            .unwrap_err();
        assert!(
            matches!(error, DbError::UnknownDocType { .. }),
            "{bad:?}: {error:?}"
        );
    }
    let error = list(&**client, &reg, "item_line", &sys(), &ListQuery::default())
        .await
        .unwrap_err();
    assert!(matches!(error, DbError::ChildDocType { .. }), "{error:?}");
    // The table is still there.
    assert_eq!(
        titles(&db, &reg, &sys(), &ListQuery::default()).await.len(),
        5
    );
}

#[tokio::test]
async fn hostile_values_are_stored_and_compared_literally() {
    let (db, reg) = setup().await;
    let nasty = [
        "x'); DROP TABLE doc_item; --",
        "\"; DROP TABLE doc_item; --",
        "' OR '1'='1",
        "$1 $2 ${x} \\' %s",
        "50% off_ _",
    ];
    for t in nasty {
        add(&db, &reg, &sys(), vec![("title", text(t))]).await;
    }
    add(&db, &reg, &sys(), vec![("title", text("plain"))]).await;
    for t in nasty {
        let found = titles(
            &db,
            &reg,
            &sys(),
            &q(vec![f("title", FilterOp::Eq(text(t)))]),
        )
        .await;
        assert_eq!(found, [t], "{t}");
        let found = titles(
            &db,
            &reg,
            &sys(),
            &q(vec![f("title", FilterOp::In(vec![text(t), text("zzz")]))]),
        )
        .await;
        assert_eq!(found, [t], "{t}");
    }
    // The injection attempt as a value matches nothing else, and as an owner scope it is inert.
    let none = titles(
        &db,
        &reg,
        &sys(),
        &q(vec![f("title", FilterOp::Eq(text("' OR 1=1 --")))]),
    )
    .await;
    assert!(none.is_empty());
    let hostile_user = user("x' OR '1'='1", &["Own"]);
    assert!(
        titles(&db, &reg, &hostile_user, &ListQuery::default())
            .await
            .is_empty()
    );
    let made = add(&db, &reg, &hostile_user, vec![("title", text("mine"))]).await;
    assert_eq!(
        titles(&db, &reg, &hostile_user, &ListQuery::default()).await,
        ["mine"]
    );
    let mut client = db.pool.get().await.unwrap();
    let stored = get(&mut **client, &reg, "item", &made, &hostile_user)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(stored.owner, "x' OR '1'='1");
    // And the table survived all of it.
    assert_eq!(
        titles(
            &db,
            &reg,
            &sys(),
            &ListQuery {
                limit: Some(500),
                ..ListQuery::default()
            }
        )
        .await
        .len(),
        7
    );
    // Cleanup path of a delete still works for a Clerk (smoke test of the combined flow).
    let clerk = user("c", &["Clerk"]);
    delete(&mut **client, &reg, "item", &made, 1, &clerk)
        .await
        .unwrap();
}
