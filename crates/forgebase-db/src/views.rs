//! Saved list views (M8.3, architecture §11): a named filter, sort and column set per DocType.
//!
//! A view is a row of `fb_saved_view` (migration 0009). It is **private** (it has an owner, the
//! user who saved it) or **shared** (no owner; everyone who can read the DocType sees it, and only
//! an Administrator, or the system context, creates, changes or deletes one).
//!
//! - **Privacy.** [`list_views`] returns the caller's own private views and the shared ones.
//!   Another user's private view is [`DbError::NotFound`] for [`update_view`] and
//!   [`delete_view`], not a refusal, so nothing tells that it exists, an Administrator included.
//!   A shared view that a non-administrator tries to change is [`DbError::ViewDenied`]: they can
//!   see it anyway.
//! - **Rights.** Every call needs the **read** right on the DocType, decided by the one
//!   evaluator ([`access_to`]), like [`list`](crate::list). A view grants nothing: applying it
//!   runs the ordinary list query, with the caller's own read scope.
//! - **Definition.** `{"filters": [...], "sort": [...], "columns": [...]}`, the filters and the
//!   sort in the form of the list API. It is checked when it is written, by the code that runs
//!   a list ([`ViewDefinition`]), and **not** when it is read: a DocType may lose a field that a
//!   view names, and the client then gets `invalid_query` from the list and offers to fix the
//!   view.
//! - **Limits.** A label is 1 to 64 characters, trimmed; a definition is at most 16 KiB; a user
//!   has at most 100 private views per DocType and a DocType at most 100 shared ones.
//! - **Concurrency.** `update_view` and `delete_view` need the `version` the caller read.
//! - A view is never converted between private and shared (not supported in M8): save a new one.
//!
//! The system context has no user id: it manages shared views only.

use forgebase_core::meta::{DocType, FieldType, MAX_LIST_FIELDS, Registry};
use forgebase_core::permission::Caller;
use serde_json::Value as Json;
use time::OffsetDateTime;
use tokio_postgres::GenericClient;
use tokio_postgres::error::SqlState;
use tokio_postgres::types::ToSql;
use uuid::Uuid;

use crate::document::{access_to, document_doctype};
use crate::error::DbError;
use crate::list::{ListQuery, validate};
use forgebase_core::permission::Right;

/// The longest label, in characters.
pub const MAX_LABEL_CHARS: usize = 64;
/// The largest serialized definition, in bytes.
pub const MAX_DEFINITION_BYTES: usize = 16 * 1024;
/// The most private views one user has per DocType.
pub const MAX_PRIVATE_VIEWS: i64 = 100;
/// The most shared views a DocType has.
pub const MAX_SHARED_VIEWS: i64 = 100;
/// The most columns a view shows (the cap of a DocType's `list_fields`).
pub const MAX_VIEW_COLUMNS: usize = MAX_LIST_FIELDS;

/// What `doctype` is called in `NotFound` errors.
const VIEW: &str = "saved view";

/// A saved view as stored.
#[derive(Debug, Clone, PartialEq)]
pub struct View {
    /// The id (UUID v7).
    pub id: Uuid,
    /// The DocType.
    pub doctype: String,
    /// The label.
    pub label: String,
    /// The user name of the owner, or `None` for a shared view.
    pub owner: Option<String>,
    /// `{"filters", "sort", "columns"}`, as written.
    pub definition: Json,
    /// Incremented by every update.
    pub version: i64,
    /// When the view was last written.
    pub modified_at: OffsetDateTime,
}

impl View {
    /// True for a shared view.
    pub fn shared(&self) -> bool {
        self.owner.is_none()
    }
}

