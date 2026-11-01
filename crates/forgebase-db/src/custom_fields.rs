//! Site custom fields (M10.1, ADR 0023): fields a site adds to a DocType in its own database.
//!
//! A field is a row of `fb_custom_field` (migration 0012), kept for ever: disabling hides it and
//! keeps its column and data, and enabling brings both back. The effective metadata of a site is
//! the registry of the apps (with their overlays) plus the **enabled** fields, appended after the
//! fields of the app, in `(doctype, created_at, name)` order ([`effective_registry`]).
//!
//! Every change runs in one transaction under the advisory lock of `migrate`, so schema changes
//! are serialised with it; it adds the column with the statements of the schema sync
//! ([`crate::plan::field_steps`]) and increments `fb_meta_version`, which running servers poll
//! ([`crate::RegistryHolder`]). Nothing here drops a column.
//!
//! A stored field that no longer validates (for example its Link target left the apps) is
//! **skipped with a warning**, never an error: a bad custom field must not take a site down.

use std::time::Duration;

use forgebase_core::ident::table_name;
use forgebase_core::meta::{DocField, Registry};
use forgebase_core::overlay::{field_to_json, parse_field_json};
use serde_json::Value as Json;
use time::OffsetDateTime;
use tokio_postgres::{Client, GenericClient};

use crate::error::DbError;
use crate::migrate::MIGRATE_LOCK_KEY;
use crate::plan::field_steps;
use crate::sync::lock_timeout_setting;

/// A custom field as stored.
#[derive(Debug, Clone, PartialEq)]
pub struct CustomField {
    /// The DocType the field was added to.
    pub doctype: String,
    /// The field name (starts with `custom_`).
    pub name: String,
    /// The field as JSON, with the keys of a TOML `[[fields]]` entry.
    pub definition: Json,
    /// False for a disabled field: hidden everywhere, column and data kept.
    pub enabled: bool,
    /// When the field was added.
    pub created_at: OffsetDateTime,
    /// When the field was last changed.
    pub modified_at: OffsetDateTime,
    /// Who changed it last (`cli` or a user name).
    pub modified_by: String,
}

impl CustomField {
    /// The `type` key of the definition (`?` when it is not there: a damaged row).
    pub fn type_name(&self) -> &str {
        self.definition
            .get("type")
            .and_then(Json::as_str)
            .unwrap_or("?")
    }

    /// The `label` key of the definition (empty when it is not there).
    pub fn label(&self) -> &str {
        self.definition
            .get("label")
            .and_then(Json::as_str)
            .unwrap_or("")
    }
}

const COLUMNS: &str =
    "doctype, name, definition::text, enabled, created_at, modified_at, modified_by";

fn from_row(row: &tokio_postgres::Row) -> Result<CustomField, DbError> {
    let text: String = row.try_get(2)?;
    Ok(CustomField {
        doctype: row.try_get(0)?,
        name: row.try_get(1)?,
        // A definition that is not JSON cannot be written (the column is jsonb), so this only
        // fails for a row that someone edited around the framework.
        definition: serde_json::from_str(&text).unwrap_or(Json::Null),
        enabled: row.try_get(3)?,
        created_at: row.try_get(4)?,
        modified_at: row.try_get(5)?,
        modified_by: row.try_get(6)?,
    })
}

/// The custom fields of `doctype` (every DocType when `None`), enabled and disabled, in the order
/// they are added to the DocType: DocType, creation time, name.
pub async fn list_custom_fields(
    client: &impl GenericClient,
    doctype: Option<&str>,
) -> Result<Vec<CustomField>, DbError> {
    let rows = client
        .query(
            &format!(
                "SELECT {COLUMNS} FROM fb_custom_field \
                 WHERE ($1::text IS NULL OR doctype = $1) ORDER BY doctype, created_at, name"
            ),
            &[&doctype],
        )
        .await?;
    rows.iter().map(from_row).collect()
}

/// The metadata of a site as it is now.
#[derive(Debug)]
pub struct Effective {
    /// The registry of the apps with the site's custom fields.
    pub registry: Registry,
    /// The value of `fb_meta_version` the registry was built from.
    pub version: i64,
    /// One message per stored field that was skipped because it no longer validates.
    pub warnings: Vec<String>,
}

