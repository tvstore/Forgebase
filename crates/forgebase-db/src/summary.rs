//! The group-by summary of a list (M8.3, architecture §11): documents grouped by one field, with
//! a count and optional sum, average, minimum and maximum per group.
//!
//! It is a read like [`list`](crate::list), and takes **everything that decides what is read
//! from the list**: the read scope ([`read_scope`], so an `if_owner` read counts the caller's own
//! documents only and a missing read right is [`DbError::PermissionDenied`]) and the filters
//! ([`conditions`], so a filter is checked and bound exactly as in a list). No second
//! permission check exists. A summary gives no right: it sees only rows the list sees.
//!
//! # Fields
//!
//! - **Group by** one of `owner`, `modified_by`, `docstatus` (submittable DocTypes),
//!   `workflow_state` (DocTypes with a workflow), or a Data, Select, Link, Check, Date or Int
//!   field. A Link groups by the target's `name`, as a filter takes it. Not Text, Table, Decimal
//!   or Datetime fields, and not `name`, `created_at`, `modified_at` or `version` (nearly every
//!   document would be a group of its own). Documents with no value form one group whose key is
//!   [`Value::Null`].
//! - **Aggregates** `count` is always there. `sum`, `avg`, `min` and `max` apply to Int and
//!   Decimal fields; `min` and `max` also to Date fields. At most [`MAX_AGGREGATES`], none twice.
//!   The result key is `<function>_<field>`. `sum` and `min`/`max` of an Int are integers (a sum
//!   beyond 64 bits is a decimal); a Decimal field's results are decimals; `avg` is a decimal
//!   rounded to the field's scale plus 2 digits (6 digits for an Int). Groups where every value
//!   is null have a null aggregate.
//!
//! Groups are ordered by `count` descending, then by key (byte order for text, nulls last), and
//! at most [`MAX_GROUPS`] are returned; `truncated` says there were more. A child DocType is
//! refused, as in a list.

use std::collections::BTreeMap;

use forgebase_core::ident::table_name;
use forgebase_core::meta::{DocType, FieldType, Registry};
use forgebase_core::permission::{Caller, field_access, readable_doctype};
use forgebase_core::value::Value;
use rust_decimal::Decimal;
use tokio_postgres::GenericClient;
use tokio_postgres::types::ToSql;

use crate::document::document_doctype;
use crate::document::read_scope;
use crate::error::DbError;
use crate::ident::quote_ident;
use crate::list::{Binds, Filter, column, conditions, invalid};
use crate::scope::ReadScope;
use crate::value::{SqlValue, decode};

/// The most groups a summary returns.
pub const MAX_GROUPS: usize = 500;
/// The most aggregates besides the count.
pub const MAX_AGGREGATES: usize = 6;

/// An aggregate function (the count is always included and is not listed).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AggregateFn {
    /// The total of an Int or Decimal field.
    Sum,
    /// The average of an Int or Decimal field.
    Avg,
    /// The smallest value of an Int, Decimal or Date field.
    Min,
    /// The largest value of an Int, Decimal or Date field.
    Max,
}

impl AggregateFn {
    /// `sum`, `avg`, `min` or `max`: the prefix of the result key.
    pub fn name(self) -> &'static str {
        match self {
            AggregateFn::Sum => "sum",
            AggregateFn::Avg => "avg",
            AggregateFn::Min => "min",
            AggregateFn::Max => "max",
        }
    }

    /// The function with this name.
    pub fn from_name(name: &str) -> Option<Self> {
        [Self::Sum, Self::Avg, Self::Min, Self::Max]
            .into_iter()
            .find(|f| f.name() == name)
    }
}

/// What [`summary`] computes.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct SummaryQuery {
    /// Conditions every counted document meets (as in a list).
    pub filters: Vec<Filter>,
    /// The field to group by, see the module documentation.
    pub group_by: String,
    /// The aggregates besides the count: a function and a field.
    pub aggregates: Vec<(AggregateFn, String)>,
}

