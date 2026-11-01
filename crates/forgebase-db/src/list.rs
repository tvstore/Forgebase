//! The list query: filter, sort and page the documents of one DocType (M3.2, ADR 0011).
//!
//! A [`ListQuery`] is a typed tree, not text: fields are looked up in the DocType's metadata (an
//! unknown field is an error and is never put into SQL), identifiers are quoted, and **every
//! value is a bound parameter** with an explicit cast taken from the field type. The caller's
//! read access is part of the same statement (`WHERE "owner" = $1` for an `if_owner` read, and the
//! conditions of the caller's user permissions, M10.3, ADR 0025): rows the caller cannot read are
//! never fetched, so no filter, sort or page can reveal one.
//!
//! # Operators
//!
//! | Operator | Fields |
//! |---|---|
//! | [`FilterOp::Eq`], [`Ne`](FilterOp::Ne), [`In`](FilterOp::In) | every column field and standard column; a Link field takes the target's `name` |
//! | [`Lt`](FilterOp::Lt), [`Le`](FilterOp::Le), [`Gt`](FilterOp::Gt), [`Ge`](FilterOp::Ge) | Data, Text, Select, Int, Decimal, Date, Datetime and the text, time and number standard columns (not Check, not Link) |
//! | [`Like`](FilterOp::Like) | text only: Data, Text, Select, `name`, `owner`, `modified_by`, `workflow_state` (a SQL `LIKE` pattern: `%` any text, `_` one character, `\` escapes; case-sensitive) |
//! | [`IsNull`](FilterOp::IsNull) | every column field and standard column |
//!
//! `Ne` and the comparisons leave out rows where the field is null (SQL semantics); use
//! `IsNull(true)` for those. Filters are combined with AND.
//!
//! # Fields
//!
//! Filterable and sortable: every column field of the DocType (not Table fields, whose rows are
//! not listed) and the standard columns `name`, `owner`, `modified_by`, `created_at`,
//! `modified_at`, `version` and, for a submittable DocType, `docstatus` (an integer, 0, 1 or 2) and,
//! for a DocType with a workflow, `workflow_state` (the name of the state; text, so `like` works).
//! Sorting by a Link field sorts by the target's `name`. Nulls sort as PostgreSQL does: last in
//! ascending, first in descending order.
//!
//! # Paging
//!
//! `limit` defaults to [`DEFAULT_LIMIT`] (20) and may be at most [`MAX_LIMIT`] (500): a larger
//! value is an error, not silently reduced; 0 is an error too. `offset` defaults to 0. The sort
//! always ends with `name` ascending (it is unique), so the order is total and paging is stable.

use forgebase_core::ident::table_name;
use forgebase_core::meta::{DocType, FieldType, Registry};
use forgebase_core::permission::{Caller, field_access, readable_doctype};
use forgebase_core::value::Value;
use time::UtcOffset;
use tokio_postgres::GenericClient;
use tokio_postgres::types::ToSql;

use crate::document::{StoredDocument, decode_row, document_doctype, read_scope, select_list};
use crate::error::DbError;
use crate::ident::quote_ident;
use crate::scope::ReadScope;
use crate::value::{SqlValue, cast_type};

/// The number of documents [`list`] returns when [`ListQuery::limit`] is `None`.
pub const DEFAULT_LIMIT: u32 = 20;
/// The largest allowed [`ListQuery::limit`].
pub const MAX_LIMIT: u32 = 500;
/// The most values an [`FilterOp::In`] filter may have.
pub const MAX_IN_VALUES: usize = 500;

/// What [`list`] selects: filters (AND), a sort order and a page.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct ListQuery {
    /// Conditions every returned document meets.
    pub filters: Vec<Filter>,
    /// Sort keys, most significant first. `name` ascending is always appended.
    pub sort: Vec<Sort>,
    /// Page size: at most [`MAX_LIMIT`], default [`DEFAULT_LIMIT`].
    pub limit: Option<u32>,
    /// Documents to skip, default 0.
    pub offset: Option<u64>,
}

/// One filter: a field and a condition on it.
#[derive(Debug, Clone, PartialEq)]
pub struct Filter {
    /// A field name or standard column, see the module documentation.
    pub field: String,
    /// The condition.
    pub op: FilterOp,
}