/// A definition to store: the JSON as it is kept, and the filters and sort it holds, parsed
/// by the caller with the parser of the list API (that parser lives in `forgebase-http`, which
/// depends on this crate). The parsed query is checked here by the list's own validation; the
/// JSON must have exactly the keys `filters`, `sort` and `columns` and the same number of
/// filters and sort keys as the query.
#[derive(Debug, Clone, PartialEq)]
pub struct ViewDefinition {
    /// What is stored.
    pub json: Json,
    /// The filters and sort of `json`, parsed. `limit` and `offset` stay unset.
    pub query: ListQuery,
}

/// The changes [`update_view`] makes; a `None` keeps what the view has.
#[derive(Debug, Clone, Copy, Default)]
pub struct ViewChanges<'a> {
    /// A new label.
    pub label: Option<&'a str>,
    /// A new definition.
    pub definition: Option<&'a ViewDefinition>,
}

const COLUMNS: &str =
    "v.id, v.doctype, v.label, u.username, v.definition::text, v.version, v.modified_at";

fn invalid(message: impl Into<String>) -> DbError {
    DbError::InvalidView(message.into())
}

fn not_found(id: Uuid) -> DbError {
    DbError::NotFound {
        doctype: VIEW.to_owned(),
        name: id.to_string(),
    }
}

fn from_row(row: &tokio_postgres::Row) -> Result<View, DbError> {
    let text: String = row.try_get(4)?;
    Ok(View {
        id: row.try_get(0)?,
        doctype: row.try_get(1)?,
        label: row.try_get(2)?,
        owner: row.try_get(3)?,
        definition: serde_json::from_str(&text)
            .map_err(|e| DbError::Job(format!("a stored view definition is not JSON: {e}")))?,
        version: row.try_get(5)?,
        modified_at: row.try_get(6)?,
    })
}

/// The id of the user `caller` is, `None` for the system context or a user that does not exist.
async fn caller_id(client: &impl GenericClient, caller: &Caller) -> Result<Option<Uuid>, DbError> {
    match caller {
        Caller::User { username, .. } => Ok(client
            .query_opt("SELECT id FROM fb_user WHERE username = $1", &[username])
            .await?
            .map(|row| row.get(0))),
        Caller::System { .. } => Ok(None),
    }
}

/// The views `caller` sees for `doctype`: the shared ones first, then the caller's own, each
/// group by label in byte order. Needs the read right on the DocType
/// ([`DbError::PermissionDenied`]). The definitions are returned as stored, valid or not.
pub async fn list_views(
    client: &impl GenericClient,
    registry: &Registry,
    doctype: &str,
    caller: &Caller,
) -> Result<Vec<View>, DbError> {
    let doctype = document_doctype(registry, doctype)?;
    access_to(doctype, caller, Right::Read)?;
    let owner = caller_id(client, caller).await?;
    let sql = format!(
        "SELECT {COLUMNS} FROM fb_saved_view v LEFT JOIN fb_user u ON u.id = v.owner_id \
         WHERE v.doctype = $1 AND (v.owner_id IS NULL OR v.owner_id = $2) \
         ORDER BY (v.owner_id IS NOT NULL), v.label COLLATE \"C\""
    );
    client
        .query(&sql, &[&doctype.name, &owner])
        .await?
        .iter()
        .map(from_row)
        .collect()
}

