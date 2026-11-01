//! Query reports (M8.4, ADR 0016): the list of the reports a caller may run, and running one.
//! The handlers map JSON; who may run a report, the parameter checks and the read-only run are
//! decided by `forgebase-core` and `forgebase-db`. **The SQL is never sent**, not in a list and
//! not in an error.

use std::collections::BTreeMap;

use axum::extract::rejection::PathRejection;
use axum::extract::{Json as AxumJson, Path, Query, State};
use axum::http::StatusCode;
use axum::response::IntoResponse;
use forgebase_core::meta::Registry;
use forgebase_core::permission::{Caller, can_run_report};
use forgebase_core::report::{ColumnKind, ParamKind, ReportColumn, ReportDef, ReportParam};
use forgebase_core::value::Value;
use forgebase_db::ReportConnection;
use serde_json::{Map, Value as Json, json};

use crate::AppState;
use crate::auth::Authenticated;
use crate::error::ApiError;
use crate::json::{from_value, param_value};
use crate::routes::{ApiJson, RawQuery, Reply, data, link_titles_flag, object, params};

fn param_json(param: &ReportParam) -> Result<Json, ApiError> {
    let mut map = Map::new();
    map.insert("name".to_owned(), json!(param.name));
    map.insert("label".to_owned(), json!(param.label));
    map.insert("kind".to_owned(), json!(param.kind.name()));
    map.insert("required".to_owned(), json!(param.required));
    map.insert(
        "default".to_owned(),
        match &param.default {
            Some(value) => from_value(&param.name, value)?,
            None => Json::Null,
        },
    );
    match &param.kind {
        ParamKind::Link { doctype } => {
            map.insert("doctype".to_owned(), json!(doctype));
        }
        ParamKind::Select { options } => {
            map.insert("options".to_owned(), json!(options));
        }
        _ => {}
    }
    Ok(Json::Object(map))
}

fn column_json(column: &ReportColumn) -> Json {
    let mut map = Map::new();
    map.insert("name".to_owned(), json!(column.name));
    map.insert("label".to_owned(), json!(column.label));
    map.insert("kind".to_owned(), json!(column.kind.name()));
    if let ColumnKind::Link { doctype } = &column.kind {
        map.insert("doctype".to_owned(), json!(doctype));
    }
    Json::Object(map)
}

pub(crate) fn columns_json(report: &ReportDef) -> Json {
    Json::Array(report.columns.iter().map(column_json).collect())
}

/// `GET /api/v1/report`: the reports the caller may run, by label (byte order, then name), with
/// their parameters and columns.
pub async fn list(
    State(state): State<AppState>,
    authenticated: Authenticated,
    query: RawQuery,
) -> Reply {
    let Query(query) = query?;
    params(query, &[])?;
    let caller = authenticated.caller();
    let registry = state.registry();
    let mut reports: Vec<&ReportDef> = registry
        .reports()
        .filter(|report| can_run_report(&report.roles, &caller))
        .collect();
    reports.sort_by(|a, b| {
        (a.label.as_bytes(), a.full_name()).cmp(&(b.label.as_bytes(), b.full_name()))
    });
    let mut items = Vec::with_capacity(reports.len());
    for report in reports {
        let params = report
            .params
            .iter()
            .map(param_json)
            .collect::<Result<Vec<_>, _>>()?;
        items.push(json!({
            "name": report.full_name(),
            "app": report.app,
            "label": report.label,
            "params": params,
            "columns": columns_json(report),
        }));
    }
    Ok(data(StatusCode::OK, Json::Array(items)))
}

/// The report `name` if the caller may run it, else the one `not_found` of every report that
/// does not exist or is not for them. Before the body is mapped: the parameter names of a report
/// the caller may not run are not for them. This asks the one evaluator of `forgebase-core`;
/// `forgebase-db` checks again.
pub(crate) fn find_report<'a>(
    registry: &'a Registry,
    name: &str,
    caller: &Caller,
) -> Result<&'a ReportDef, ApiError> {
    registry
        .report(name)
        .filter(|report| can_run_report(&report.roles, caller))
        .ok_or_else(|| ApiError::not_found("report not found"))
}

/// The parameter values of a report run from its request body `{"params": {..}}` (optional).
pub(crate) fn report_params(
    report: &ReportDef,
    body: Json,
) -> Result<BTreeMap<String, Value>, ApiError> {
    let mut map = object(body, &["params"])?;
    let given = match map.remove("params") {
        None | Some(Json::Null) => Map::new(),
        Some(Json::Object(given)) => given,
        Some(_) => return Err(ApiError::bad_request("`params` must be an object")),
    };
    let mut values = BTreeMap::new();
    let mut problems = Vec::new();
    for (key, json) in &given {
        match report.params.iter().find(|p| &p.name == key) {
            Some(param) => match param_value(key, &param.kind, json) {
                Ok(value) => {
                    values.insert(key.clone(), value);
                }
                Err(error) => problems.push(error.to_string()),
            },
            // Reported by the checks of the report as an unknown parameter.
            None => {
                values.insert(key.clone(), Value::Null);
            }
        }
    }
    if !problems.is_empty() {
        return Err(ApiError::validation(problems));
    }
    Ok(values)
}

/// `POST /api/v1/report/{app}.{name}` with `{"params": {..}}` (optional): runs the report and
/// returns `{"data": {"columns", "rows", "truncated"}}`. `?link_titles=1` adds the titles of the
/// Link columns as a sibling `link_titles` key, under the caller's read rules.
pub async fn run(
    State(state): State<AppState>,
    authenticated: Authenticated,
    path: Result<Path<String>, PathRejection>,
    query: RawQuery,
    ApiJson(body): ApiJson,
) -> Reply {
    let (Path(name), Query(query)) = (path?, query?);
    let caller = authenticated.caller();
    let registry = state.registry();
    let report = find_report(&registry, &name, &caller)?;
    let query = params(query, &["link_titles"])?;
    let titles = link_titles_flag(&query)?;
    let values = report_params(report, body)?;
    // A connection that is closed afterwards, not pooled: the report's SQL may leave session
    // state behind (`ReportConnection`).
    let mut client = ReportConnection::get(state.pool()).await?;
    let result = forgebase_db::run_report(&mut client, report, &caller, &values).await?;
    let mut rows = Vec::with_capacity(result.rows.len());
    for row in &result.rows {
        let mut cells = Vec::with_capacity(row.len());
        for (value, column) in row.iter().zip(&report.columns) {
            cells.push(from_value(&column.name, value)?);
        }
        rows.push(Json::Array(cells));
    }
    let body = json!({
        "data": {
            "columns": columns_json(report),
            "rows": rows,
            "truncated": result.truncated,
        }
    });
    let body = if titles {
        // The connection of this request, as in `with_link_titles`: a second one could never be
        // had when every pool slot is held by a request like this.
        let link_titles =
            forgebase_db::report_link_titles(&*client, &registry, &caller, report, &result.rows)
                .await?;
        let mut body = body;
        body["link_titles"] = json!(link_titles);
        body
    } else {
        body
    };
    Ok((StatusCode::OK, AxumJson(body)).into_response())
}