impl Filter {
    /// A filter on `field`.
    pub fn new(field: impl Into<String>, op: FilterOp) -> Self {
        Self {
            field: field.into(),
            op,
        }
    }
}

/// A condition on a field. Operand values must have the kind of the field type (a
/// [`Value::Text`] for Data, Text, Select and Link, a [`Value::Int`] for Int, ...) and are never
/// [`Value::Null`] (use [`FilterOp::IsNull`]).
#[derive(Debug, Clone, PartialEq)]
pub enum FilterOp {
    /// `=`.
    Eq(Value),
    /// `<>`.
    Ne(Value),
    /// `<`.
    Lt(Value),
    /// `<=`.
    Le(Value),
    /// `>`.
    Gt(Value),
    /// `>=`.
    Ge(Value),
    /// One of the values (1 to [`MAX_IN_VALUES`]).
    In(Vec<Value>),
    /// A SQL `LIKE` pattern, text fields only.
    Like(String),
    /// `IS NULL` with `true`, `IS NOT NULL` with `false`.
    IsNull(bool),
}

/// One sort key.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Sort {
    /// A field name or standard column that is not a Table field.
    pub field: String,
    /// Descending order.
    pub descending: bool,
}

impl Sort {
    /// Ascending by `field`.
    pub fn asc(field: impl Into<String>) -> Self {
        Self {
            field: field.into(),
            descending: false,
        }
    }

    /// Descending by `field`.
    pub fn desc(field: impl Into<String>) -> Self {
        Self {
            field: field.into(),
            descending: true,
        }
    }
}

/// Returns the documents of `doctype` that the caller may read and that meet the query.
///
/// The caller needs the **read** right: none at all is [`DbError::PermissionDenied`] (that a
/// DocType exists is not secret, and the answer does not depend on any document). With an
/// `if_owner` read the statement is limited to the caller's own documents in SQL. An invalid
/// query is [`DbError::InvalidQuery`]; a name that is not a DocType is
/// [`DbError::UnknownDocType`] (nothing is put into SQL before it is found in the registry).
///
/// The documents are as [`get`](crate::get) returns them (without the fields the caller cannot
/// read, which a filter or sort may not name either: they are unknown, ADR 0024), except that
/// `tables` is empty: the rows of child tables are not listed. Link values are the targets' names. One statement, so the
/// page is consistent without a transaction.
pub async fn list(
    client: &impl GenericClient,
    registry: &Registry,
    doctype: &str,
    caller: &Caller,
    query: &ListQuery,
) -> Result<Vec<StoredDocument>, DbError> {
    let doctype = document_doctype(registry, doctype)?;
    let scope = read_scope(doctype, caller)?;
    // A field the caller cannot read is unknown to the query (ADR 0024).
    let doctype = &*readable_doctype(doctype, field_access(doctype, caller));
    let (sql, binds) = build(doctype, &scope, query)?;
    let params: Vec<SqlValue<'_>> = binds.iter().map(SqlValue).collect();
    let params: Vec<&(dyn ToSql + Sync)> =
        params.iter().map(|p| p as &(dyn ToSql + Sync)).collect();
    client
        .query(&sql, &params)
        .await?
        .iter()
        .map(|row| decode_row(doctype, row))
        .collect()
}

/// Checks that `query` can be run on `doctype` (fields, operators, operand kinds, limit and
/// offset) without running it. Saved views use it, so a view is validated by the very code
/// that later runs it.
pub(crate) fn validate(doctype: &DocType, query: &ListQuery) -> Result<(), DbError> {
    build(doctype, &ReadScope::default(), query).map(|_| ())
}

/// The type of a field the list query can filter or sort by (a column field or a standard
/// column), or `None` for any other name, a Table field included. Lets callers map input values
/// to the right [`Value`] kind.
pub fn column_type(doctype: &DocType, field: &str) -> Option<FieldType> {
    column(doctype, field).ok().map(|c| c.field_type)
}