/// Creates a view and returns it. A private view (`shared` false) belongs to the caller; a
/// shared one needs the Administrator role or the system context ([`DbError::ViewDenied`]), and
/// the system context can create shared views only. Needs the read right on the DocType.
///
/// The label and definition are checked ([`DbError::InvalidView`], or the list's
/// [`DbError::InvalidQuery`] reworded as one); a label that is taken, for the same user and
/// DocType or for a shared view, is [`DbError::ViewExists`]; over the limit of private or shared
/// views is [`DbError::InvalidView`]. The count and the insert run in one transaction under an
/// advisory lock, so the limit holds for concurrent requests too.
pub async fn create_view(
    client: &mut impl GenericClient,
    registry: &Registry,
    doctype: &str,
    caller: &Caller,
    label: &str,
    definition: &ViewDefinition,
    shared: bool,
) -> Result<View, DbError> {
    let doctype = document_doctype(registry, doctype)?;
    access_to(doctype, caller, Right::Read)?;
    if shared && !caller.bypasses_permissions() {
        return Err(DbError::ViewDenied(
            "only an Administrator can create a shared view",
        ));
    }
    let owner = if shared {
        None
    } else {
        Some(caller_id(client, caller).await?.ok_or(DbError::ViewDenied(
            "the system context can manage shared views only",
        ))?)
    };
    check_label(label)?;
    check_definition(doctype, definition)?;

    let tx = client.transaction().await?;
    let scope = owner.map_or_else(|| "shared".to_owned(), |id| id.to_string());
    tx.query(
        "SELECT pg_advisory_xact_lock(hashtextextended($1, 0))",
        &[&format!("fb_saved_view:{}:{scope}", doctype.name)],
    )
    .await?;
    let (count, limit, what) = if owner.is_some() {
        (
            tx.query_one(
                "SELECT count(*) FROM fb_saved_view WHERE doctype = $1 AND owner_id = $2",
                &[&doctype.name, &owner],
            )
            .await?,
            MAX_PRIVATE_VIEWS,
            "private views for this DocType",
        )
    } else {
        (
            tx.query_one(
                "SELECT count(*) FROM fb_saved_view WHERE doctype = $1 AND owner_id IS NULL",
                &[&doctype.name],
            )
            .await?,
            MAX_SHARED_VIEWS,
            "shared views for this DocType",
        )
    };
    if count.get::<_, i64>(0) >= limit {
        return Err(invalid(format!("at most {limit} {what}: delete one first")));
    }
    let id = Uuid::now_v7();
    let sql = format!(
        "WITH v AS (INSERT INTO fb_saved_view (id, doctype, label, owner_id, definition) \
         VALUES ($1, $2, $3, $4, $5::text::jsonb) \
         RETURNING id, doctype, label, owner_id, definition, version, modified_at) \
         SELECT {COLUMNS} FROM v LEFT JOIN fb_user u ON u.id = v.owner_id"
    );
    let text = definition.json.to_string();
    let row = tx
        .query_one(&sql, &[&id, &doctype.name, &label, &owner, &text])
        .await
        .map_err(|e| exists_or(e, &doctype.name, label))?;
    let view = from_row(&row)?;
    tx.commit().await?;
    Ok(view)
}

/// Changes the label and/or the definition of a view and returns it with the next `version`.
///
/// Who may: the owner of a private view; an Administrator (or the system context) for a shared
/// view. Anyone else gets [`DbError::NotFound`] for a private view (also when it is in another
/// DocType than `doctype`), [`DbError::ViewDenied`] for a shared one. The permission is decided
/// before the `version` is compared ([`DbError::Conflict`]), so a hidden view is never told
/// from a missing one. Without a change: [`DbError::NoChanges`].
pub async fn update_view(
    client: &impl GenericClient,
    registry: &Registry,
    doctype: &str,
    id: Uuid,
    expected_version: i64,
    caller: &Caller,
    changes: &ViewChanges<'_>,
) -> Result<View, DbError> {
    let (label, definition) = (changes.label, changes.definition);
    let doctype = document_doctype(registry, doctype)?;
    access_to(doctype, caller, Right::Read)?;
    authorize(client, doctype, id, expected_version, caller).await?;
    if label.is_none() && definition.is_none() {
        return Err(DbError::NoChanges);
    }
    if let Some(label) = label {
        check_label(label)?;
    }
    if let Some(definition) = definition {
        check_definition(doctype, definition)?;
    }
    let text = definition.map(|d| d.json.to_string());
    let sql = format!(
        "WITH v AS (UPDATE fb_saved_view SET label = coalesce($3::text, label), \
         definition = coalesce($4::text::jsonb, definition), modified_at = now(), \
         version = version + 1 WHERE id = $1 AND doctype = $2 AND version = $5 \
         RETURNING id, doctype, label, owner_id, definition, version, modified_at) \
         SELECT {COLUMNS} FROM v LEFT JOIN fb_user u ON u.id = v.owner_id"
    );
    let rows = client
        .query(
            &sql,
            &[&id, &doctype.name, &label, &text, &expected_version],
        )
        .await
        .map_err(|e| exists_or(e, &doctype.name, label.unwrap_or_default()))?;
    match rows.first() {
        Some(row) => from_row(row),
        // Changed or deleted between the check and the write.
        None => Err(missing_or_conflict(client, doctype, id, expected_version).await),
    }
}

