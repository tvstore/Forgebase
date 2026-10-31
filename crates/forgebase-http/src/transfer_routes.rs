//! The routes of import and export (M9.3, ADR 0021): `GET /api/v1/export/{doctype}`,
//! `POST /api/v1/report/{app}.{name}/export` and `POST /api/v1/import/{doctype}`. The formats
//! are in [`crate::transfer`]; the rights, the snapshot and the transaction are in
//! `forgebase-db`. These handlers map requests to them.

use std::collections::BTreeMap;
use std::io;

use axum::body::{Body, Bytes};
use axum::extract::rejection::PathRejection;
use axum::extract::{FromRequest, Path, Query, Request, State};
use axum::http::header::{CACHE_CONTROL, CONTENT_DISPOSITION, CONTENT_TYPE};
use axum::http::{HeaderMap, HeaderValue, StatusCode};
use axum::response::Response;
use forgebase_core::permission::Right;
use forgebase_db::{
    DbError, Export, ExportQuery, ImportMode, ListQuery, ReportConnection, begin_export,
};
use http_body_util::channel::{Channel, Sender};
use serde_json::{Value as Json, json};
use time::OffsetDateTime;
use time::format_description;
use tokio::sync::oneshot;

use crate::AppState;
use crate::auth::Authenticated;
use crate::error::ApiError;
use crate::json::{from_value, parse_filters, parse_sort};
use crate::reports::{columns_json, find_report, report_params};
use crate::routes::{ApiJson, RawQuery, Reply, data, doctype, params, query_error, require};
use crate::transfer::csv_cell;
use crate::transfer::{Exporter, FileError, Format, parse_import, push_csv_record, run_import};

/// The `format` parameter: `csv` (the default) or `json`.
fn format_param(query: &BTreeMap<String, String>) -> Result<Format, ApiError> {
    match query.get("format") {
        None => Ok(Format::Csv),
        Some(text) => Format::parse(text)
            .ok_or_else(|| ApiError::invalid_query("`format` must be csv or json")),
    }
}

/// `attachment; filename="<name>-<UTC timestamp>.<ext>"`. The name is a DocType or report name:
/// lower-case letters, digits, `_` and `.`.
fn disposition(name: &str, format: Format) -> Result<HeaderValue, ApiError> {
    let pattern =
        format_description::parse_borrowed::<2>("[year][month][day]T[hour][minute][second]Z")
            .map_err(|e| ApiError::internal(&e))?;
    let stamp = OffsetDateTime::now_utc()
        .format(&pattern)
        .map_err(|e| ApiError::internal(&e))?;
    HeaderValue::from_str(&format!(
        "attachment; filename=\"{name}-{stamp}.{}\"",
        format.extension()
    ))
    .map_err(|e| ApiError::internal(&e))
}

