//! Fixtures: records an app ships, applied by `migrate` through the document pipeline (M9.4,
//! ADR 0022).
//!
//! The table (`fb_fixture`) comes from framework migration 0011. A fixture record is identified
//! by its DocType and name (the DocType uses field naming); `fb_fixture` remembers the checksum of
//! the version of the record that was last applied, so that `migrate` can tell "the app shipped
//! a new version" from "nothing changed" and leave the site's own edits alone until then.
//!
//! | `fb_fixture` row | document | action |
//! |---|---|---|
//! | none | missing | insert, record the checksum |
//! | none | exists | keep it as it is, record the checksum (adopted) |
//! | same checksum | any | nothing: no read of the document, no write |
//! | other checksum | exists | update the fields the record has, record the checksum |
//! | other checksum | missing | insert again, record the checksum |
//!
//! Nothing is ever deleted. The checksum itself is computed by the caller (`forgebase`), which
//! also parses the JSON: this crate only gets parsed documents, like [`crate::import`].

use forgebase_core::document::{Changes, Document};
use forgebase_core::ident::table_name;
use forgebase_core::meta::{Naming, Registry};
use forgebase_core::permission::Caller;
use tokio_postgres::Client;

use crate::app_migrations::AppSpec;
use crate::error::DbError;
use crate::ident::quote_ident;
use crate::sync::lock_timeout_setting;
use crate::{get, insert, update};

/// One fixture record, already parsed and checked by the caller.
#[derive(Debug, Clone, Copy)]
pub struct FixtureRecord<'a> {
    /// The DocType (field naming, no workflow, not a child).
    pub doctype: &'a str,
    /// The path of the fixture file, for messages.
    pub file: &'a str,
    /// The position of the record in the file, counting from 1, for messages.
    pub index: usize,
    /// The document name: the value of the naming field.
    pub name: &'a str,
    /// Hex SHA-256 of the canonical JSON of the record.
    pub checksum: &'a str,
    /// The record as a document: every field it gives, Table fields as rows.
    pub document: &'a Document,
}

/// What `migrate` did (or, in a dry run, would do) with the fixtures of one app.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AppFixtures {
    /// The app name.
    pub app: String,
    /// How many records the app has in its fixture files.
    pub records: usize,
    /// Records inserted, including records inserted again because the app shipped a new version
    /// of a record the site had deleted.
    pub inserted: usize,
    /// Records whose new version was applied to an existing document.
    pub updated: usize,
    /// Records whose document existed already when the fixture was first applied: the document
    /// was left as it is and the record is now tracked.
    pub kept: usize,
    /// Records that did not change since they were last applied: nothing was done.
    pub unchanged: usize,
}

impl AppFixtures {
    fn new(app: &str, records: usize) -> Self {
        Self {
            app: app.to_owned(),
            records,
            inserted: 0,
            updated: 0,
            kept: 0,
            unchanged: 0,
        }
    }
}

/// Whether the fixture table exists (framework migration 0011 has been applied).
pub(crate) async fn table_exists(client: &Client) -> Result<bool, DbError> {
    let row = client
        .query_one("SELECT to_regclass('fb_fixture') IS NOT NULL", &[])
        .await?;
    Ok(row.try_get(0)?)
}

/// The actor recorded as `owner` and `modified_by` of what the fixtures of `app` write.
fn actor(app: &str) -> String {
    format!("fixture:{app}")
}

fn describe(record: &FixtureRecord<'_>) -> String {
    format!("record {} (`{}`)", record.index, record.name)
}