/// Deletes a view. The same rules as [`update_view`].
pub async fn delete_view(
    client: &impl GenericClient,
    registry: &Registry,
    doctype: &str,
    id: Uuid,
    expected_version: i64,
    caller: &Caller,
) -> Result<(), DbError> {
    let doctype = document_doctype(registry, doctype)?;
    access_to(doctype, caller, Right::Read)?;
    authorize(client, doctype, id, expected_version, caller).await?;
    let deleted = client
        .execute(
            "DELETE FROM fb_saved_view WHERE id = $1 AND doctype = $2 AND version = $3",
            &[&id, &doctype.name, &expected_version],
        )
        .await?;
    if deleted == 0 {
        return Err(missing_or_conflict(client, doctype, id, expected_version).await);
    }
    Ok(())
}

/// Decides whether `caller` may change the view, then compares the version.
async fn authorize(
    client: &impl GenericClient,
    doctype: &DocType,
    id: Uuid,
    expected_version: i64,
    caller: &Caller,
) -> Result<(), DbError> {
    let row = client
        .query_opt(
            "SELECT owner_id, version FROM fb_saved_view WHERE id = $1 AND doctype = $2",
            &[&id, &doctype.name],
        )
        .await?
        .ok_or_else(|| not_found(id))?;
    let owner: Option<Uuid> = row.get(0);
    let current: i64 = row.get(1);
    match owner {
        Some(owner) if Some(owner) != caller_id(client, caller).await? => {
            return Err(not_found(id));
        }
        Some(_) => {}
        None if caller.bypasses_permissions() => {}
        None => {
            return Err(DbError::ViewDenied(
                "only an Administrator can change a shared view",
            ));
        }
    }
    if current != expected_version {
        return Err(DbError::Conflict {
            doctype: VIEW.to_owned(),
            name: id.to_string(),
            expected: expected_version,
            current,
        });
    }
    Ok(())
}

/// Why a write that matched no row (after the authorization passed) did nothing.
async fn missing_or_conflict(
    client: &impl GenericClient,
    doctype: &DocType,
    id: Uuid,
    expected: i64,
) -> DbError {
    let params: [&(dyn ToSql + Sync); 2] = [&id, &doctype.name];
    match client
        .query_opt(
            "SELECT version FROM fb_saved_view WHERE id = $1 AND doctype = $2",
            &params,
        )
        .await
    {
        Ok(Some(row)) => DbError::Conflict {
            doctype: VIEW.to_owned(),
            name: id.to_string(),
            expected,
            current: row.get(0),
        },
        Ok(None) => not_found(id),
        Err(e) => e.into(),
    }
}

/// A unique violation of the label indexes becomes [`DbError::ViewExists`].
fn exists_or(error: tokio_postgres::Error, doctype: &str, label: &str) -> DbError {
    if error.code() == Some(&SqlState::UNIQUE_VIOLATION) {
        DbError::ViewExists {
            doctype: doctype.to_owned(),
            label: label.to_owned(),
        }
    } else {
        error.into()
    }
}

