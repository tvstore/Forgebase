//! The route handlers. Every handler that needs a user takes [`Authenticated`] first, so
//! authentication and CSRF are decided before the path, query or body is looked at.

use std::collections::BTreeMap;

use axum::Json as AxumJson;
use axum::extract::rejection::{PathRejection, QueryRejection};
use axum::extract::{FromRequest, Path, Query, Request, State};
use axum::http::header::SET_COOKIE;
use axum::http::{HeaderMap, HeaderValue, StatusCode};
use axum::response::{IntoResponse, Response};
use forgebase_core::document::{Changes, Document};
use forgebase_core::meta::DocType;
use forgebase_core::permission::{Access, Caller, Right, evaluate};
use forgebase_db::auth;
use forgebase_db::{DbError, ListQuery};
use serde_json::{Map, Value as Json, json};

use crate::AppState;
use crate::auth::{Authenticated, Credential, SESSION_COOKIE};
use crate::error::ApiError;
use crate::json::{
    JsonError, document_to_json, documents_to_json, input_from_json, parse_filters, parse_sort,
    utc_rfc3339,
};

/// Session lifetime in the cookie (`Max-Age`): the 7 days of the session's absolute expiry.
const COOKIE_MAX_AGE_SECS: u32 = 7 * 24 * 3600;

pub(crate) type Reply = Result<Response, ApiError>;

/// The raw query string as ordered pairs, so that a repeated parameter is still visible.
type RawQuery = Result<Query<Vec<(String, String)>>, QueryRejection>;

/// A JSON request body. Unlike axum's own rejections, a failure is an [`ApiError`] (the
/// envelope): 413 over the body limit, 415 for another content type, 400 for bad JSON.
pub struct ApiJson(Json);

impl FromRequest<AppState> for ApiJson {
    type Rejection = ApiError;

    async fn from_request(request: Request, state: &AppState) -> Result<Self, Self::Rejection> {
        Ok(Self(
            AxumJson::<Json>::from_request(request, state).await?.0,
        ))
    }
}

pub(crate) fn data(status: StatusCode, value: Json) -> Response {
    (status, AxumJson(json!({ "data": value }))).into_response()
}

pub(crate) fn doctype<'a>(state: &'a AppState, name: &str) -> Result<&'a DocType, ApiError> {
    state
        .registry()
        .get(name)
        .filter(|d| !d.is_child)
        .ok_or_else(|| ApiError::not_found("unknown doctype"))
}

/// Refuses with `permission_denied` when the caller has no DocType-level access for one of
/// `rights`, before the body or the query is mapped. Mapping errors name fields, so they must
/// not reach a caller who may not use the DocType at all. This asks the one evaluator of
/// `forgebase-core` and decides nothing itself; `forgebase-db` still enforces every right.
fn require(
    doctype: &DocType,
    caller: &Caller,
    rights: &[Right],
    reported: Right,
) -> Result<(), ApiError> {
    if rights
        .iter()
        .any(|right| evaluate(doctype, caller, *right) == Access::Denied)
    {
        return Err(DbError::PermissionDenied {
            doctype: doctype.name.clone(),
            right: reported,
        }
        .into());
    }
    Ok(())
}

/// A problem with the `filters` or `sort` parameter (not with a body).
fn query_error(error: JsonError) -> ApiError {
    match error {
        JsonError::Syntax(_) => ApiError::invalid_query("invalid filter: not valid JSON"),
        other => other.into(),
    }
}

/// The session cookie with the attributes of the owner decision: `HttpOnly`, `Secure` (unless
/// the explicit development mode), `SameSite=Lax`, `Path=/`.
fn cookie(state: &AppState, value: &str, max_age: u32) -> Result<HeaderValue, ApiError> {
    let secure = if state.config().secure_cookies {
        " Secure;"
    } else {
        ""
    };
    HeaderValue::from_str(&format!(
        "{SESSION_COOKIE}={value}; HttpOnly;{secure} SameSite=Lax; Path=/; Max-Age={max_age}"
    ))
    .map_err(|e| ApiError::internal(&e))
}

/// A JSON object with no keys other than `allowed`.
fn object(json: Json, allowed: &[&str]) -> Result<Map<String, Json>, ApiError> {
    let Json::Object(map) = json else {
        return Err(ApiError::bad_request("the body must be a JSON object"));
    };
    if let Some(key) = map.keys().find(|k| !allowed.contains(&k.as_str())) {
        return Err(ApiError::bad_request(format!("unknown key {key:?}")));
    }
    Ok(map)
}

