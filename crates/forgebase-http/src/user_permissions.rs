//! The user permission routes (M10.3, ADR 0025): which documents a user is restricted to, for the
//! Administrator role only. `forgebase-db` does the work (the checks, the limit, the row); these
//! handlers check the role and map the body and the errors. A change applies on the affected
//! user's next request: the permissions are read with the roles of every authenticated request,
//! so no registry is reloaded.

use axum::extract::{Query, State};
use axum::http::StatusCode;
use forgebase_db::user_permissions::{
    UserPermission, add_user_permission, list_user_permissions, remove_user_permission,
};
use serde_json::{Map, Value as Json, json};

use crate::AppState;
use crate::auth::Authenticated;
use crate::error::ApiError;
use crate::json::{JsonError, utc_rfc3339};
use crate::routes::{ApiJson, RawQuery, Reply, data, object, params};

/// Refuses every caller without the Administrator role: `403 permission_denied`, as the customize
/// routes do. Checked before anything is looked at, so a non-administrator learns nothing.
fn administrator(authenticated: &Authenticated) -> Result<(), ApiError> {
    if authenticated.caller().bypasses_permissions() {
        Ok(())
    } else {
        Err(ApiError::new(
            StatusCode::FORBIDDEN,
            "permission_denied",
            "permission denied: only an Administrator can manage user permissions",
        ))
    }
}

fn permission_json(permission: &UserPermission) -> Result<Json, ApiError> {
    let created_at = utc_rfc3339(&permission.created_at).map_err(|problem| {
        ApiError::from(JsonError::Output {
            field: "created_at".to_owned(),
            problem,
        })
    })?;
    Ok(json!({
        "user": permission.user,
        "doctype": permission.doctype,
        "value": permission.value,
        "created_at": created_at,
    }))
}

/// `GET /api/v1/user-permissions?user=U&doctype=D`: the restrictions, sorted by user, DocType and
/// value; both parameters are optional filters. An unknown user is 404.
pub async fn list(
    State(state): State<AppState>,
    authenticated: Authenticated,
    query: RawQuery,
) -> Reply {
    let Query(query) = query?;
    administrator(&authenticated)?;
    let query = params(query, &["user", "doctype"])?;
    let client = state.client().await?;
    let rows = list_user_permissions(
        &**client,
        query.get("user").map(String::as_str),
        query.get("doctype").map(String::as_str),
    )
    .await?;
    let rows = rows
        .iter()
        .map(permission_json)
        .collect::<Result<Vec<_>, _>>()?;
    Ok(data(StatusCode::OK, Json::Array(rows)))
}

/// The three strings of a body `{"user", "doctype", "value"}`.
fn triple(body: Json) -> Result<(String, String, String), ApiError> {
    let map: Map<String, Json> = object(body, &["user", "doctype", "value"])?;
    let text = |key: &str| match map.get(key) {
        Some(Json::String(text)) => Ok(text.clone()),
        _ => Err(ApiError::bad_request(format!("`{key}` must be a string"))),
    };
    Ok((text("user")?, text("doctype")?, text("value")?))
}

/// `POST /api/v1/user-permissions`: body `{"user", "doctype", "value"}`. 201 with
/// `{"data": {"user", "doctype", "value", "added": true}}`, or 200 with `"added": false` when the
/// row existed already. 404 for an unknown user; 422 for an unknown or child DocType, a
/// document that does not exist, or the limit of 1000 documents per user and DocType.
pub async fn add(
    State(state): State<AppState>,
    authenticated: Authenticated,
    query: RawQuery,
    ApiJson(body): ApiJson,
) -> Reply {
    let Query(query) = query?;
    administrator(&authenticated)?;
    params(query, &[])?;
    let (user, doctype, value) = triple(body)?;
    let registry = state.registry();
    let mut client = state.client().await?;
    let added = add_user_permission(&mut **client, &registry, &user, &doctype, &value).await?;
    let status = if added {
        StatusCode::CREATED
    } else {
        StatusCode::OK
    };
    Ok(data(
        status,
        json!({"user": user, "doctype": doctype, "value": value, "added": added}),
    ))
}

/// `POST /api/v1/user-permissions/remove`: the same body. 200 with
/// `{"data": {"user", "doctype", "value", "removed": bool}}` (`false`: the user did not have the
/// row). 404 for an unknown user. (There is no `DELETE` with a body in this API: a delete is a
/// `POST` to `.../remove` here, as the other actions are, for example `.../disable`.)
pub async fn remove(
    State(state): State<AppState>,
    authenticated: Authenticated,
    query: RawQuery,
    ApiJson(body): ApiJson,
) -> Reply {
    let Query(query) = query?;
    administrator(&authenticated)?;
    params(query, &[])?;
    let (user, doctype, value) = triple(body)?;
    let mut client = state.client().await?;
    let removed = remove_user_permission(&mut **client, &user, &doctype, &value).await?;
    Ok(data(
        StatusCode::OK,
        json!({"user": user, "doctype": doctype, "value": value, "removed": removed}),
    ))
}
