//! Link titles (UX.4): the title of the documents that Link values point to, so that the desk can
//! show "Ada Lovelace" instead of "CUST-0001".
//!
//! The titles follow **the caller's read rules**, exactly like [`list`](crate::list): a target
//! DocType the caller cannot read at all gets no entry, and for an `if_owner` read the statement
//! is limited to the caller's own documents (`"owner" = $2`), so a title never reveals a document
//! the caller could not fetch. Nothing is decided here: the access comes from
//! the one evaluator in `forgebase-core`, and the owner
//! condition is the one `list` and `get` use.
//!
//! One statement per target DocType (never one per row):
//! `SELECT name, <title> FROM <target> WHERE name = ANY($1)`.

use std::collections::{BTreeMap, BTreeSet};

use forgebase_core::ident::table_name;
use forgebase_core::meta::{DocType, FieldType, Registry};
use forgebase_core::permission::{Access, Caller, Right, evaluate};
use forgebase_core::value::Value;
use tokio_postgres::GenericClient;
use tokio_postgres::types::ToSql;

use crate::document::{StoredDocument, document_doctype, owner_scope};
use crate::error::DbError;
use crate::ident::quote_ident;
use crate::list::MAX_LIMIT;

/// Titles by target DocType and then by document name: `{"customer": {"CUST-0001": "Ada"}}`.
/// A target the caller cannot read, a target without a `title_field`, and a document without a
/// title have no entry.
pub type LinkTitles = BTreeMap<String, BTreeMap<String, String>>;

/// The Link values of `documents` (of `doctype`), grouped by target DocType; the Link fields of
/// the rows of Table fields are included. The names are deduplicated and at most [`MAX_LIMIT`]
/// per target are kept (a page is at most that many documents; a title that is left out only
/// means the desk shows the name).
fn collect(
    registry: &Registry,
    doctype: &DocType,
    documents: &[StoredDocument],
) -> BTreeMap<String, BTreeSet<String>> {
    let mut found: BTreeMap<String, BTreeSet<String>> = BTreeMap::new();
    let mut add = |target: &str, value: Option<&Value>| {
        if let Some(Value::Text(name)) = value {
            let names = found.entry(target.to_owned()).or_default();
            if names.len() < MAX_LIMIT as usize {
                names.insert(name.clone());
            }
        }
    };
    for field in &doctype.fields {
        match &field.field_type {
            FieldType::Link { target } => {
                for document in documents {
                    add(target, document.values.get(&field.name));
                }
            }
            FieldType::Table { target } => {
                let Some(child) = registry.get(target) else {
                    continue;
                };
                for child_field in &child.fields {
                    let FieldType::Link { target } = &child_field.field_type else {
                        continue;
                    };
                    for row in documents
                        .iter()
                        .filter_map(|d| d.tables.get(&field.name))
                        .flatten()
                    {
                        add(target, row.get(&child_field.name));
                    }
                }
            }
            _ => {}
        }
    }
    found
}

/// The titles of the documents that `documents` (of `doctype`) link to, for the targets that
/// have a `title_field` and that the caller may read. See the module documentation.
///
/// `documents` are as [`list`](crate::list) or [`get`](crate::get) return them (Link values are
/// the targets' names). A caller who may not read a target gets nothing for it, not an error.
pub async fn link_titles(
    client: &impl GenericClient,
    registry: &Registry,
    caller: &Caller,
    doctype: &str,
    documents: &[StoredDocument],
) -> Result<LinkTitles, DbError> {
    let doctype = document_doctype(registry, doctype)?;
    resolve(
        client,
        registry,
        caller,
        collect(registry, doctype, documents),
    )
    .await
}

/// The titles for names grouped by target DocType, under the caller's read rules. Shared with
/// the Link columns of a report ([`report_link_titles`](crate::report_link_titles)).
pub(crate) async fn resolve(
    client: &impl GenericClient,
    registry: &Registry,
    caller: &Caller,
    found: BTreeMap<String, BTreeSet<String>>,
) -> Result<LinkTitles, DbError> {
    let mut titles = LinkTitles::new();
    for (target, names) in found {
        let Some(target_type) = registry.get(&target) else {
            continue;
        };
        let Some(title_field) = &target_type.title_field else {
            continue;
        };
        let access = evaluate(target_type, caller, Right::Read);
        if access == Access::Denied {
            continue;
        }
        let owner = owner_scope(access, caller);
        let names: Vec<String> = names.into_iter().collect();
        let found = query_titles(client, registry, target_type, title_field, &names, owner).await?;
        if !found.is_empty() {
            titles.insert(target, found);
        }
    }
    Ok(titles)
}

async fn query_titles(
    client: &impl GenericClient,
    registry: &Registry,
    target: &DocType,
    title_field: &str,
    names: &[String],
    owner: Option<&str>,
) -> Result<BTreeMap<String, String>, DbError> {
    let table = quote_ident(&table_name(&target.name));
    let column = quote_ident(title_field);
    // `title_field` was checked against the DocType's fields when it was loaded. A Link title is
    // shown as the name of what it points to, like every Link value; anything else as text.
    let title = match target.field(title_field).map(|f| &f.field_type) {
        Some(FieldType::Link { target: next }) => {
            if registry.get(next).is_none() {
                return Ok(BTreeMap::new());
            }
            format!(
                "(SELECT t.\"name\" FROM {} t WHERE t.\"id\" = {table}.{column})",
                quote_ident(&table_name(next))
            )
        }
        _ => format!("{table}.{column}::text"),
    };
    let mut sql = format!(
        "SELECT {table}.\"name\", {title} FROM {table} WHERE {table}.\"name\" = ANY($1::text[])"
    );
    let mut params: Vec<&(dyn ToSql + Sync)> = vec![&names];
    if let Some(owner) = &owner {
        sql.push_str(&format!(" AND {table}.\"owner\" = $2::text"));
        params.push(owner);
    }
    let mut found = BTreeMap::new();
    for row in client.query(&sql, &params).await? {
        let name: String = row.try_get(0)?;
        let title: Option<String> = row.try_get(1)?;
        if let Some(title) = title.filter(|t| !t.is_empty()) {
            found.insert(name, title);
        }
    }
    Ok(found)
}