/// A filterable column: its qualified SQL expression and its type.
pub(crate) struct Column {
    /// `"doc_x"."field"`, quoted.
    pub(crate) expr: String,
    pub(crate) field_type: FieldType,
}

pub(crate) fn invalid(message: String) -> DbError {
    DbError::InvalidQuery(message)
}

/// Looks `field` up in the metadata. Only names found here ever reach the SQL, quoted.
pub(crate) fn column(doctype: &DocType, field: &str) -> Result<Column, DbError> {
    let table = quote_ident(&table_name(&doctype.name));
    let standard = match field {
        "name" | "owner" | "modified_by" => Some(FieldType::Text),
        "created_at" | "modified_at" => Some(FieldType::Datetime),
        "version" => Some(FieldType::Int),
        "docstatus" if doctype.submittable => Some(FieldType::Int),
        // A text column of a DocType with a workflow (M8.1); filter and sort by the state name.
        "workflow_state" if doctype.workflow.is_some() => Some(FieldType::Text),
        _ => None,
    };
    let field_type = match (standard, doctype.field(field)) {
        (Some(field_type), _) => field_type,
        (None, Some(f)) if matches!(f.field_type, FieldType::Table { .. }) => {
            return Err(invalid(format!(
                "field {field:?} is a table field: its rows cannot be filtered or sorted"
            )));
        }
        (None, Some(f)) => f.field_type.clone(),
        (None, None) => {
            return Err(invalid(format!(
                "unknown field {:?} for DocType `{}`",
                forgebase_core::shorten(field),
                doctype.name
            )));
        }
    };
    Ok(Column {
        expr: format!("{table}.{}", quote_ident(field)),
        field_type,
    })
}

/// Collects the bound values and hands out numbered placeholders with a cast.
#[derive(Default)]
pub(crate) struct Binds(pub(crate) Vec<Value>);

impl Binds {
    pub(crate) fn add(&mut self, value: Value, cast: &str) -> String {
        self.0.push(value);
        format!("${}::{cast}", self.0.len())
    }
}

/// The `WHERE` conditions of a read: the read scope first (the owner and the user permissions,
/// all bound, so nothing can be injected), then one per filter. The list, the group-by summary
/// and the export build theirs here.
pub(crate) fn conditions(
    doctype: &DocType,
    scope: &ReadScope<'_>,
    filters: &[Filter],
    binds: &mut Binds,
) -> Result<Vec<String>, DbError> {
    let table = quote_ident(&table_name(&doctype.name));
    let mut conditions = scope.conditions(Some(&table), &mut |value, cast| {
        binds.add(Value::Text(value), cast)
    });
    for filter in filters {
        let col = column(doctype, &filter.field)?;
        conditions.push(condition(binds, &filter.field, &col, &filter.op)?);
    }
    Ok(conditions)
}

/// Builds the statement and its bound values. `scope` limits it to what the caller may read.
fn build(
    doctype: &DocType,
    scope: &ReadScope<'_>,
    query: &ListQuery,
) -> Result<(String, Vec<Value>), DbError> {
    let limit = match query.limit {
        None => DEFAULT_LIMIT,
        Some(0) => return Err(invalid("limit must be at least 1".to_owned())),
        Some(n) if n > MAX_LIMIT => {
            return Err(invalid(format!(
                "limit {n} is above the maximum of {MAX_LIMIT}"
            )));
        }
        Some(n) => n,
    };
    let offset = i64::try_from(query.offset.unwrap_or(0))
        .map_err(|_| invalid("offset is too large".to_owned()))?;

    let table = quote_ident(&table_name(&doctype.name));
    let mut binds = Binds::default();
    let conditions = conditions(doctype, scope, &query.filters, &mut binds)?;

    let order = order_keys(doctype, &query.sort)?;

    let mut sql = format!("SELECT {} FROM {table}", select_list(doctype));
    if !conditions.is_empty() {
        sql.push_str(" WHERE ");
        sql.push_str(&conditions.join(" AND "));
    }
    sql.push_str(" ORDER BY ");
    sql.push_str(&order.join(", "));
    let limit = binds.add(Value::Int(i64::from(limit)), "bigint");
    let offset = binds.add(Value::Int(offset), "bigint");
    sql.push_str(&format!(" LIMIT {limit} OFFSET {offset}"));
    Ok((sql, binds.0))
}