/// The current `fb_meta_version`.
pub async fn meta_version(client: &impl GenericClient) -> Result<i64, DbError> {
    let row = client
        .query_one("SELECT version FROM fb_meta_version WHERE id = 1", &[])
        .await?;
    Ok(row.try_get(0)?)
}

/// `base` plus the custom fields stored in the database: the enabled ones, or with
/// `include_disabled` every one (the schema sync of `migrate` needs the column of a disabled
/// field too). Fields are appended per DocType in creation order. A field that does not validate
/// against the registry built so far is skipped and reported in [`Effective::warnings`], naming
/// `site`, the DocType and the field.
///
/// The version and the rows are read by separate statements: for a consistent pair run it in a
/// `REPEATABLE READ` transaction (the registry holder does), or under the migrate lock.
pub async fn effective_registry(
    client: &impl GenericClient,
    base: &Registry,
    site: &str,
    include_disabled: bool,
) -> Result<Effective, DbError> {
    let version = meta_version(client).await?;
    let rows = client
        .query(
            &format!(
                "SELECT {COLUMNS} FROM fb_custom_field \
                 WHERE enabled OR $1 ORDER BY doctype, created_at, name"
            ),
            &[&include_disabled],
        )
        .await?;
    let mut registry = base.clone();
    let mut warnings = Vec::new();
    for row in &rows {
        let stored = from_row(row)?;
        let skipped = |why: &dyn std::fmt::Display| {
            format!(
                "site `{site}`: custom field `{}` of DocType `{}` is skipped: {why}",
                stored.name, stored.doctype
            )
        };
        let field = match parse_field_json(&stored.definition) {
            Ok(field) => field,
            Err(e) => {
                warnings.push(skipped(&e));
                continue;
            }
        };
        if field.name != stored.name {
            warnings.push(skipped(&format!(
                "the definition names the field `{}`",
                field.name
            )));
            continue;
        }
        if let Err(e) = registry.add_custom_field(&stored.doctype, field) {
            warnings.push(skipped(&e));
        }
    }
    Ok(Effective {
        registry,
        version,
        warnings,
    })
}

/// Adds a custom field to `doctype` of the site (M10.1), in one transaction under the advisory
/// lock of `migrate` (waiting at most `lock_timeout`):
///
/// 1. the field is validated against the site's effective registry built **inside** the
///    transaction ([`Registry::add_custom_field`]: the prefix, no `required`, no `read_only`,
///    no Table type, a DocType that exists and is not a child, a free name, a valid Link
///    target); a name used by a disabled field is [`DbError::CustomFieldExists`];
/// 2. the row is inserted (`modified_by` is `actor`);
/// 3. the column is added, with the foreign key and index of a Link and the unique index of a
///    unique field, by the statements of the schema sync;
/// 4. `fb_meta_version` is incremented.
///
/// `base` is the registry of the apps (with their overlays). The table of the DocType must
/// exist ([`DbError::DocTypeNotMigrated`]). Running servers see the field within their reload
/// interval; the caller can reload its own registry at once.
pub async fn add_custom_field(
    client: &mut Client,
    base: &Registry,
    site: &str,
    doctype: &str,
    field: DocField,
    actor: &str,
    lock_timeout: Duration,
) -> Result<CustomField, DbError> {
    let tx = client.transaction().await?;
    lock(&tx, lock_timeout).await?;
    require_tables(&tx).await?;
    let exists = tx
        .query_opt(
            "SELECT 1 FROM fb_custom_field WHERE doctype = $1 AND name = $2",
            &[&doctype, &field.name],
        )
        .await?;
    if exists.is_some() {
        return Err(DbError::CustomFieldExists {
            doctype: doctype.to_owned(),
            name: field.name,
        });
    }
    let mut effective = effective_registry(&tx, base, site, false).await?.registry;
    effective
        .add_custom_field(doctype, field.clone())
        .map_err(DbError::CustomField)?;
    let table = table_name(doctype);
    let present = tx
        .query_one("SELECT to_regclass($1::text) IS NOT NULL", &[&table])
        .await?;
    if !present.try_get::<_, bool>(0)? {
        return Err(DbError::DocTypeNotMigrated {
            doctype: doctype.to_owned(),
        });
    }
    let definition = field_to_json(&field).to_string();
    let row = tx
        .query_one(
            &format!(
                "INSERT INTO fb_custom_field (doctype, name, definition, modified_by) \
                 VALUES ($1, $2, $3::text::jsonb, $4) RETURNING {COLUMNS}"
            ),
            &[&doctype, &field.name, &definition, &actor],
        )
        .await?;
    for step in field_steps(doctype, &field) {
        tx.batch_execute(&step_sql(&step)).await?;
    }
    bump_version(&tx).await?;
    let stored = from_row(&row)?;
    tx.commit().await?;
    Ok(stored)
}