/// One group.
#[derive(Debug, Clone, PartialEq)]
pub struct Group {
    /// The value of the group-by field; [`Value::Null`] for documents without one.
    pub key: Value,
    /// The number of documents.
    pub count: i64,
    /// The aggregates, by `<function>_<field>`.
    pub values: BTreeMap<String, Value>,
}

/// The result of [`summary`].
#[derive(Debug, Clone, PartialEq)]
pub struct Summary {
    /// At most [`MAX_GROUPS`] groups, largest first.
    pub groups: Vec<Group>,
    /// True when there were more groups than are returned.
    pub truncated: bool,
}

/// How an aggregate is read back.
struct Selected {
    name: String,
    field_type: FieldType,
    function: AggregateFn,
}

/// Groups the documents of `doctype` that the caller may read and that meet the filters.
/// See the module documentation. An invalid query is [`DbError::InvalidQuery`]; no read right is
/// [`DbError::PermissionDenied`]; an unknown or child DocType is an error as in a list.
pub async fn summary(
    client: &impl GenericClient,
    registry: &Registry,
    doctype: &str,
    caller: &Caller,
    query: &SummaryQuery,
) -> Result<Summary, DbError> {
    let doctype = document_doctype(registry, doctype)?;
    let scope = read_scope(doctype, caller)?;
    // A group-by, an aggregate or a filter on a field the caller cannot read is an unknown
    // field (ADR 0024).
    let doctype = &*readable_doctype(doctype, field_access(doctype, caller));
    let (sql, binds, key_type, selected) = build(doctype, &scope, query)?;
    let params: Vec<SqlValue<'_>> = binds.iter().map(SqlValue).collect();
    let params: Vec<&(dyn ToSql + Sync)> =
        params.iter().map(|p| p as &(dyn ToSql + Sync)).collect();
    let rows = client.query(&sql, &params).await?;
    let mut groups = Vec::with_capacity(rows.len().min(MAX_GROUPS));
    for row in rows.iter().take(MAX_GROUPS) {
        let mut values = BTreeMap::new();
        for (i, item) in selected.iter().enumerate() {
            values.insert(item.name.clone(), read_aggregate(row, i + 2, item)?);
        }
        groups.push(Group {
            key: decode(row, 0, &key_type)
                .map_err(|e| DbError::decode(&doctype.name, &query.group_by, &e))?,
            count: row.try_get(1)?,
            values,
        });
    }
    Ok(Summary {
        groups,
        truncated: rows.len() > MAX_GROUPS,
    })
}

fn read_aggregate(
    row: &tokio_postgres::Row,
    index: usize,
    item: &Selected,
) -> Result<Value, DbError> {
    match (item.function, &item.field_type) {
        // Computed as numeric in SQL: an Int total is an integer unless it outgrows 64 bits.
        (AggregateFn::Sum, FieldType::Int) => Ok(row
            .try_get::<_, Option<Decimal>>(index)?
            .map_or(Value::Null, |d| {
                i64::try_from(d).map_or(Value::Decimal(d), Value::Int)
            })),
        (AggregateFn::Sum | AggregateFn::Avg, _) => Ok(row
            .try_get::<_, Option<Decimal>>(index)?
            .map_or(Value::Null, Value::Decimal)),
        (AggregateFn::Min | AggregateFn::Max, field_type) => Ok(decode(row, index, field_type)?),
    }
}

/// The group-by column: the SQL of the key, its type and whether it is text (ordered by bytes).
fn group_key(doctype: &DocType, name: &str) -> Result<(String, FieldType, bool), DbError> {
    if matches!(name, "name" | "created_at" | "modified_at" | "version") {
        return Err(invalid(format!(
            "cannot group by {name:?}: almost every document has its own value"
        )));
    }
    let col = column(doctype, name)?;
    // A field is looked up first: a Text field is refused, but the standard text columns
    // (`owner`, `modified_by`, `workflow_state`) are fine.
    let is_field = doctype.field(name).is_some();
    let (expr, text) = match &col.field_type {
        FieldType::Data { .. } | FieldType::Select { .. } => (col.expr, true),
        FieldType::Text if !is_field => (col.expr, true),
        FieldType::Link { target } => (
            format!(
                "(SELECT t.\"name\" FROM {} t WHERE t.\"id\" = {})",
                quote_ident(&table_name(target)),
                col.expr
            ),
            true,
        ),
        FieldType::Int => (format!("{}::bigint", col.expr), false),
        FieldType::Check | FieldType::Date => (col.expr, false),
        other => {
            return Err(invalid(format!(
                "cannot group by {} field {name:?} (use a data, select, link, check, date or int field)",
                other.name()
            )));
        }
    };
    Ok((expr, col.field_type, text))
}

