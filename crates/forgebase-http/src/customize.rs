//! The customize routes (M10.1, ADR 0023): site custom fields, for the Administrator role only.
//!
//! `forgebase-db` does the work (validation against the site's effective registry, the row, the
//! column, the metadata version, all in one transaction under the migrate lock); these handlers
//! check the role, map the body and the errors, and reload this site's registry once a change
//! committed, so the next request on this server sees it. Other servers see it within their
//! `meta_reload_interval`.

use std::time::Duration;

use axum::extract::rejection::PathRejection;
use axum::extract::{Path, Query, State};
use axum::http::StatusCode;
use forgebase_core::overlay::parse_field_json;
use forgebase_db::CustomField;
use serde_json::{Value as Json, json};

use crate::AppState;
use crate::auth::Authenticated;
use crate::error::ApiError;
use crate::json::utc_rfc3339;
use crate::routes::{ApiJson, RawQuery, Reply, data, params};

/// How long a change waits for the migrate lock (the default of `migrate`).
const LOCK_TIMEOUT: Duration = Duration::from_secs(5);

/// Refuses every caller without the Administrator role: `403 permission_denied`, like a shared
/// saved view. Checked before the DocType is looked at, so a non-administrator learns nothing.
fn administrator(authenticated: &Authenticated) -> Result<(), ApiError> {
    if authenticated.caller().bypasses_permissions() {
        Ok(())
    } else {
        Err(ApiError::new(
            StatusCode::FORBIDDEN,
            "permission_denied",
            "permission denied: only an Administrator can customize fields",
        ))
    }
}

/// The DocType must exist on the site (a child DocType exists, and is refused as a target by
/// the rules of a custom field).
fn known(state: &AppState, doctype: &str) -> Result<(), ApiError> {
    if state.registry().get(doctype).is_some() {
        Ok(())
    } else {
        Err(ApiError::not_found("unknown doctype"))
    }
}

/// Rebuilds this server's registry after a change committed. A failure is only logged: the change
/// is made and the version poll catches up within the reload interval, so the caller must not be
/// told that a committed change failed.
async fn reload_after_commit(state: &AppState) {
    if let Err(error) = state.reload_registry().await {
        let _ = std::io::Write::write_fmt(
            &mut std::io::stderr(),
            format_args!(
                "warning: site `{}`: the change was saved but the registry could not be reloaded; the next poll will: {}\n",
                state.site_name(),
                error.message()
            ),
        );
    }
}

fn field_json(field: &CustomField) -> Result<Json, ApiError> {
    let time = |t: &time::OffsetDateTime, name: &str| {
        utc_rfc3339(t).map_err(|problem| {
            ApiError::from(crate::json::JsonError::Output {
                field: name.to_owned(),
                problem,
            })
        })
    };
    Ok(json!({
        "doctype": field.doctype,
        "name": field.name,
        "label": field.label(),
        "type": field.type_name(),
        "definition": field.definition,
        "enabled": field.enabled,
        "created_at": time(&field.created_at, "created_at")?,
        "modified_at": time(&field.modified_at, "modified_at")?,
        "modified_by": field.modified_by,
    }))
}

/// `GET /api/v1/customize/{doctype}/fields`: the custom fields of the DocType, enabled and
/// disabled, in the order they were added.
pub async fn list(
    State(state): State<AppState>,
    authenticated: Authenticated,
    path: Result<Path<String>, PathRejection>,
    query: RawQuery,
) -> Reply {
    let (Path(doctype), Query(query)) = (path?, query?);
    administrator(&authenticated)?;
    known(&state, &doctype)?;
    params(query, &[])?;
    let client = state.client().await?;
    let fields = forgebase_db::list_custom_fields(&**client, Some(&doctype)).await?;
    let fields = fields
        .iter()
        .map(field_json)
        .collect::<Result<Vec<_>, _>>()?;
    Ok(data(StatusCode::OK, Json::Array(fields)))
}

/// `POST /api/v1/customize/{doctype}/fields`: the body is the field as JSON with the keys of a
/// TOML `[[fields]]` entry. 201 with the stored field.
pub async fn add(
    State(state): State<AppState>,
    authenticated: Authenticated,
    path: Result<Path<String>, PathRejection>,
    query: RawQuery,
    ApiJson(body): ApiJson,
) -> Reply {
    let (Path(doctype), Query(query)) = (path?, query?);
    administrator(&authenticated)?;
    known(&state, &doctype)?;
    params(query, &[])?;
    let field = parse_field_json(&body).map_err(forgebase_db::DbError::CustomField)?;
    let mut client = state.client().await?;
    let stored = forgebase_db::add_custom_field(
        &mut client,
        state.base_registry(),
        state.site_name(),
        &doctype,
        field,
        authenticated.caller().name(),
        LOCK_TIMEOUT,
    )
    .await?;
    drop(client);
    reload_after_commit(&state).await;
    Ok(data(StatusCode::CREATED, field_json(&stored)?))
}

async fn set_enabled(
    state: AppState,
    authenticated: Authenticated,
    path: Result<Path<(String, String)>, PathRejection>,
    query: RawQuery,
    enabled: bool,
) -> Reply {
    let (Path((doctype, name)), Query(query)) = (path?, query?);
    administrator(&authenticated)?;
    known(&state, &doctype)?;
    params(query, &[])?;
    let mut client = state.client().await?;
    let stored = forgebase_db::set_custom_field_enabled(
        &mut client,
        &doctype,
        &name,
        enabled,
        authenticated.caller().name(),
        LOCK_TIMEOUT,
    )
    .await?;
    drop(client);
    reload_after_commit(&state).await;
    Ok(data(StatusCode::OK, field_json(&stored)?))
}

/// `POST /api/v1/customize/{doctype}/fields/{name}/disable`: hides the field everywhere; its
/// column and data stay. 200 with the field.
pub async fn disable(
    State(state): State<AppState>,
    authenticated: Authenticated,
    path: Result<Path<(String, String)>, PathRejection>,
    query: RawQuery,
) -> Reply {
    set_enabled(state, authenticated, path, query, false).await
}

/// `POST /api/v1/customize/{doctype}/fields/{name}/enable`: brings back a disabled field with
/// its data. 200 with the field.
pub async fn enable(
    State(state): State<AppState>,
    authenticated: Authenticated,
    path: Result<Path<(String, String)>, PathRejection>,
    query: RawQuery,
) -> Reply {
    set_enabled(state, authenticated, path, query, true).await
}