/// The `ORDER BY` keys of a sort: one per sort key, then `name` ascending (unique, so the order
/// is total). A Link column sorts by the target's `name`.
fn order_keys(doctype: &DocType, sorts: &[Sort]) -> Result<Vec<String>, DbError> {
    let table = quote_ident(&table_name(&doctype.name));
    let mut order = Vec::new();
    for sort in sorts {
        let col = column(doctype, &sort.field)?;
        let key = match &col.field_type {
            // A Link column holds the target's id; order by what the user sees, its name.
            FieldType::Link { target } => format!(
                "(SELECT t.\"name\" FROM {} t WHERE t.\"id\" = {})",
                quote_ident(&table_name(target)),
                col.expr
            ),
            _ => col.expr,
        };
        order.push(format!(
            "{key} {}",
            if sort.descending { "DESC" } else { "ASC" }
        ));
    }
    order.push(format!("{table}.\"name\" ASC"));
    Ok(order)
}

/// The statements of an export (M9.3): the rows (filters and sort, no page) and their count,
/// which share one list of bound values. Built from the same conditions and sort as [`list`].
pub(crate) fn build_export(
    doctype: &DocType,
    scope: &ReadScope<'_>,
    filters: &[Filter],
    sort: &[Sort],
) -> Result<(String, String, Vec<Value>), DbError> {
    let table = quote_ident(&table_name(&doctype.name));
    let mut binds = Binds::default();
    let conditions = conditions(doctype, scope, filters, &mut binds)?;
    let order = order_keys(doctype, sort)?;
    let filter = if conditions.is_empty() {
        String::new()
    } else {
        format!(" WHERE {}", conditions.join(" AND "))
    };
    let rows = format!(
        "SELECT {} FROM {table}{filter} ORDER BY {}",
        select_list(doctype),
        order.join(", ")
    );
    let count = format!("SELECT count(*) FROM {table}{filter}");
    Ok((rows, count, binds.0))
}

/// The SQL condition of one filter; values are bound.
fn condition(
    binds: &mut Binds,
    field: &str,
    col: &Column,
    op: &FilterOp,
) -> Result<String, DbError> {
    let ft = &col.field_type;
    let expr = &col.expr;
    let cast = cast_type(ft);
    let link = match ft {
        FieldType::Link { target } => Some(quote_ident(&table_name(target))),
        _ => None,
    };
    match op {
        FilterOp::IsNull(true) => Ok(format!("{expr} IS NULL")),
        FilterOp::IsNull(false) => Ok(format!("{expr} IS NOT NULL")),
        FilterOp::Eq(v) | FilterOp::Ne(v) => {
            let sql_op = if matches!(op, FilterOp::Eq(_)) {
                "="
            } else {
                "<>"
            };
            check_operand(field, ft, v)?;
            Ok(match &link {
                // The target's id for a name; a name that does not exist matches nothing for
                // `=` and every non-null link for `<>`.
                Some(target) => {
                    let p = binds.add(v.clone(), "text");
                    let sub = format!("(SELECT t.\"id\" FROM {target} t WHERE t.\"name\" = {p})");
                    if sql_op == "=" {
                        format!("{expr} IN {sub}")
                    } else {
                        // `NOT IN` an empty set is true even for NULL: leave unlinked rows out
                        // explicitly, as `<>` does for every other field.
                        format!("({expr} IS NOT NULL AND {expr} NOT IN {sub})")
                    }
                }
                None => {
                    let p = binds.add(v.clone(), cast);
                    format!("{expr} {sql_op} {p}")
                }
            })
        }
        FilterOp::Lt(v) | FilterOp::Le(v) | FilterOp::Gt(v) | FilterOp::Ge(v) => {
            let (sql_op, name) = match op {
                FilterOp::Lt(_) => ("<", "<"),
                FilterOp::Le(_) => ("<=", "<="),
                FilterOp::Gt(_) => (">", ">"),
                _ => (">=", ">="),
            };
            if matches!(ft, FieldType::Check | FieldType::Link { .. }) {
                return Err(invalid(format!(
                    "operator {name} does not apply to {} field {field:?}",
                    ft.name()
                )));
            }
            check_operand(field, ft, v)?;
            let p = binds.add(v.clone(), cast);
            Ok(format!("{expr} {sql_op} {p}"))
        }
        FilterOp::In(values) => {
            if values.is_empty() || values.len() > MAX_IN_VALUES {
                return Err(invalid(format!(
                    "`in` on field {field:?} needs 1 to {MAX_IN_VALUES} values, got {}",
                    values.len()
                )));
            }
            let mut placeholders = Vec::with_capacity(values.len());
            for v in values {
                check_operand(field, ft, v)?;
                let cast = if link.is_some() { "text" } else { cast };
                placeholders.push(binds.add(v.clone(), cast));
            }
            let list = placeholders.join(", ");
            Ok(match &link {
                Some(target) => format!(
                    "{expr} IN (SELECT t.\"id\" FROM {target} t WHERE t.\"name\" IN ({list}))"
                ),
                None => format!("{expr} IN ({list})"),
            })
        }
        FilterOp::Like(pattern) => {
            if !matches!(
                ft,
                FieldType::Data { .. } | FieldType::Text | FieldType::Select { .. }
            ) {
                return Err(invalid(format!(
                    "`like` applies to text fields only, but field {field:?} is {}",
                    ft.name()
                )));
            }
            if pattern.contains('\0') {
                return Err(invalid(format!(
                    "the pattern for field {field:?} must not contain a NUL character"
                )));
            }
            let p = binds.add(Value::Text(pattern.clone()), "text");
            Ok(format!("{expr} LIKE {p}"))
        }
    }
}