/// Enables or disables a custom field (M10.1): flips the flag, sets `modified_at` and
/// `modified_by`, and increments `fb_meta_version`, under the lock of `migrate`. It never drops
/// a column or data: a disabled field is only left out of the effective registry, and enabling
/// it brings back the field with its data. Setting the state it has already is not an error and
/// still increments the version. An unknown field is [`DbError::CustomFieldNotFound`].
///
/// Enabling re-validates nothing here: a field that no longer validates is skipped with a
/// warning when the registry is built.
pub async fn set_custom_field_enabled(
    client: &mut Client,
    doctype: &str,
    name: &str,
    enabled: bool,
    actor: &str,
    lock_timeout: Duration,
) -> Result<CustomField, DbError> {
    let tx = client.transaction().await?;
    lock(&tx, lock_timeout).await?;
    require_tables(&tx).await?;
    let row = tx
        .query_opt(
            &format!(
                "UPDATE fb_custom_field SET enabled = $3, modified_at = now(), modified_by = $4 \
                 WHERE doctype = $1 AND name = $2 RETURNING {COLUMNS}"
            ),
            &[&doctype, &name, &enabled, &actor],
        )
        .await?
        .ok_or_else(|| DbError::CustomFieldNotFound {
            doctype: doctype.to_owned(),
            name: name.to_owned(),
        })?;
    bump_version(&tx).await?;
    let stored = from_row(&row)?;
    tx.commit().await?;
    Ok(stored)
}

/// Makes sure that every stored custom field, enabled or disabled, has its column (and
/// foreign key and indexes): the registry the schema sync of `migrate` works from. Returns the
/// registry (`base` plus every stored field that validates) and the warnings for the skipped
/// ones. Idempotent: the sync adds only what is missing.
pub(crate) async fn schema_registry(
    client: &impl GenericClient,
    base: &Registry,
) -> Result<Effective, DbError> {
    effective_registry(client, base, "", true).await
}

fn step_sql(step: &crate::plan::Step) -> String {
    use crate::plan::Step;
    match step {
        Step::CreateTable { sql, .. }
        | Step::AddKeys { sql, .. }
        | Step::AddForeignKeys { sql, .. }
        | Step::AddColumn { sql, .. }
        | Step::CreateUniqueIndex { sql, .. }
        | Step::CreateIndex { sql, .. } => sql.clone(),
        Step::RegisterDocType { .. } => String::new(),
    }
}

async fn lock(tx: &tokio_postgres::Transaction<'_>, lock_timeout: Duration) -> Result<(), DbError> {
    tx.query(
        "SELECT set_config('lock_timeout', $1, true)",
        &[&lock_timeout_setting(lock_timeout)],
    )
    .await?;
    tx.query("SELECT pg_advisory_xact_lock($1)", &[&MIGRATE_LOCK_KEY])
        .await?;
    Ok(())
}

async fn require_tables(tx: &tokio_postgres::Transaction<'_>) -> Result<(), DbError> {
    let row = tx
        .query_one(
            "SELECT to_regclass('fb_custom_field') IS NOT NULL AND to_regclass('fb_meta_version') IS NOT NULL",
            &[],
        )
        .await?;
    if row.try_get::<_, bool>(0)? {
        Ok(())
    } else {
        Err(DbError::FrameworkNotMigrated)
    }
}

async fn bump_version(tx: &tokio_postgres::Transaction<'_>) -> Result<(), DbError> {
    tx.execute(
        "UPDATE fb_meta_version SET version = version + 1 WHERE id = 1",
        &[],
    )
    .await?;
    Ok(())
}
