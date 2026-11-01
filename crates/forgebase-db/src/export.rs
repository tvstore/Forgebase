//! Export: the documents of one DocType that the caller may read and export, in pages from one
//! consistent snapshot (M9.3, ADR 0021).
//!
//! An export runs in one **read-only `REPEATABLE READ` transaction** ([`begin_export`]), so the
//! count, every page and the child rows of every page see the same data. [`Export::start`]
//! checks the rights and counts the rows with the same `WHERE` as the list query (including the
//! `if_owner` scope); a count over the limit is [`DbError::ExportTooLarge`] and nothing is read.
//! [`Export::next_page`] then fetches [`PAGE_ROWS`] documents at a time through a portal, so the
//! memory in use does not depend on the size of the result. Formatting the pages (CSV or JSON)
//! is the job of the caller, `forgebase_http::transfer`.
//!
//! **Rights.** The caller needs **read and export**. Each right is decided by the one evaluator
//! ([`forgebase_core::permission::evaluate`]); if either right comes from an `if_owner` rule,
//! only the caller's own documents are exported.

use forgebase_core::meta::{DocType, Registry};
use forgebase_core::permission::{Access, Caller, Right, field_access, readable_doctype};
use tokio_postgres::types::ToSql;
use tokio_postgres::{Client, IsolationLevel, Portal, Transaction};

use crate::document::{
    StoredDocument, access_to, check_actor, decode_row, document_doctype, load_tables_for,
};
use crate::error::DbError;
use crate::list::{Filter, Sort, build_export};
use crate::value::SqlValue;

/// How many documents one page of an export has.
pub const PAGE_ROWS: i32 = 1000;

/// Starts the transaction an export runs in: read-only, `REPEATABLE READ`. Dropping it (or
/// calling `rollback`) ends it; nothing is ever written.
pub async fn begin_export(client: &mut Client) -> Result<Transaction<'_>, DbError> {
    Ok(client
        .build_transaction()
        .isolation_level(IsolationLevel::RepeatableRead)
        .read_only(true)
        .start()
        .await?)
}

/// What to export.
#[derive(Debug, Clone, Copy)]
pub struct ExportQuery<'a> {
    /// The DocType (not a child DocType).
    pub doctype: &'a str,
    /// Conditions every document meets (AND), as for the list query.
    pub filters: &'a [Filter],
    /// Sort keys, most significant first; `name` ascending is always appended.
    pub sort: &'a [Sort],
    /// The most rows allowed: more is [`DbError::ExportTooLarge`].
    pub limit: u64,
    /// Whether the pages carry the rows of the Table fields.
    pub with_tables: bool,
}

/// An export in progress: the portal of the rows and what is needed to read the next page.
pub struct Export {
    portal: Portal,
    doctype: DocType,
    count: u64,
    with_tables: bool,
    done: bool,
}

impl Export {
    /// Checks the caller's rights, counts the rows and opens the portal.
    ///
    /// Errors: [`DbError::PermissionDenied`] (read first, then export), [`DbError::InvalidQuery`]
    /// for a bad filter or sort, [`DbError::ExportTooLarge`], and database errors.
    pub async fn start(
        tx: &Transaction<'_>,
        registry: &Registry,
        caller: &Caller,
        query: &ExportQuery<'_>,
    ) -> Result<Self, DbError> {
        let ExportQuery {
            doctype,
            filters,
            sort,
            limit,
            with_tables,
        } = *query;
        let doctype = document_doctype(registry, doctype)?;
        check_actor(caller.name())?;
        let read = access_to(doctype, caller, Right::Read)?;
        let export = access_to(doctype, caller, Right::Export)?;
        let owner =
            (read == Access::OwnerOnly || export == Access::OwnerOnly).then(|| caller.name());
        // The columns, the filters, the sort and the child rows follow the fields the caller can
        // read: a hidden field is not exported and is unknown to the query (ADR 0024).
        let doctype = &*readable_doctype(doctype, field_access(doctype, caller));
        let (rows_sql, count_sql, binds) = build_export(doctype, owner, filters, sort)?;
        let params: Vec<SqlValue<'_>> = binds.iter().map(SqlValue).collect();
        let params: Vec<&(dyn ToSql + Sync)> =
            params.iter().map(|p| p as &(dyn ToSql + Sync)).collect();
        let count: i64 = tx.query_one(&count_sql, &params).await?.try_get(0)?;
        let count = u64::try_from(count).unwrap_or(0);
        if count > limit {
            return Err(DbError::ExportTooLarge { count, limit });
        }
        let statement = tx.prepare(&rows_sql).await?;
        let portal = tx.bind(&statement, &params).await?;
        Ok(Self {
            portal,
            doctype: doctype.clone(),
            count,
            with_tables,
            done: count == 0,
        })
    }

    /// The number of documents the export has (counted before the first page).
    pub fn count(&self) -> u64 {
        self.count
    }

    /// The next page of at most [`PAGE_ROWS`] documents in the order of the query, or `None`
    /// when there are no more. With tables, one query per child table reads the rows of the
    /// whole page. `tx` must be the transaction of [`Export::start`].
    pub async fn next_page(
        &mut self,
        tx: &Transaction<'_>,
        registry: &Registry,
    ) -> Result<Option<Vec<StoredDocument>>, DbError> {
        if self.done {
            return Ok(None);
        }
        let rows = tx.query_portal(&self.portal, PAGE_ROWS).await?;
        if rows.len() < PAGE_ROWS as usize {
            self.done = true;
        }
        if rows.is_empty() {
            return Ok(None);
        }
        let mut documents = rows
            .iter()
            .map(|row| decode_row(&self.doctype, row))
            .collect::<Result<Vec<_>, _>>()?;
        if self.with_tables && self.doctype.table_fields().next().is_some() {
            let ids: Vec<_> = documents.iter().map(|d| d.id).collect();
            let mut tables = load_tables_for(tx, registry, &self.doctype, &ids).await?;
            for document in &mut documents {
                document.tables = tables.remove(&document.id).unwrap_or_default();
            }
        }
        Ok(Some(documents))
    }
}
