//! The read scope of a statement: the SQL form of what a caller may see of a DocType (M10.3, ADR
//! 0025, next to `if_owner` of ADR 0011).
//!
//! **Nothing is decided here.** [`forgebase_core::permission::evaluate`] says whether the caller
//! reads every document or only their own, and [`forgebase_core::permission::row_scope`] says
//! which documents their user permissions allow; a [`ReadScope`] holds both answers and turns
//! them into `WHERE` conditions. Every statement that finds a document on behalf of a caller
//! (get, list, summary, export, the locking reads, link resolution, link titles) builds its
//! conditions here, so no path can forget one.
//!
//! The conditions are ANDed:
//!
//! - `if_owner`: `"owner" = $n`;
//! - the own name of a restricted DocType: `"name" = ANY($n::text::text[])`;
//! - a Link field to a restricted target:
//!   `("col" IS NULL OR "col" IN (SELECT t."id" FROM "doc_target" t WHERE t."name" = ANY($n::text::text[])))`.
//!
//! A Link column holds the target's `id`, so the subquery goes through the primary key and the
//! unique `name` index of the target. **Every value is a bound parameter**, the allowed names as
//! one array literal in a text parameter (see [`array_literal`]); only validated, quoted
//! identifiers reach the SQL. With no restriction there is no condition, and the statement is
//! exactly what it is without user permissions.

use forgebase_core::ident::table_name;
use forgebase_core::permission::RowScope;

use crate::ident::quote_ident;

/// What a caller may see of one DocType: the `if_owner` owner and the user-permission scope.
#[derive(Debug, Clone, Default)]
pub(crate) struct ReadScope<'a> {
    /// With an `if_owner` read: the owner of the only documents the caller can see.
    pub(crate) owner: Option<&'a str>,
    /// The documents the caller's user permissions allow.
    pub(crate) rows: RowScope<'a>,
}

/// A statement's scope conditions with their bound values, see [`ReadScope::and_sql`].
pub(crate) struct ScopeSql {
    /// Empty, or ` AND <condition> AND <condition> ...`.
    pub(crate) sql: String,
    /// The values of the placeholders the conditions use, in order; all of them text.
    pub(crate) params: Vec<String>,
}

impl<'a> ReadScope<'a> {
    pub(crate) fn new(owner: Option<&'a str>, rows: RowScope<'a>) -> Self {
        Self { owner, rows }
    }

    /// The conditions, owner first, then the own name, then the Links in declaration order.
    /// `table` qualifies the columns of the statement's own table (`None`: unqualified, for a
    /// statement with a single table). `bind` takes a value and the cast it needs and returns
    /// the placeholder to put in the SQL.
    pub(crate) fn conditions(
        &self,
        table: Option<&str>,
        bind: &mut dyn FnMut(String, &str) -> String,
    ) -> Vec<String> {
        let column = |name: &str| match table {
            Some(table) => format!("{table}.{}", quote_ident(name)),
            None => quote_ident(name),
        };
        let mut conditions = Vec::new();
        if let Some(owner) = self.owner {
            let placeholder = bind(owner.to_owned(), "text");
            conditions.push(format!("{} = {placeholder}", column("owner")));
        }
        if let Some(names) = self.rows.names {
            let placeholder = bind(array_literal(names), "text::text[]");
            conditions.push(format!("{} = ANY({placeholder})", column("name")));
        }
        // One parameter per target, even when several fields point at it.
        let mut bound: Vec<(&str, String)> = Vec::new();
        for link in &self.rows.links {
            let placeholder = match bound.iter().find(|(target, _)| *target == link.target) {
                Some((_, placeholder)) => placeholder.clone(),
                None => {
                    let placeholder = bind(array_literal(link.allowed), "text::text[]");
                    bound.push((link.target, placeholder.clone()));
                    placeholder
                }
            };
            let col = column(link.field);
            conditions.push(format!(
                "({col} IS NULL OR {col} IN (SELECT t.\"id\" FROM {} t WHERE t.\"name\" = ANY({placeholder})))",
                quote_ident(&table_name(link.target)),
            ));
        }
        conditions
    }

    /// The conditions as ` AND ...` for a statement that already has a `WHERE`, and the values
    /// to bind after the first `first - 1` parameters (the placeholders are numbered from
    /// `first`). Unqualified columns: only for a statement on a single table.
    pub(crate) fn and_sql(&self, first: usize) -> ScopeSql {
        self.and_sql_in(None, first)
    }