/// An operand must be a non-null value of the kind the field type takes.
fn check_operand(field: &str, ft: &FieldType, value: &Value) -> Result<(), DbError> {
    match (ft, value) {
        (_, Value::Null) => Err(invalid(format!(
            "field {field:?}: a filter value cannot be null (use `is null`)"
        ))),
        (
            FieldType::Data { .. }
            | FieldType::Text
            | FieldType::Select { .. }
            | FieldType::Link { .. },
            Value::Text(text),
        ) => {
            if text.contains('\0') {
                Err(invalid(format!(
                    "field {field:?}: a value must not contain a NUL character"
                )))
            } else {
                Ok(())
            }
        }
        (FieldType::Datetime, Value::Datetime(t)) => {
            if t.checked_to_offset(UtcOffset::UTC).is_some() {
                Ok(())
            } else {
                Err(invalid(format!(
                    "field {field:?}: the datetime is out of range once converted to UTC"
                )))
            }
        }
        (FieldType::Int, Value::Int(_))
        | (FieldType::Decimal { .. }, Value::Decimal(_))
        | (FieldType::Check, Value::Bool(_))
        | (FieldType::Date, Value::Date(_)) => Ok(()),
        (ft, value) => Err(invalid(format!(
            "field {field:?} is {}: expected a {} value, found {}",
            ft.name(),
            ft.name(),
            value.kind()
        ))),
    }
}

#[cfg(test)]
mod tests {
    use forgebase_core::permission::{Access, Right, RowScope};

    use super::*;
    use forgebase_core::permission::evaluate;

    fn doctype() -> DocType {
        DocType::from_toml_str(
            "name = \"order\"\nlabel = \"O\"\nsubmittable = true\n\
             [[fields]]\nname = \"title\"\ntype = \"data\"\n\
             [[fields]]\nname = \"qty\"\ntype = \"int\"\n\
             [[fields]]\nname = \"done\"\ntype = \"check\"\n\
             [[fields]]\nname = \"customer\"\ntype = \"link\"\ntarget = \"customer\"\n\
             [[fields]]\nname = \"lines\"\ntype = \"table\"\ntarget = \"line\"\n",
        )
        .expect("valid DocType")
    }

    fn text(s: &str) -> Value {
        Value::Text(s.to_owned())
    }

    fn query(filters: Vec<Filter>) -> ListQuery {
        ListQuery {
            filters,
            ..ListQuery::default()
        }
    }