/// A label is 1 to [`MAX_LABEL_CHARS`] characters, has no leading or trailing whitespace and no
/// control characters.
fn check_label(label: &str) -> Result<(), DbError> {
    let length = label.chars().count();
    if length == 0 || length > MAX_LABEL_CHARS {
        Err(invalid(format!(
            "the label must be 1 to {MAX_LABEL_CHARS} characters"
        )))
    } else if label.trim() != label {
        Err(invalid(
            "the label must not start or end with whitespace".to_owned(),
        ))
    } else if label.chars().any(char::is_control) {
        Err(invalid(
            "the label must not contain control characters".to_owned(),
        ))
    } else {
        Ok(())
    }
}

/// The columns a view may show: the column fields except Text (not Table: it has no column),
/// and the standard columns `name`, `owner`, `modified_by`, `created_at`, `modified_at`, and
/// `docstatus` and `workflow_state` where the DocType has them.
fn column_allowed(doctype: &DocType, name: &str) -> Result<(), String> {
    match name {
        "name" | "owner" | "modified_by" | "created_at" | "modified_at" => return Ok(()),
        "docstatus" if doctype.submittable => return Ok(()),
        "workflow_state" if doctype.workflow.is_some() => return Ok(()),
        _ => {}
    }
    match doctype.field(name).map(|f| &f.field_type) {
        None => Err(format!(
            "unknown column {name:?} for DocType `{}`",
            doctype.name
        )),
        Some(FieldType::Table { .. }) => Err(format!(
            "column {name:?} is a table field: its rows cannot be shown in a list"
        )),
        Some(FieldType::Text) => Err(format!(
            "column {name:?} is a text field: long text cannot be shown in a list"
        )),
        Some(_) => Ok(()),
    }
}

fn strings<'a>(json: &'a Json, key: &str) -> Result<Vec<&'a str>, DbError> {
    json.get(key)
        .and_then(Json::as_array)
        .ok_or_else(|| invalid(format!("`{key}` must be a list")))?
        .iter()
        .map(|item| {
            item.as_str()
                .ok_or_else(|| invalid(format!("`{key}` must be a list of strings")))
        })
        .collect()
}

/// Checks the definition: the keys, the size, the columns, and the filters and sort through the
/// list's own validation ([`validate`]).
fn check_definition(doctype: &DocType, definition: &ViewDefinition) -> Result<(), DbError> {
    let Json::Object(object) = &definition.json else {
        return Err(invalid("the definition must be an object"));
    };
    if let Some(key) = object
        .keys()
        .find(|k| !matches!(k.as_str(), "filters" | "sort" | "columns"))
    {
        return Err(invalid(format!(
            "unknown key {key:?} in the definition (use filters, sort, columns)"
        )));
    }
    if definition.json.to_string().len() > MAX_DEFINITION_BYTES {
        return Err(invalid(format!(
            "the definition is larger than {} KiB",
            MAX_DEFINITION_BYTES / 1024
        )));
    }
    let filters = object
        .get("filters")
        .and_then(Json::as_array)
        .ok_or_else(|| invalid("`filters` must be a list"))?;
    let sort = strings(&definition.json, "sort")?;
    if filters.len() != definition.query.filters.len() || sort.len() != definition.query.sort.len()
    {
        return Err(invalid(
            "the filters and sort do not match their parsed form",
        ));
    }
    validate(doctype, &definition.query).map_err(|e| match e {
        DbError::InvalidQuery(message) => invalid(format!("the filters or sort: {message}")),
        other => other,
    })?;
    let columns = strings(&definition.json, "columns")?;
    if columns.len() > MAX_VIEW_COLUMNS {
        return Err(invalid(format!(
            "a view shows at most {MAX_VIEW_COLUMNS} columns, got {}",
            columns.len()
        )));
    }
    for (i, column) in columns.iter().enumerate() {
        if columns[..i].contains(column) {
            return Err(invalid(format!("column {column:?} is listed twice")));
        }
        column_allowed(doctype, column).map_err(invalid)?;
    }
    Ok(())
}
