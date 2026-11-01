//! Saved list views and the group-by summary (M8.3). The handlers only map JSON and parameters;
//! who may see or change a view, and what a summary may read, is decided by `forgebase-db`
//! (`views` and `summary`), which take the read rule from the same evaluator and list code as
//! the resource routes.

use axum::extract::rejection::PathRejection;
use axum::extract::{Path, Query, State};
use axum::http::StatusCode;
use axum::response::IntoResponse;
use forgebase_core::meta::DocType;
use forgebase_core::permission::Right;
use forgebase_db::{
    AggregateFn, Group, ListQuery, SummaryQuery, Uuid, View, ViewChanges, ViewDefinition,
};
use serde_json::{Map, Value as Json, json};

use crate::AppState;
use crate::auth::Authenticated;
use crate::error::ApiError;
use crate::json::{JsonError, from_value, parse_filters, parse_sort, utc_rfc3339};
use crate::routes::{
    ApiJson, RawQuery, Reply, data, doctype, object, params, query_error, require, version,
    version_param,
};

fn view_json(view: &View) -> Result<Json, ApiError> {
    Ok(json!({
        "id": view.id.hyphenated().to_string(),
        "label": view.label,
        "shared": view.shared(),
        "owner": view.owner,
        "definition": view.definition,
        "version": view.version,
        "modified_at": utc_rfc3339(&view.modified_at).map_err(|problem| JsonError::Output {
            field: "modified_at".to_owned(),
            problem,
        })?,
    }))
}

/// A view id from the path; one that is not a UUID names no view.
fn view_id(text: &str) -> Result<Uuid, ApiError> {
    text.parse()
        .map_err(|_| ApiError::not_found("saved view not found"))
}

fn problem(error: JsonError) -> ApiError {
    ApiError::validation(vec![error.to_string()])
}

/// The `definition` of a body: `{"filters", "sort", "columns"}`, each optional, in the forms of
/// the list API. The filters and the sort go through the parsers of the list route; the checks
/// against the DocType are `forgebase-db`'s.
fn definition(doctype: &DocType, json: Json) -> Result<ViewDefinition, ApiError> {
    let Json::Object(map) = json else {
        return Err(ApiError::validation(vec![
            "`definition` must be an object".to_owned(),
        ]));
    };
    if let Some(key) = map
        .keys()
        .find(|k| !matches!(k.as_str(), "filters" | "sort" | "columns"))
    {
        return Err(ApiError::validation(vec![format!(
            "unknown key {:?} in `definition` (use filters, sort, columns)",
            forgebase_core::shorten(key)
        )]));
    }
    let list = |key: &str| match map.get(key) {
        None => Ok(Vec::new()),
        Some(Json::Array(items)) => Ok(items.clone()),
        Some(_) => Err(ApiError::validation(vec![format!(
            "`definition.{key}` must be a list"
        )])),
    };
    let filters = list("filters")?;
    let strings = |key: &str| {
        list(key)?
            .into_iter()
            .map(|item| match item {
                Json::String(text) => Ok(text),
                _ => Err(ApiError::validation(vec![format!(
                    "`definition.{key}` must be a list of strings"
                )])),
            })
            .collect::<Result<Vec<String>, ApiError>>()
    };
    let (sort, columns) = (strings("sort")?, strings("columns")?);
    let query = ListQuery {
        filters: parse_filters(doctype, &Json::Array(filters.clone()).to_string())
            .map_err(problem)?,
        sort: parse_sort(&sort).map_err(problem)?,
        ..ListQuery::default()
    };
    Ok(ViewDefinition {
        json: json!({"filters": filters, "sort": sort, "columns": columns}),
        query,
    })
}

fn label(map: &Map<String, Json>) -> Result<Option<&str>, ApiError> {
    match map.get("label") {
        None => Ok(None),
        Some(Json::String(text)) => Ok(Some(text)),
        Some(_) => Err(ApiError::bad_request("`label` must be a string")),
    }
}

/// `GET /api/v1/views/{doctype}`: the views the caller sees, shared first.
pub async fn list(
    State(state): State<AppState>,
    authenticated: Authenticated,
    path: Result<Path<String>, PathRejection>,
    query: RawQuery,
) -> Reply {
    let (Path(name), Query(query)) = (path?, query?);
    let doctype = doctype(&state, &name)?;
    let caller = authenticated.caller();
    require(doctype, &caller, &[Right::Read], Right::Read)?;
    params(query, &[])?;
    let client = state.client().await?;
    let views =
        forgebase_db::list_views(&**client, state.registry(), &doctype.name, &caller).await?;
    let views = views.iter().map(view_json).collect::<Result<Vec<_>, _>>()?;
    Ok(data(StatusCode::OK, Json::Array(views)))
}

/// `POST /api/v1/views/{doctype}`: body `{"label", "definition", "shared"?}`; 201 with the view.
pub async fn create(
    State(state): State<AppState>,
    authenticated: Authenticated,
    path: Result<Path<String>, PathRejection>,
    query: RawQuery,
    ApiJson(body): ApiJson,
) -> Reply {
    let (Path(name), Query(query)) = (path?, query?);
    let doctype = doctype(&state, &name)?;
    let caller = authenticated.caller();
    require(doctype, &caller, &[Right::Read], Right::Read)?;
    params(query, &[])?;
    let mut map = object(body, &["label", "definition", "shared"])?;
    let shared = match map.get("shared") {
        None => false,
        Some(Json::Bool(shared)) => *shared,
        Some(_) => return Err(ApiError::bad_request("`shared` must be true or false")),
    };
    let Some(label) = label(&map)?.map(str::to_owned) else {
        return Err(ApiError::bad_request("`label` must be a string"));
    };
    let definition = match map.remove("definition") {
        Some(json) => definition(doctype, json)?,
        None => return Err(ApiError::bad_request("`definition` must be an object")),
    };
    let mut client = state.client().await?;
    let view = forgebase_db::create_view(
        &mut **client,
        state.registry(),
        &doctype.name,
        &caller,
        &label,
        &definition,
        shared,
    )
    .await?;
    Ok(data(StatusCode::CREATED, view_json(&view)?))
}