    /// Like [`and_sql`](Self::and_sql) with the columns qualified by `table` (a quoted table
    /// name), for a statement whose `WHERE` has other tables or subqueries around.
    pub(crate) fn and_sql_for(&self, table: &str, first: usize) -> ScopeSql {
        self.and_sql_in(Some(table), first)
    }

    fn and_sql_in(&self, table: Option<&str>, first: usize) -> ScopeSql {
        let mut params: Vec<String> = Vec::new();
        let conditions = self.conditions(table, &mut |value, cast| {
            params.push(value);
            format!("${}::{cast}", first + params.len() - 1)
        });
        let sql = conditions
            .iter()
            .map(|c| format!(" AND {c}"))
            .collect::<String>();
        ScopeSql { sql, params }
    }
}

/// `names` as a PostgreSQL array literal for a text array: `{"a","b"}`. Every element is quoted,
/// with `\` and `"` escaped, so a name with a comma, a brace, a quote, spaces or the text `NULL`
/// is that name and nothing else. The literal is bound as a parameter and cast on the server
/// (`$n::text::text[]`); it is never put into the SQL text. (A text parameter keeps the list in
/// one value that every statement builder of the crate can bind alike.)
pub(crate) fn array_literal(names: &[String]) -> String {
    let mut out = String::from("{");
    for (i, name) in names.iter().enumerate() {
        if i > 0 {
            out.push(',');
        }
        out.push('"');
        for c in name.chars() {
            if c == '\\' || c == '"' {
                out.push('\\');
            }
            out.push(c);
        }
        out.push('"');
    }
    out.push('}');
    out
}

#[cfg(test)]
mod tests {
    use forgebase_core::permission::LinkScope;

    use super::*;

    fn names(items: &[&str]) -> Vec<String> {
        items.iter().map(|s| (*s).to_owned()).collect()
    }

    #[test]
    fn an_array_literal_quotes_every_element() {
        assert_eq!(array_literal(&[]), "{}");
        assert_eq!(array_literal(&names(&["a"])), r#"{"a"}"#);
        assert_eq!(array_literal(&names(&["a", "b c"])), r#"{"a","b c"}"#);
        assert_eq!(
            array_literal(&names(&[r#"x,"y"\z"#, "{b}", "NULL", ""])),
            r#"{"x,\"y\"\\z","{b}","NULL",""}"#
        );
    }

    #[test]
    fn no_restriction_adds_nothing() {
        let scope = ReadScope::default();
        let sql = scope.and_sql(3);
        assert_eq!(sql.sql, "");
        assert!(sql.params.is_empty());
    }

    #[test]
    fn the_owner_condition_is_what_it_was() {
        let scope = ReadScope::new(Some("ann"), RowScope::default());
        let sql = scope.and_sql(2);
        assert_eq!(sql.sql, " AND \"owner\" = $2::text");
        assert_eq!(sql.params, ["ann"]);
    }

    #[test]
    fn names_and_links_are_bound_conditions_numbered_after_the_owner() {
        let retail = names(&["Retail", "Wholesale"]);
        let c1 = names(&["C-1"]);
        let rows = RowScope {
            names: Some(&c1),
            links: vec![
                LinkScope {
                    field: "group",
                    target: "customer_group",
                    allowed: &retail,
                },
                LinkScope {
                    field: "billing_group",
                    target: "customer_group",
                    allowed: &retail,
                },
            ],
        };
        let sql = ReadScope::new(Some("ann"), rows).and_sql(2);
        assert_eq!(
            sql.sql,
            " AND \"owner\" = $2::text \
             AND \"name\" = ANY($3::text::text[]) \
             AND (\"group\" IS NULL OR \"group\" IN (SELECT t.\"id\" FROM \"doc_customer_group\" t WHERE t.\"name\" = ANY($4::text::text[]))) \
             AND (\"billing_group\" IS NULL OR \"billing_group\" IN (SELECT t.\"id\" FROM \"doc_customer_group\" t WHERE t.\"name\" = ANY($4::text::text[])))"
        );
        // One parameter for the target both fields point at.
        assert_eq!(
            sql.params,
            ["ann", r#"{"C-1"}"#, r#"{"Retail","Wholesale"}"#]
        );
    }

    #[test]
    fn a_qualified_condition_names_the_table() {
        let c1 = names(&["C-1"]);
        let rows = RowScope {
            names: Some(&c1),
            links: vec![],
        };
        let mut n = 0;
        let conditions =
            ReadScope::new(None, rows).conditions(Some("\"doc_x\""), &mut |_, cast| {
                n += 1;
                format!("${n}::{cast}")
            });
        assert_eq!(conditions, ["\"doc_x\".\"name\" = ANY($1::text::text[])"]);
    }
}