fn version(map: &Map<String, Json>) -> Result<i64, ApiError> {
    map.get("version")
        .and_then(Json::as_i64)
        .ok_or_else(|| ApiError::bad_request("`version` must be an integer"))
}

fn version_param(query: &BTreeMap<String, String>) -> Result<i64, ApiError> {
    query
        .get("version")
        .and_then(|v| v.parse().ok())
        .ok_or_else(|| ApiError::invalid_query("`version` must be an integer"))
}

/// The query parameters as a map, `invalid_query` for a parameter that is not in `allowed` or
/// that is repeated (`?limit=1&limit=500` must not depend on which one a parser keeps).
fn params(
    pairs: Vec<(String, String)>,
    allowed: &[&str],
) -> Result<BTreeMap<String, String>, ApiError> {
    let mut map = BTreeMap::new();
    for (key, value) in pairs {
        if !allowed.contains(&key.as_str()) {
            return Err(ApiError::invalid_query(format!(
                "unknown parameter {key:?}"
            )));
        }
        if map.contains_key(&key) {
            return Err(ApiError::invalid_query(format!(
                "duplicate parameter {key:?}"
            )));
        }
        map.insert(key, value);
    }
    Ok(map)
}

// ---------------------------------------------------------------------------------------------
// Login and logout
// ---------------------------------------------------------------------------------------------

/// `POST /api/v1/login`. Not authenticated (and so not CSRF-protected: there is no session yet).
pub async fn login(State(state): State<AppState>, ApiJson(body): ApiJson) -> Reply {
    let map = object(body, &["username", "password"])?;
    let text = |key: &str| map.get(key).and_then(Json::as_str);
    let (Some(username), Some(password)) = (text("username"), text("password")) else {
        return Err(ApiError::bad_request(
            "`username` and `password` must be strings",
        ));
    };
    // Bounds the Argon2 work (memory) and the connections that logins hold. Not waiting: an
    // excess login is turned away so an attacker cannot queue requests.
    let _permit = state
        .logins()
        .try_acquire()
        .map_err(|_| ApiError::too_many_logins())?;
    let mut client = state.client().await?;
    let session = auth::login(&mut **client, username, password).await?;
    let header = cookie(&state, &session.token, COOKIE_MAX_AGE_SECS)?;
    let body = data(
        StatusCode::OK,
        json!({
            "username": session.user.username,
            "roles": session.user.roles,
            "csrf_token": session.csrf_token,
        }),
    );
    Ok(([(SET_COOKIE, header)], body).into_response())
}

/// `POST /api/v1/logout`: ends the cookie session and clears the cookie. A request that
/// authenticated with an API token has no session to end: 400.
pub async fn logout(State(state): State<AppState>, authenticated: Authenticated) -> Reply {
    let Credential::Session(token) = authenticated.credential else {
        return Err(ApiError::bad_request(
            "logout needs a cookie session; revoke an API token with the command-line tool",
        ));
    };
    let client = state.client().await?;
    auth::logout(&**client, &token).await?;
    let header = cookie(&state, "", 0)?;
    Ok((StatusCode::NO_CONTENT, [(SET_COOKIE, header)]).into_response())
}