/// The head of a file download: the type, the name, and `no-store`.
fn file_response(
    name: &str,
    format: Format,
    body: Body,
    extra: &[(&'static str, String)],
) -> Reply {
    let mut response = Response::new(body);
    let headers = response.headers_mut();
    headers.insert(
        CONTENT_TYPE,
        HeaderValue::from_static(format.content_type()),
    );
    headers.insert(CONTENT_DISPOSITION, disposition(name, format)?);
    headers.insert(CACHE_CONTROL, HeaderValue::from_static("no-store"));
    for (key, value) in extra {
        headers.insert(
            *key,
            HeaderValue::from_str(value).map_err(|e| ApiError::internal(&e))?,
        );
    }
    Ok(response)
}

// ---------------------------------------------------------------------------------------------
// Export of documents
// ---------------------------------------------------------------------------------------------

/// `GET /api/v1/export/{doctype}` with `format` (`csv` or `json`), `filters`, `sort` and `fields`
/// (comma separated). The caller needs the read and the export right. The answer is a streamed
/// file from one `REPEATABLE READ` snapshot; the rows are counted first, and more than
/// [`crate::AppConfig::export_max_rows`] is `422 export_too_large` before any byte is sent.
pub async fn export(
    State(state): State<AppState>,
    authenticated: Authenticated,
    path: Result<Path<String>, PathRejection>,
    query: RawQuery,
) -> Reply {
    let (Path(name), Query(query)) = (path?, query?);
    let doctype = doctype(&state, &name)?;
    let caller = authenticated.caller();
    require(doctype, &caller, &[Right::Read], Right::Read)?;
    require(doctype, &caller, &[Right::Export], Right::Export)?;
    let query = params(query, &["format", "filters", "sort", "fields"])?;
    let format = format_param(&query)?;
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
        limit: None,
        offset: None,
    };
    let fields: Option<Vec<String>> = query
        .get("fields")
        .map(|text| text.split(',').map(str::to_owned).collect());
    let exporter = Exporter::new(doctype, format, fields.as_deref()).map_err(query_error)?;

    let client = state
        .pool()
        .get()
        .await
        .map_err(|e| ApiError::internal(&e))?;
    // The task owns the connection and the snapshot; the handler waits only for the count.
    let (started_tx, started_rx) = oneshot::channel();
    let (sender, channel) = Channel::<Bytes, io::Error>::new(2);
    let job = ExportJob {
        registry: state.registry_arc().clone(),
        doctype: doctype.name.clone(),
        caller,
        filters: list_query.filters,
        sort: list_query.sort,
        limit: state.config().export_max_rows,
        exporter,
    };
    tokio::spawn(stream_export(client, job, started_tx, sender));
    let count = match started_rx.await {
        Ok(Ok(count)) => count,
        Ok(Err(error)) => return Err(error.into()),
        Err(_) => {
            return Err(ApiError::internal(
                &"the export task ended before it started",
            ));
        }
    };
    file_response(
        &doctype.name,
        format,
        Body::new(channel),
        &[("x-forgebase-row-count", count.to_string())],
    )
}

/// Everything the streaming task needs, owned.
struct ExportJob {
    registry: std::sync::Arc<forgebase_core::meta::Registry>,
    doctype: String,
    caller: forgebase_core::permission::Caller,
    filters: Vec<forgebase_db::Filter>,
    sort: Vec<forgebase_db::Sort>,
    limit: u64,
    exporter: Exporter,
}

/// Runs the export in its own task: opens the snapshot, counts (the answer to the handler, so a
/// refusal is a normal error response), then writes the body in chunks. A database error
/// halfway aborts the body: the connection is closed and the client sees an incomplete body, so
/// a client must treat that as a failure.
async fn stream_export<C>(
    mut client: C,
    mut job: ExportJob,
    started: oneshot::Sender<Result<u64, DbError>>,
    mut sender: Sender<Bytes, io::Error>,
) where
    C: std::ops::DerefMut,
    C::Target: std::ops::DerefMut<Target = forgebase_db::Client>,
{
    let tx = match begin_export(&mut client).await {
        Ok(tx) => tx,
        Err(error) => {
            let _ = started.send(Err(error));
            return;
        }
    };
    let query = ExportQuery {
        doctype: &job.doctype,
        filters: &job.filters,
        sort: &job.sort,
        limit: job.limit,
        with_tables: job.exporter.with_tables(),
    };
    let mut export = match Export::start(&tx, &job.registry, &job.caller, &query).await {
        Ok(export) => export,
        Err(error) => {
            let _ = started.send(Err(error));
            return;
        }
    };
    if started.send(Ok(export.count())).is_err() {
        return; // the request is gone
    }
    if sender
        .send_data(Bytes::from(job.exporter.start()))
        .await
        .is_err()
    {
        return;
    }
    loop {
        let page = match export.next_page(&tx, &job.registry).await {
            Ok(Some(page)) => page,
            Ok(None) => break,
            Err(error) => {
                let _ = io::Write::write_fmt(
                    &mut io::stderr(),
                    format_args!("export failed halfway: {error}\n"),
                );
                sender.abort(io::Error::other("the export failed"));
                return;
            }
        };
        let text = match job.exporter.rows(&page) {
            Ok(text) => text,
            Err(error) => {
                let _ = io::Write::write_fmt(
                    &mut io::stderr(),
                    format_args!("export failed halfway: {error}\n"),
                );
                sender.abort(io::Error::other("the export failed"));
                return;
            }
        };
        if sender.send_data(Bytes::from(text)).await.is_err() {
            return; // the client went away: the snapshot ends with the task
        }
    }
    let _ = sender.send_data(Bytes::from(job.exporter.finish())).await;
    let _ = tx.rollback().await;
}

// ---------------------------------------------------------------------------------------------
// Export of a report
// ---------------------------------------------------------------------------------------------

/// `POST /api/v1/report/{app}.{name}/export?format=csv|json` with `{"params": {..}}`: runs the
/// report as `POST /report/{name}` does (its roles, its row limit and timeout) and answers a
/// file. Not streamed: the result is already bounded. CSV has the column names as its header;
/// JSON is `{"columns": [..], "rows": [[..]], "truncated": bool}`. A result cut at the row limit
/// has the header `X-Forgebase-Truncated: true`.
pub async fn report_export(
    State(state): State<AppState>,
    authenticated: Authenticated,
    path: Result<Path<String>, PathRejection>,
    query: RawQuery,
    ApiJson(body): ApiJson,
) -> Reply {
    let (Path(name), Query(query)) = (path?, query?);
    let caller = authenticated.caller();
    let report = find_report(&state, &name, &caller)?;
    let format = format_param(&params(query, &["format"])?)?;
    let values = report_params(report, body)?;
    let mut client = ReportConnection::get(state.pool()).await?;
    let result = forgebase_db::run_report(&mut client, report, &caller, &values).await?;
    let text = match format {
        Format::Csv => {
            let mut text = String::new();
            let header: Vec<String> = report.columns.iter().map(|c| c.name.clone()).collect();
            push_csv_record(&mut text, &header);
            for row in &result.rows {
                let mut cells = Vec::with_capacity(row.len());
                for (value, column) in row.iter().zip(&report.columns) {
                    cells.push(csv_cell(&column.name, value)?);
                }
                push_csv_record(&mut text, &cells);
            }
            text
        }
        Format::Json => {
            let mut rows = Vec::with_capacity(result.rows.len());
            for row in &result.rows {
                let mut cells = Vec::with_capacity(row.len());
                for (value, column) in row.iter().zip(&report.columns) {
                    cells.push(from_value(&column.name, value)?);
                }
                rows.push(Json::Array(cells));
            }
            let mut text = serde_json::to_string(&json!({
                "columns": columns_json(report),
                "rows": rows,
                "truncated": result.truncated,
            }))
            .map_err(|e| ApiError::internal(&e))?;
            text.push('\n');
            text
        }
    };
    let mut extra = Vec::new();
    if result.truncated {
        extra.push(("x-forgebase-truncated", "true".to_owned()));
    }
    file_response(&report.full_name(), format, Body::from(text), &extra)
}

// ---------------------------------------------------------------------------------------------
// Import
// ---------------------------------------------------------------------------------------------

/// The media type of a `Content-Type` header, lower-cased, without parameters.
fn media_type(headers: &HeaderMap) -> Option<String> {
    let mut values = headers.get_all(CONTENT_TYPE).iter();
    let (Some(value), None) = (values.next(), values.next()) else {
        return None;
    };
    let text = value.to_str().ok()?;
    Some(
        text.split(';')
            .next()
            .unwrap_or("")
            .trim()
            .to_ascii_lowercase(),
    )
}

/// `POST /api/v1/import/{doctype}?format=csv|json&mode=insert|upsert&dry_run=1`; the body is the
/// file. Needs the import and create rights (and write for an upsert). All rows run in one
/// transaction, each in a savepoint; see `forgebase_db::import`. 200 with the summary, or
/// `422 import_failed` with the same summary in `details` when a row failed (nothing is kept
/// either way for a dry run).
pub async fn import(
    State(state): State<AppState>,
    authenticated: Authenticated,
    path: Result<Path<String>, PathRejection>,
    query: RawQuery,
    request: Request,
) -> Reply {
    let (Path(name), Query(query)) = (path?, query?);
    let doctype = doctype(&state, &name)?;
    let caller = authenticated.caller();
    require(doctype, &caller, &[Right::Import], Right::Import)?;
    require(doctype, &caller, &[Right::Create], Right::Create)?;
    let query = params(query, &["format", "mode", "dry_run"])?;
    let format = match query.get("format") {
        Some(text) => Format::parse(text)
            .ok_or_else(|| ApiError::invalid_query("`format` must be csv or json"))?,
        None => {
            return Err(ApiError::invalid_query(
                "`format` (csv or json) is required",
            ));
        }
    };
    let mode = match query.get("mode").map(String::as_str) {
        None | Some("insert") => ImportMode::Insert,
        Some("upsert") => ImportMode::Upsert,
        Some(_) => return Err(ApiError::invalid_query("`mode` must be insert or upsert")),
    };
    if mode == ImportMode::Upsert {
        require(doctype, &caller, &[Right::Write], Right::Write)?;
    }
    let dry_run = match query.get("dry_run").map(String::as_str) {
        None | Some("0") => false,
        Some("1") => true,
        Some(_) => return Err(ApiError::invalid_query("`dry_run` must be 1")),
    };
    let expected = match format {
        Format::Csv => "text/csv",
        Format::Json => "application/json",
    };
    if media_type(request.headers()).as_deref() != Some(expected) {
        return Err(ApiError::new(
            StatusCode::UNSUPPORTED_MEDIA_TYPE,
            "unsupported_media_type",
            format!(
                "the body of a {} import must be {expected}",
                format.extension()
            ),
        ));
    }
    let body = Bytes::from_request(request, &state).await.map_err(|e| {
        if e.status() == StatusCode::PAYLOAD_TOO_LARGE {
            ApiError::new(
                StatusCode::PAYLOAD_TOO_LARGE,
                "payload_too_large",
                "the request body is too large",
            )
        } else {
            ApiError::bad_request("the body cannot be read")
        }
    })?;
    let max_rows = state.config().import_max_rows;
    let parsed = parse_import(state.registry(), doctype, format, mode, &body, max_rows).map_err(
        |error| match error {
            FileError::Invalid(problems) => ApiError::invalid_import(problems),
            FileError::TooLarge { limit } => ApiError::import_too_large(limit),
        },
    )?;
    drop(body);
    let mut client = state.client().await?;
    let summary = run_import(
        &mut **client,
        state.registry(),
        &doctype.name,
        &caller,
        parsed,
        mode,
        dry_run,
    )
    .await?;
    if summary.error_count > 0 && !dry_run {
        return Err(ApiError::import_failed(summary.to_json()));
    }
    Ok(data(StatusCode::OK, summary.to_json()))
}