/// `PATCH /api/v1/views/{doctype}/{id}`: body `{"version", "label"?, "definition"?}`.
pub async fn update(
    State(state): State<AppState>,
    authenticated: Authenticated,
    path: Result<Path<(String, String)>, PathRejection>,
    query: RawQuery,
    ApiJson(body): ApiJson,
) -> Reply {
    let (Path((name, id)), Query(query)) = (path?, query?);
    let doctype = doctype(&state, &name)?;
    let caller = authenticated.caller();
    require(doctype, &caller, &[Right::Read], Right::Read)?;
    params(query, &[])?;
    let id = view_id(&id)?;
    let mut map = object(body, &["version", "label", "definition"])?;
    let expected_version = version(&map)?;
    let new_definition = map
        .remove("definition")
        .map(|json| definition(doctype, json))
        .transpose()?;
    let client = state.client().await?;
    let view = forgebase_db::update_view(
        &**client,
        state.registry(),
        &doctype.name,
        id,
        expected_version,
        &caller,
        &ViewChanges {
            label: label(&map)?,
            definition: new_definition.as_ref(),
        },
    )
    .await?;
    Ok(data(StatusCode::OK, view_json(&view)?))
}

/// `DELETE /api/v1/views/{doctype}/{id}?version=N`: 204 without a body.
pub async fn delete(
    State(state): State<AppState>,
    authenticated: Authenticated,
    path: Result<Path<(String, String)>, PathRejection>,
    query: RawQuery,
) -> Reply {
    let (Path((name, id)), Query(query)) = (path?, query?);
    let doctype = doctype(&state, &name)?;
    let caller = authenticated.caller();
    require(doctype, &caller, &[Right::Read], Right::Read)?;
    let expected_version = version_param(&params(query, &["version"])?)?;
    let id = view_id(&id)?;
    let client = state.client().await?;
    forgebase_db::delete_view(
        &**client,
        state.registry(),
        &doctype.name,
        id,
        expected_version,
        &caller,
    )
    .await?;
    Ok(StatusCode::NO_CONTENT.into_response())
}

/// `aggregates=sum:amount,avg:amount`: `FUNCTION:FIELD` pairs (the count is always there).
fn aggregates(text: &str) -> Result<Vec<(AggregateFn, String)>, ApiError> {
    if text.is_empty() {
        return Ok(Vec::new());
    }
    text.split(',')
        .map(|item| {
            let (function, field) = item.split_once(':').ok_or_else(|| {
                ApiError::invalid_query(format!(
                    "invalid aggregate {:?}: use FUNCTION:FIELD, for example sum:amount",
                    forgebase_core::shorten(item)
                ))
            })?;
            let function = AggregateFn::from_name(function).ok_or_else(|| {
                ApiError::invalid_query(format!(
                    "unknown aggregate function {:?} (use sum, avg, min or max; the count is always included)",
                    forgebase_core::shorten(function)
                ))
            })?;
            Ok((function, field.to_owned()))
        })
        .collect()
}

fn group_json(group: &Group) -> Result<Json, ApiError> {
    let mut values = Map::new();
    for (name, value) in &group.values {
        values.insert(name.clone(), from_value(name, value)?);
    }
    Ok(json!({
        "key": from_value("key", &group.key)?,
        "count": group.count,
        "values": values,
    }))
}

/// `GET /api/v1/summary/{doctype}?group_by=FIELD&aggregates=...&filters=...`: the documents the
/// caller may read, grouped. A separate path, because `/resource/{doctype}/summary` would
/// collide with a document named `summary`.
pub async fn summary(
    State(state): State<AppState>,
    authenticated: Authenticated,
    path: Result<Path<String>, PathRejection>,
    query: RawQuery,
) -> Reply {
    let (Path(name), Query(query)) = (path?, query?);
    let doctype = doctype(&state, &name)?;
    let caller = authenticated.caller();
    require(doctype, &caller, &[Right::Read], Right::Read)?;
    let query = params(query, &["group_by", "aggregates", "filters"])?;
    let group_by = query
        .get("group_by")
        .ok_or_else(|| ApiError::invalid_query("`group_by` is required"))?;
    let summary_query = SummaryQuery {
        filters: match query.get("filters") {
            Some(text) => parse_filters(doctype, text).map_err(query_error)?,
            None => Vec::new(),
        },
        group_by: group_by.clone(),
        aggregates: aggregates(query.get("aggregates").map_or("", String::as_str))?,
    };
    let client = state.client().await?;
    let summary = forgebase_db::summary(
        &**client,
        state.registry(),
        &doctype.name,
        &caller,
        &summary_query,
    )
    .await?;
    let groups = summary
        .groups
        .iter()
        .map(group_json)
        .collect::<Result<Vec<_>, _>>()?;
    Ok(data(
        StatusCode::OK,
        json!({"groups": groups, "truncated": summary.truncated}),
    ))
}