/// `GET /api/v1/session`: the user of the cookie session and a **new** CSRF token, so that the
/// desk can keep the token in memory only and recover it after a page reload. The session's
/// stored CSRF digest is replaced: the previous token (also the one another tab holds) stops
/// working, and a request that fails with `csrf_failed` can call this once and retry. Safe
/// because the response cannot be read cross-origin (no CORS) and the cookie is `SameSite=Lax`.
/// An API token has no session: 400.
///
/// The request must carry `X-Forgebase-CSRF: refresh`, else 400 `bad_request` and nothing is
/// rotated. A navigation or an `<img>` cannot send a custom header, and a cross-origin `fetch`
/// with it needs a preflight, which this server answers with 405 and no CORS headers; so a
/// foreign page cannot make the desk's in-memory token stop working.
pub async fn session(
    State(state): State<AppState>,
    authenticated: Authenticated,
    headers: HeaderMap,
) -> Reply {
    // Exactly one value: a repeated header must not depend on which one a parser keeps.
    let mut values = headers.get_all("x-forgebase-csrf").iter();
    let marked = matches!(
        (values.next(), values.next()),
        (Some(value), None) if value.as_bytes() == b"refresh"
    );
    if !marked {
        return Err(ApiError::bad_request(
            "this request needs the header X-Forgebase-CSRF: refresh",
        ));
    }
    let Credential::Session(token) = &authenticated.credential else {
        return Err(ApiError::bad_request(
            "a session needs a cookie; an API token has none",
        ));
    };
    let client = state.client().await?;
    // `None`: the session ended between the check and now; the same 401 as any other.
    let csrf_token = auth::rotate_csrf(&**client, token)
        .await?
        .ok_or_else(ApiError::unauthenticated)?;
    Ok(data(
        StatusCode::OK,
        json!({
            "username": authenticated.user.username,
            "roles": authenticated.user.roles,
            "csrf_token": csrf_token,
        }),
    ))
}

// ---------------------------------------------------------------------------------------------
// Resources
// ---------------------------------------------------------------------------------------------

/// `GET /api/v1/resource/{doctype}` with `filters`, `sort`, `limit` and `offset`.
pub async fn list(
    State(state): State<AppState>,
    authenticated: Authenticated,
    path: Result<Path<String>, PathRejection>,
    query: RawQuery,
) -> Reply {
    let (Path(name), Query(query)) = (path?, query?);
    let doctype = doctype(&state, &name)?;
    require(
        doctype,
        &authenticated.caller(),
        &[Right::Read],
        Right::Read,
    )?;
    let query = params(query, &["filters", "sort", "limit", "offset"])?;
    let list_query = ListQuery {
        filters: match query.get("filters") {
            Some(text) => parse_filters(doctype, text).map_err(query_error)?,
            None => Vec::new(),
        },
        sort: match query.get("sort") {
            Some(text) => parse_sort(&text.split(',').map(str::to_owned).collect::<Vec<_>>())
                .map_err(query_error)?,
            None => Vec::new(),
        },
        limit: match query.get("limit") {
            Some(text) => Some(
                text.parse()
                    .map_err(|_| ApiError::invalid_query("`limit` must be a positive integer"))?,
            ),
            None => None,
        },
        offset: match query.get("offset") {
            Some(text) => {
                Some(text.parse().map_err(|_| {
                    ApiError::invalid_query("`offset` must be a non-negative integer")
                })?)
            }
            None => None,
        },
    };
    let client = state.client().await?;
    let documents = forgebase_db::list(
        &**client,
        state.registry(),
        &doctype.name,
        &authenticated.caller(),
        &list_query,
    )
    .await?;
    Ok(data(StatusCode::OK, documents_to_json(&documents)?))
}

/// `POST /api/v1/resource/{doctype}`: the body is the document.
pub async fn create(
    State(state): State<AppState>,
    authenticated: Authenticated,
    path: Result<Path<String>, PathRejection>,
    query: RawQuery,
    ApiJson(body): ApiJson,
) -> Reply {
    let (Path(name), Query(query)) = (path?, query?);
    let doctype = doctype(&state, &name)?;
    require(
        doctype,
        &authenticated.caller(),
        &[Right::Create],
        Right::Create,
    )?;
    params(query, &[])?;
    let input = input_from_json(state.registry(), doctype, body)?;
    let document = Document {
        name: None,
        values: input.values,
        tables: input.tables,
    };
    let mut client = state.client().await?;
    let stored = forgebase_db::insert(
        &mut **client,
        state.registry(),
        &doctype.name,
        document,
        &authenticated.caller(),
    )
    .await?;
    Ok(data(StatusCode::CREATED, document_to_json(&stored)?))
}

/// `GET /api/v1/resource/{doctype}/{name}`.
pub async fn read(
    State(state): State<AppState>,
    authenticated: Authenticated,
    path: Result<Path<(String, String)>, PathRejection>,
    query: RawQuery,
) -> Reply {
    let (Path((doctype_name, name)), Query(query)) = (path?, query?);
    let doctype = doctype(&state, &doctype_name)?;
    params(query, &[])?;
    let mut client = state.client().await?;
    let stored = forgebase_db::get(
        &mut **client,
        state.registry(),
        &doctype.name,
        &name,
        &authenticated.caller(),
    )
    .await?
    .ok_or_else(|| {
        ApiError::from(forgebase_db::DbError::NotFound {
            doctype: doctype.name.clone(),
            name: name.clone(),
        })
    })?;
    Ok(data(StatusCode::OK, document_to_json(&stored)?))
}