    fn err(q: &ListQuery) -> String {
        match build(&doctype(), &ReadScope::default(), q) {
            Err(DbError::InvalidQuery(message)) => message,
            other => panic!("expected InvalidQuery, got {other:?}"),
        }
    }

    #[test]
    fn the_default_statement_orders_by_name_and_pages() {
        let (sql, binds) = build(&doctype(), &ReadScope::default(), &ListQuery::default()).unwrap();
        assert!(
            sql.starts_with("SELECT \"id\", \"name\", \"owner\""),
            "{sql}"
        );
        assert!(
            sql.ends_with(
                "FROM \"doc_order\" ORDER BY \"doc_order\".\"name\" ASC LIMIT $1::bigint OFFSET $2::bigint"
            ),
            "{sql}"
        );
        assert_eq!(binds, vec![Value::Int(20), Value::Int(0)]);
    }

    #[test]
    fn an_owner_scope_comes_first_and_is_bound() {
        let q = query(vec![Filter::new("qty", FilterOp::Gt(Value::Int(5)))]);
        let (sql, binds) = build(
            &doctype(),
            &ReadScope::new(Some("ann'; DROP TABLE x; --"), RowScope::default()),
            &q,
        )
        .unwrap();
        assert!(
            sql.contains(
                "WHERE \"doc_order\".\"owner\" = $1::text AND \"doc_order\".\"qty\" > $2::bigint ORDER BY"
            ),
            "{sql}"
        );
        assert!(!sql.contains("DROP"), "{sql}");
        assert_eq!(binds[0], text("ann'; DROP TABLE x; --"));
        assert_eq!(binds[1], Value::Int(5));
    }

    #[test]
    fn user_permissions_are_bound_conditions_before_the_filters() {
        let allowed = vec!["Retail".to_owned(), "a\"b".to_owned()];
        let rows = RowScope {
            names: None,
            links: vec![forgebase_core::permission::LinkScope {
                field: "customer",
                target: "customer",
                allowed: &allowed,
            }],
        };
        let q = query(vec![Filter::new("qty", FilterOp::Gt(Value::Int(5)))]);
        let (sql, binds) = build(&doctype(), &ReadScope::new(Some("ann"), rows), &q).unwrap();
        assert!(
            sql.contains(
                "WHERE \"doc_order\".\"owner\" = $1::text \
                 AND (\"doc_order\".\"customer\" IS NULL OR \"doc_order\".\"customer\" IN \
                 (SELECT t.\"id\" FROM \"doc_customer\" t WHERE t.\"name\" = ANY($2::text::text[]))) \
                 AND \"doc_order\".\"qty\" > $3::bigint ORDER BY"
            ),
            "{sql}"
        );
        assert_eq!(binds[0], text("ann"));
        assert_eq!(binds[1], text("{\"Retail\",\"a\\\"b\"}"));
        assert_eq!(binds[2], Value::Int(5));
        // Without a scope the statement is the plain one.
        let (plain, _) = build(&doctype(), &ReadScope::default(), &q).unwrap();
        assert!(
            !plain.contains("ANY") && !plain.contains("\"owner\" ="),
            "{plain}"
        );
    }

    #[test]
    fn every_operator_binds_its_values_with_a_cast() {
        let q = query(vec![
            Filter::new("title", FilterOp::Eq(text("a"))),
            Filter::new("title", FilterOp::Ne(text("b"))),
            Filter::new("qty", FilterOp::Lt(Value::Int(1))),
            Filter::new("qty", FilterOp::Le(Value::Int(2))),
            Filter::new("qty", FilterOp::Ge(Value::Int(3))),
            Filter::new("qty", FilterOp::In(vec![Value::Int(4), Value::Int(5)])),
            Filter::new("title", FilterOp::Like("a%".into())),
            Filter::new("title", FilterOp::IsNull(true)),
            Filter::new("version", FilterOp::IsNull(false)),
            Filter::new("docstatus", FilterOp::Eq(Value::Int(1))),
        ]);
        let (sql, binds) = build(&doctype(), &ReadScope::default(), &q).unwrap();
        let t = "\"doc_order\"";
        let expected = format!(
            "WHERE {t}.\"title\" = $1::varchar AND {t}.\"title\" <> $2::varchar \
             AND {t}.\"qty\" < $3::bigint AND {t}.\"qty\" <= $4::bigint AND {t}.\"qty\" >= $5::bigint \
             AND {t}.\"qty\" IN ($6::bigint, $7::bigint) AND {t}.\"title\" LIKE $8::text \
             AND {t}.\"title\" IS NULL AND {t}.\"version\" IS NOT NULL AND {t}.\"docstatus\" = $9::bigint ORDER BY"
        );
        assert!(sql.contains(&expected), "{sql}");
        assert_eq!(binds.len(), 9 + 2);
    }