type Built = (String, Vec<Value>, FieldType, Vec<Selected>);

fn build(doctype: &DocType, scope: &ReadScope<'_>, query: &SummaryQuery) -> Result<Built, DbError> {
    let table = quote_ident(&table_name(&doctype.name));
    let (key, key_type, text_key) = group_key(doctype, &query.group_by)?;
    if query.aggregates.len() > MAX_AGGREGATES {
        return Err(invalid(format!(
            "at most {MAX_AGGREGATES} aggregates, got {}",
            query.aggregates.len()
        )));
    }
    let mut items = Vec::new();
    let mut selected: Vec<Selected> = Vec::new();
    for (function, field) in &query.aggregates {
        let name = format!("{}_{field}", function.name());
        if selected.iter().any(|s| s.name == name) {
            return Err(invalid(format!(
                "the aggregate {:?} is listed twice",
                forgebase_core::shorten(&name)
            )));
        }
        // Only fields: the standard columns are not summed.
        let field_type = match doctype.field(field).map(|f| f.field_type.clone()) {
            Some(FieldType::Table { .. }) => {
                return Err(invalid(format!(
                    "field {field:?} is a table field: its rows cannot be summarised"
                )));
            }
            Some(t) => t,
            None => {
                return Err(invalid(format!(
                    "unknown field {:?} for DocType `{}` in {:?}",
                    forgebase_core::shorten(field),
                    doctype.name,
                    forgebase_core::shorten(&name)
                )));
            }
        };
        let fits = matches!(
            (&field_type, function),
            (FieldType::Int | FieldType::Decimal { .. }, _)
                | (FieldType::Date, AggregateFn::Min | AggregateFn::Max)
        );
        if !fits {
            return Err(invalid(format!(
                "{} cannot be applied to {} field {field:?} (sum and avg take int and decimal fields, min and max also date fields)",
                function.name(),
                field_type.name()
            )));
        }
        let expr = column(doctype, field)?.expr;
        items.push(match function {
            AggregateFn::Sum => format!("sum({expr})::numeric"),
            AggregateFn::Avg => {
                let scale = match &field_type {
                    FieldType::Decimal { scale, .. } => u32::from(*scale) + 2,
                    _ => 6,
                };
                format!("round(avg({expr}), {scale})::numeric")
            }
            AggregateFn::Min => format!("min({expr})"),
            AggregateFn::Max => format!("max({expr})"),
        });
        selected.push(Selected {
            name,
            field_type,
            function: *function,
        });
    }

    let mut binds = Binds::default();
    let conditions = conditions(doctype, scope, &query.filters, &mut binds)?;
    let mut select = vec![format!("{key} AS \"key\""), "count(*)".to_owned()];
    select.extend(items);
    let mut sql = format!("SELECT {} FROM {table}", select.join(", "));
    if !conditions.is_empty() {
        sql.push_str(" WHERE ");
        sql.push_str(&conditions.join(" AND "));
    }
    let order_key = if text_key {
        format!("({key}) COLLATE \"C\"")
    } else {
        format!("({key})")
    };
    // One more than the cap, to know that there are more.
    sql.push_str(&format!(
        " GROUP BY 1 ORDER BY count(*) DESC, {order_key} ASC LIMIT {}",
        MAX_GROUPS + 1
    ));
    Ok((sql, binds.0, key_type, selected))
}