/// `PATCH /api/v1/resource/{doctype}/{name}`: body `{"version": N, "data": {changes}}`.
pub async fn update(
    State(state): State<AppState>,
    authenticated: Authenticated,
    path: Result<Path<(String, String)>, PathRejection>,
    query: RawQuery,
    ApiJson(body): ApiJson,
) -> Reply {
    let (Path((doctype_name, name)), Query(query)) = (path?, query?);
    let doctype = doctype(&state, &doctype_name)?;
    require(
        doctype,
        &authenticated.caller(),
        &[Right::Write, Right::Read],
        Right::Write,
    )?;
    params(query, &[])?;
    let mut map = object(body, &["version", "data"])?;
    let expected_version = version(&map)?;
    let changes = map
        .remove("data")
        .ok_or_else(|| ApiError::bad_request("`data` must be a JSON object"))?;
    let input = input_from_json(state.registry(), doctype, changes)?;
    let changes = Changes {
        values: input.values,
        tables: input.tables,
    };
    let mut client = state.client().await?;
    let stored = forgebase_db::update(
        &mut **client,
        state.registry(),
        &doctype.name,
        &name,
        expected_version,
        &changes,
        &authenticated.caller(),
    )
    .await?;
    Ok(data(StatusCode::OK, document_to_json(&stored)?))
}

/// `DELETE /api/v1/resource/{doctype}/{name}?version=N`: 204 without a body.
pub async fn delete(
    State(state): State<AppState>,
    authenticated: Authenticated,
    path: Result<Path<(String, String)>, PathRejection>,
    query: RawQuery,
) -> Reply {
    let (Path((doctype_name, name)), Query(query)) = (path?, query?);
    let doctype = doctype(&state, &doctype_name)?;
    let expected_version = version_param(&params(query, &["version"])?)?;
    let mut client = state.client().await?;
    forgebase_db::delete(
        &mut **client,
        state.registry(),
        &doctype.name,
        &name,
        expected_version,
        &authenticated.caller(),
    )
    .await?;
    Ok(StatusCode::NO_CONTENT.into_response())
}

#[derive(Clone, Copy)]
enum Lifecycle {
    Submit,
    Cancel,
    Amend,
}

async fn lifecycle(
    kind: Lifecycle,
    state: AppState,
    authenticated: Authenticated,
    path: Result<Path<(String, String)>, PathRejection>,
    query: RawQuery,
    body: Json,
) -> Reply {
    let (Path((doctype_name, name)), Query(query)) = (path?, query?);
    let doctype = doctype(&state, &doctype_name)?;
    params(query, &[])?;
    let expected_version = version(&object(body, &["version"])?)?;
    let caller = authenticated.caller();
    let mut client = state.client().await?;
    let client = &mut **client;
    let (registry, doctype, name) = (state.registry(), doctype.name.as_str(), name.as_str());
    let (status, stored) = match kind {
        Lifecycle::Submit => (
            StatusCode::OK,
            forgebase_db::submit(client, registry, doctype, name, expected_version, &caller)
                .await?,
        ),
        Lifecycle::Cancel => (
            StatusCode::OK,
            forgebase_db::cancel(client, registry, doctype, name, expected_version, &caller)
                .await?,
        ),
        // The amendment is a new document.
        Lifecycle::Amend => (
            StatusCode::CREATED,
            forgebase_db::amend(client, registry, doctype, name, expected_version, &caller).await?,
        ),
    };
    Ok(data(status, document_to_json(&stored)?))
}

/// `POST /api/v1/resource/{doctype}/{name}/submit`: body `{"version": N}`.
pub async fn submit(
    State(state): State<AppState>,
    authenticated: Authenticated,
    path: Result<Path<(String, String)>, PathRejection>,
    query: RawQuery,
    ApiJson(body): ApiJson,
) -> Reply {
    lifecycle(Lifecycle::Submit, state, authenticated, path, query, body).await
}