    #[test]
    fn a_link_filter_compares_through_a_bound_subquery_on_the_target() {
        let q = query(vec![
            Filter::new("customer", FilterOp::Eq(text("acme"))),
            Filter::new("customer", FilterOp::Ne(text("zed"))),
            Filter::new("customer", FilterOp::In(vec![text("a"), text("b")])),
            Filter::new("customer", FilterOp::IsNull(true)),
        ]);
        let (sql, _) = build(&doctype(), &ReadScope::default(), &q).unwrap();
        let c = "\"doc_order\".\"customer\"";
        let sub = "(SELECT t.\"id\" FROM \"doc_customer\" t WHERE t.\"name\"";
        assert!(sql.contains(&format!("{c} IN {sub} = $1::text)")), "{sql}");
        assert!(
            sql.contains(&format!("{c} NOT IN {sub} = $2::text)")),
            "{sql}"
        );
        assert!(
            sql.contains(&format!("{c} IN {sub} IN ($3::text, $4::text))")),
            "{sql}"
        );
        assert!(sql.contains(&format!("{c} IS NULL")), "{sql}");
    }

    #[test]
    fn sorting_is_multi_field_with_name_last_and_links_sort_by_name() {
        let q = ListQuery {
            sort: vec![
                Sort::desc("qty"),
                Sort::asc("customer"),
                Sort::desc("owner"),
            ],
            ..ListQuery::default()
        };
        let (sql, _) = build(&doctype(), &ReadScope::default(), &q).unwrap();
        assert!(
            sql.contains(
                "ORDER BY \"doc_order\".\"qty\" DESC, \
                 (SELECT t.\"name\" FROM \"doc_customer\" t WHERE t.\"id\" = \"doc_order\".\"customer\") ASC, \
                 \"doc_order\".\"owner\" DESC, \"doc_order\".\"name\" ASC LIMIT"
            ),
            "{sql}"
        );
    }

    #[test]
    fn identifiers_come_from_metadata_only() {
        for bad in [
            "name; drop table doc_order",
            "title\"",
            "--",
            "qty or 1=1",
            "",
            "id",
            "amended_from",
            "NAME",
            "doc_order.name",
        ] {
            let message = err(&query(vec![Filter::new(bad, FilterOp::IsNull(true))]));
            assert!(message.contains("unknown field"), "{bad}: {message}");
            let q = ListQuery {
                sort: vec![Sort::asc(bad)],
                ..ListQuery::default()
            };
            assert!(err(&q).contains("unknown field"), "{bad}");
        }
        // The text of a hostile name appears escaped in the message, never in SQL.
        let message = err(&query(vec![Filter::new("a\nb", FilterOp::IsNull(true))]));
        assert!(message.contains("\\n"), "{message}");
    }

    #[test]
    fn docstatus_exists_only_on_a_submittable_doctype() {
        let plain = DocType::from_toml_str("name = \"p\"\nlabel = \"P\"\n").unwrap();
        let q = query(vec![Filter::new("docstatus", FilterOp::IsNull(true))]);
        assert!(matches!(
            build(&plain, &ReadScope::default(), &q),
            Err(DbError::InvalidQuery(_))
        ));
        assert!(build(&doctype(), &ReadScope::default(), &q).is_ok());
    }

    #[test]
    fn table_fields_cannot_be_filtered_or_sorted() {
        let message = err(&query(vec![Filter::new("lines", FilterOp::IsNull(true))]));
        assert!(message.contains("table field"), "{message}");
        let q = ListQuery {
            sort: vec![Sort::asc("lines")],
            ..ListQuery::default()
        };
        assert!(err(&q).contains("table field"));
    }