/// Applies the fixtures of one app in one transaction: any error rolls all of them back
/// ([`DbError::FixtureFailed`]). The transaction has a `lock_timeout` like an app migration.
pub(crate) async fn apply(
    client: &mut Client,
    registry: &Registry,
    app: &AppSpec<'_>,
    lock_timeout: std::time::Duration,
) -> Result<AppFixtures, DbError> {
    let mut report = AppFixtures::new(app.name, app.fixtures.len());
    let caller = Caller::system(actor(app.name));
    let setup_failed = |e: DbError| DbError::FixtureFailed {
        app: app.name.to_owned(),
        file: "(all fixture files)".to_owned(),
        record: "transaction".to_owned(),
        message: format!("{e} (nothing from this app's fixtures was kept)"),
    };
    let mut tx = client
        .transaction()
        .await
        .map_err(|e| setup_failed(e.into()))?;
    tx.query(
        "SELECT set_config('lock_timeout', $1, true)",
        &[&lock_timeout_setting(lock_timeout)],
    )
    .await
    .map_err(|e| setup_failed(e.into()))?;
    for record in app.fixtures {
        let failed = |message: String| DbError::FixtureFailed {
            app: app.name.to_owned(),
            file: record.file.to_owned(),
            record: describe(record),
            message,
        };
        // The transaction is dropped, so rolled back, on every error below.
        let fail = |e: DbError| failed(format!("{e} (nothing from this app's fixtures was kept)"));
        let doctype = registry.get(record.doctype).ok_or_else(|| {
            failed(format!(
                "unknown DocType `{}` (nothing from this app's fixtures was kept)",
                record.doctype
            ))
        })?;
        let previous: Option<String> = tx
            .query_opt(
                "SELECT checksum FROM fb_fixture WHERE doctype = $1 AND name = $2",
                &[&record.doctype, &record.name],
            )
            .await
            .map_err(|e| fail(e.into()))?
            .map(|row| row.try_get(0))
            .transpose()
            .map_err(|e| fail(e.into()))?;
        if previous.as_deref() == Some(record.checksum) {
            report.unchanged += 1;
            continue;
        }
        let stored = get(&mut tx, registry, record.doctype, record.name, &caller)
            .await
            .map_err(fail)?;
        match (previous, stored) {
            (None, Some(_)) => report.kept += 1,
            (Some(_), Some(stored)) => {
                let mut changes = Changes {
                    values: record.document.values.clone(),
                    tables: record.document.tables.clone(),
                };
                // The naming field is the identity of the record and cannot be changed.
                if let Naming::Field(field) = &doctype.naming {
                    changes.values.remove(field);
                }
                update(
                    &mut tx,
                    registry,
                    record.doctype,
                    record.name,
                    stored.version,
                    &changes,
                    &caller,
                )
                .await
                .map_err(fail)?;
                report.updated += 1;
            }
            (_, None) => {
                let inserted = insert(
                    &mut tx,
                    registry,
                    record.doctype,
                    record.document.clone(),
                    &caller,
                )
                .await
                .map_err(fail)?;
                // A hook may change the naming field; the record would then be tracked under a
                // name its document does not have, and inserted again at every changed version.
                if inserted.name != record.name {
                    return Err(failed(format!(
                        "a hook changed the naming field of fixture record `{}` to `{}`; fixtures need a stable name (nothing from this app's fixtures was kept)",
                        record.name, inserted.name
                    )));
                }
                report.inserted += 1;
            }
        }
        tx.execute(
            "INSERT INTO fb_fixture (doctype, name, app, checksum) VALUES ($1, $2, $3, $4) \
             ON CONFLICT (doctype, name) DO UPDATE SET app = excluded.app, \
             checksum = excluded.checksum, applied_at = now()",
            &[&record.doctype, &record.name, &app.name, &record.checksum],
        )
        .await
        .map_err(|e| fail(e.into()))?;
    }
    // A database error here is still the fixtures' failure: `migrate` then keeps and prints the
    // report of what was committed before.
    tx.commit().await.map_err(|e| DbError::FixtureFailed {
        app: app.name.to_owned(),
        file: "(all fixture files)".to_owned(),
        record: "commit".to_owned(),
        message: format!(
            "{} (the outcome of the commit is unknown: the fixtures of this app may have been kept; running migrate again is safe)",
            DbError::from(e)
        ),
    })?;
    Ok(report)
}

/// What [`apply`] would do, read-only (for a dry run). The framework migration of the table may
/// be pending and the DocType tables may not exist yet (the schema sync of a dry run is rolled
/// back): a missing table or row counts as "no document", so the plan is exact for a database
/// that is up to date and a best statement for one that is not. Nothing is validated.
pub(crate) async fn plan(client: &Client, app: &AppSpec<'_>) -> Result<AppFixtures, DbError> {
    let mut report = AppFixtures::new(app.name, app.fixtures.len());
    let tracked = table_exists(client).await?;
    for record in app.fixtures {
        let previous: Option<String> = if tracked {
            client
                .query_opt(
                    "SELECT checksum FROM fb_fixture WHERE doctype = $1 AND name = $2",
                    &[&record.doctype, &record.name],
                )
                .await?
                .map(|row| row.try_get(0))
                .transpose()?
        } else {
            None
        };
        if previous.as_deref() == Some(record.checksum) {
            report.unchanged += 1;
            continue;
        }
        let table = quote_ident(&table_name(record.doctype));
        let has_table = client
            .query_one("SELECT to_regclass($1) IS NOT NULL", &[&table])
            .await?
            .try_get::<_, bool>(0)?;
        let exists = has_table
            && client
                .query_opt(
                    &format!("SELECT 1 FROM {table} WHERE \"name\" = $1::text"),
                    &[&record.name],
                )
                .await?
                .is_some();
        match (previous, exists) {
            (None, true) => report.kept += 1,
            (Some(_), true) => report.updated += 1,
            (_, false) => report.inserted += 1,
        }
    }
    Ok(report)
}