/// `POST /api/v1/resource/{doctype}/{name}/cancel`: body `{"version": N}`.
pub async fn cancel(
    State(state): State<AppState>,
    authenticated: Authenticated,
    path: Result<Path<(String, String)>, PathRejection>,
    query: RawQuery,
    ApiJson(body): ApiJson,
) -> Reply {
    lifecycle(Lifecycle::Cancel, state, authenticated, path, query, body).await
}

/// `POST /api/v1/resource/{doctype}/{name}/amend`: body `{"version": N}`; 201 with the new
/// document.
pub async fn amend(
    State(state): State<AppState>,
    authenticated: Authenticated,
    path: Result<Path<(String, String)>, PathRejection>,
    query: RawQuery,
    ApiJson(body): ApiJson,
) -> Reply {
    lifecycle(Lifecycle::Amend, state, authenticated, path, query, body).await
}

// ---------------------------------------------------------------------------------------------
// Workflow (M8.1, ADR 0015)
// ---------------------------------------------------------------------------------------------

/// `POST /api/v1/resource/{doctype}/{name}/transition`: body `{"action": "...", "version": N}`.
/// 200 with the document as stored, like submit. Who may take it is decided by `forgebase-db`
/// (the transition's role and the right of its docstatus move).
pub async fn transition(
    State(state): State<AppState>,
    authenticated: Authenticated,
    path: Result<Path<(String, String)>, PathRejection>,
    query: RawQuery,
    ApiJson(body): ApiJson,
) -> Reply {
    let (Path((doctype_name, name)), Query(query)) = (path?, query?);
    let doctype = doctype(&state, &doctype_name)?;
    params(query, &[])?;
    let map = object(body, &["action", "version"])?;
    let expected_version = version(&map)?;
    let action = map
        .get("action")
        .and_then(Json::as_str)
        .ok_or_else(|| ApiError::bad_request("`action` must be a string"))?;
    let mut client = state.client().await?;
    let stored = forgebase_db::transition(
        &mut **client,
        state.registry(),
        &doctype.name,
        &name,
        action,
        expected_version,
        &authenticated.caller(),
    )
    .await?;
    Ok(data(StatusCode::OK, document_to_json(&stored)?))
}

/// `GET /api/v1/resource/{doctype}/{name}/transitions`: the state of the document and the
/// transitions from it that the caller may take now (role and right, `if_owner` applied).
/// Hooks are not run, so a hook may still refuse one.
pub async fn transitions(
    State(state): State<AppState>,
    authenticated: Authenticated,
    path: Result<Path<(String, String)>, PathRejection>,
    query: RawQuery,
) -> Reply {
    let (Path((doctype_name, name)), Query(query)) = (path?, query?);
    let doctype = doctype(&state, &doctype_name)?;
    params(query, &[])?;
    let mut client = state.client().await?;
    let available = forgebase_db::available_transitions(
        &mut **client,
        state.registry(),
        &doctype.name,
        &name,
        &authenticated.caller(),
    )
    .await?;
    let transitions: Vec<Json> = available
        .transitions
        .iter()
        .map(|t| json!({"action": t.action, "to": t.to}))
        .collect();
    Ok(data(
        StatusCode::OK,
        json!({"state": available.state, "transitions": transitions}),
    ))
}

/// `GET /api/v1/resource/{doctype}/{name}/history`: the transitions of the document, oldest
/// first, at most the newest 500.
pub async fn history(
    State(state): State<AppState>,
    authenticated: Authenticated,
    path: Result<Path<(String, String)>, PathRejection>,
    query: RawQuery,
) -> Reply {
    let (Path((doctype_name, name)), Query(query)) = (path?, query?);
    let doctype = doctype(&state, &doctype_name)?;
    params(query, &[])?;
    let mut client = state.client().await?;
    let entries = forgebase_db::history(
        &mut **client,
        state.registry(),
        &doctype.name,
        &name,
        &authenticated.caller(),
    )
    .await?;
    let history = entries
        .iter()
        .map(|e| {
            Ok(json!({
                "action": e.action,
                "from": e.from,
                "to": e.to,
                "actor": e.actor,
                "at": utc_rfc3339(&e.at).map_err(|problem| JsonError::Output {
                    field: "at".to_owned(),
                    problem,
                })?,
            }))
        })
        .collect::<Result<Vec<_>, JsonError>>()?;
    Ok(data(StatusCode::OK, json!({"history": history})))
}