    #[test]
    fn operators_must_fit_the_field() {
        let cases = [
            (
                Filter::new("qty", FilterOp::Like("1%".into())),
                "text fields only",
            ),
            (
                Filter::new("done", FilterOp::Like("t%".into())),
                "text fields only",
            ),
            (
                Filter::new("customer", FilterOp::Like("a%".into())),
                "text fields only",
            ),
            (
                Filter::new("version", FilterOp::Like("1%".into())),
                "text fields only",
            ),
            (
                Filter::new("done", FilterOp::Lt(Value::Bool(true))),
                "does not apply",
            ),
            (
                Filter::new("customer", FilterOp::Gt(text("a"))),
                "does not apply",
            ),
            (
                Filter::new("qty", FilterOp::Eq(text("1"))),
                "expected a int value",
            ),
            (
                Filter::new("qty", FilterOp::Eq(Value::Null)),
                "cannot be null",
            ),
            (
                Filter::new("title", FilterOp::Eq(Value::Int(1))),
                "expected a data value",
            ),
            (Filter::new("title", FilterOp::Eq(text("a\0b"))), "NUL"),
            (Filter::new("title", FilterOp::Like("a\0".into())), "NUL"),
            (Filter::new("qty", FilterOp::In(vec![])), "needs 1 to"),
            (
                Filter::new("qty", FilterOp::In(vec![Value::Int(1), text("x")])),
                "expected",
            ),
            (
                Filter::new("created_at", FilterOp::Gt(Value::Int(1))),
                "expected a datetime",
            ),
        ];
        for (filter, needle) in cases {
            let message = err(&query(vec![filter.clone()]));
            assert!(message.contains(needle), "{filter:?}: {message}");
        }
        let many = vec![Value::Int(1); MAX_IN_VALUES + 1];
        assert!(err(&query(vec![Filter::new("qty", FilterOp::In(many))])).contains("needs 1 to"));
        let max = vec![Value::Int(1); MAX_IN_VALUES];
        assert!(
            build(
                &doctype(),
                &ReadScope::default(),
                &query(vec![Filter::new("qty", FilterOp::In(max))])
            )
            .is_ok()
        );
    }

    #[test]
    fn limit_and_offset_are_checked_not_clamped() {
        let with = |limit, offset| ListQuery {
            limit,
            offset,
            ..ListQuery::default()
        };
        assert!(err(&with(Some(0), None)).contains("at least 1"));
        assert!(err(&with(Some(501), None)).contains("maximum of 500"));
        assert!(err(&with(Some(u32::MAX), None)).contains("maximum"));
        assert!(err(&with(None, Some(u64::MAX))).contains("offset"));
        let (_, binds) =
            build(&doctype(), &ReadScope::default(), &with(Some(500), Some(7))).unwrap();
        assert_eq!(binds, vec![Value::Int(500), Value::Int(7)]);
        let (_, binds) = build(
            &doctype(),
            &ReadScope::default(),
            &with(Some(1), Some(i64::MAX as u64)),
        )
        .unwrap();
        assert_eq!(binds[1], Value::Int(i64::MAX));
    }

    #[test]
    fn column_types() {
        let d = doctype();
        assert_eq!(column_type(&d, "qty"), Some(FieldType::Int));
        assert_eq!(column_type(&d, "owner"), Some(FieldType::Text));
        assert_eq!(column_type(&d, "created_at"), Some(FieldType::Datetime));
        assert_eq!(column_type(&d, "docstatus"), Some(FieldType::Int));
        assert_eq!(column_type(&d, "lines"), None);
        assert_eq!(column_type(&d, "nope"), None);
    }

    #[test]
    fn the_evaluator_is_the_only_source_of_the_owner_scope() {
        // `list` derives the scope from `evaluate`; check the mapping it relies on.
        let d = doctype();
        assert_eq!(
            evaluate(&d, &Caller::system("cli"), Right::Read),
            Access::All
        );
        assert_eq!(
            evaluate(&d, &Caller::user("u", vec![]), Right::Read),
            Access::Denied
        );
    }
}
