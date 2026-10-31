//! The error envelope: every error response is
//! `{"error": {"code": "<snake_case>", "message": "<text>", "details": ["..."]}}`.
//!
//! | Code | Status | Meaning |
//! |---|---|---|
//! | `unauthenticated` | 401 | no valid session or token, or a failed login (one message each) |
//! | `permission_denied` | 403 | the caller lacks the right (for a transition: the role or the right; for an update: the `edit_roles` of the state; for a view: creating or changing a shared one without being an Administrator) |
//! | `csrf_failed` | 403 | missing or wrong `X-Forgebase-CSRF` |
//! | `not_found` | 404 | unknown route, DocType or document (also a document the caller cannot read) |
//! | `method_not_allowed` | 405 | |
//! | `conflict` | 409 | the document changed (`version`), or changed while being read; a saved view with that label exists |
//! | `invalid_docstatus`, `already_amended`, `field_locked` | 409 | the document's state forbids it |
//! | `workflow_required`, `no_workflow`, `unknown_transition` | 409 | submit or cancel on a DocType with a workflow (use a transition); a transition, its list or history on a DocType without one; no such transition from the document's state |
//! | `unique_violation`, `linked_document_exists` | 409 | |
//! | `not_submittable` | 422 | submit, cancel or amend of a DocType that is not submittable |
//! | `validation_failed` | 422 | the data breaks the DocType's rules; `details` lists every problem; also a saved view's label, definition or limit |
//! | `hook_rejected` | 422 | a lifecycle hook refused; the message is the hook's |
//! | `bad_request` | 400 | malformed body or parameters (for a method: a body that is not a JSON object) |
//! | `invalid_query` | 400 | a filter, sort, limit or offset cannot be used |
//! | `invalid_host` | 400 | |
//! | `payload_too_large` | 413 | |
//! | `export_too_large`, `import_too_large` | 422 | an export would have more rows than the limit (`details`: `count`, `limit`); an import file has more rows than the limit (`details`: `limit`) (M9.3) |
//! | `invalid_import` | 422 | the import file cannot be used at all: not UTF-8, bad syntax, an unknown, duplicate or table column; `details` lists the problems (M9.3) |
//! | `import_failed` | 422 | at least one row of an import has an error, so nothing was imported; `details` is the summary with the per-row errors (M9.3) |
//! | `unsupported_media_type` | 415 | the body is not `application/json` (for an import: not the type of its format) |
//! | `too_many_logins` | 503 | login concurrency bound; `Retry-After` is set |
//! | `method_error` | 400 or 422 | an app method refused the call; the message is the app's, written for users (M9.1) |
//! | `report_timeout` | 504 | a query report ran over its time limit (M8.4) |
//! | `internal` | 500 | generic message; the real error goes to standard error |
//!
//! Every error body is bounded where it is built (`IntoResponse`): the message and each detail are cut to
//! [`MAX_MESSAGE_CHARS`] characters and a list of details has at most 100 entries and an "and N more" entry,
//! whatever the request echoed into it.
//!
//! Messages never contain passwords, tokens, secrets or the database URL. Database errors that
//! are not the caller's fault are logged and replaced by a generic message.

use std::fmt::Display;

use axum::Json;
use axum::extract::rejection::{JsonRejection, PathRejection, QueryRejection};
use axum::http::header::RETRY_AFTER;
use axum::http::{HeaderValue, StatusCode};
use axum::response::{IntoResponse, Response};
use forgebase_db::auth::AuthError;
use forgebase_db::{DbError, MAX_REPORTED_ERRORS};
use serde_json::{Value, json};

use crate::json::JsonError;

/// An error response.
#[derive(Debug)]
pub struct ApiError {
    status: StatusCode,
    code: &'static str,
    message: String,
    details: Vec<String>,
    /// Structured details (an object), instead of the list of strings.
    details_json: Option<serde_json::Value>,
    retry_after: Option<u32>,
}

impl ApiError {
    pub fn new(status: StatusCode, code: &'static str, message: impl Into<String>) -> Self {
        Self {
            status,
            code,
            message: message.into(),
            details: Vec::new(),
            details_json: None,
            retry_after: None,
        }
    }

    /// The code, the message and the list of details, for an error that is reported inside
    /// another answer (a row of an import).
    pub(crate) fn into_parts(self) -> (&'static str, String, Vec<String>) {
        (self.code, self.message, self.details)
    }

    /// 422 `export_too_large`: the filters match more rows than the limit.
    pub(crate) fn export_too_large(count: u64, limit: u64) -> Self {
        let mut error = Self::new(
            StatusCode::UNPROCESSABLE_ENTITY,
            "export_too_large",
            format!(
                "the export would have {count} rows, over the limit of {limit}; narrow the filters"
            ),
        );
        error.details_json = Some(json!({"count": count, "limit": limit}));
        error
    }

    /// 422 `import_too_large`: more rows than the limit; nothing ran.
    pub(crate) fn import_too_large(limit: usize) -> Self {
        let mut error = Self::new(
            StatusCode::UNPROCESSABLE_ENTITY,
            "import_too_large",
            format!("the file has more than {limit} rows; split it"),
        );
        error.details_json = Some(json!({"limit": limit}));
        error
    }

    /// 422 `invalid_import`: the file cannot be imported at all (unknown column, bad syntax).
    pub(crate) fn invalid_import(problems: Vec<String>) -> Self {
        let mut error = Self::new(
            StatusCode::UNPROCESSABLE_ENTITY,
            "invalid_import",
            "the file cannot be imported",
        );
        error.details = problems;
        error
    }

    /// 422 `import_failed`: at least one row has an error, so nothing was imported. `summary` is
    /// the summary of the import, as in the success answer.
    pub(crate) fn import_failed(summary: serde_json::Value) -> Self {
        let count = summary["error_count"].as_u64().unwrap_or(0);
        let mut error = Self::new(
            StatusCode::UNPROCESSABLE_ENTITY,
            "import_failed",
            format!("{count} row(s) have errors; nothing was imported"),
        );
        error.details_json = Some(summary);
        error
    }

    /// The one 401: the same for a missing, invalid, expired or revoked credential.
    pub fn unauthenticated() -> Self {
        Self::new(
            StatusCode::UNAUTHORIZED,
            "unauthenticated",
            "authentication required",
        )
    }

    pub fn csrf_failed() -> Self {
        Self::new(
            StatusCode::FORBIDDEN,
            "csrf_failed",
            "missing or invalid CSRF token",
        )
    }

    pub fn bad_request(message: impl Into<String>) -> Self {
        Self::new(StatusCode::BAD_REQUEST, "bad_request", message)
    }

    pub fn invalid_query(message: impl Into<String>) -> Self {
        Self::new(StatusCode::BAD_REQUEST, "invalid_query", message)
    }

    pub fn not_found(message: impl Into<String>) -> Self {
        Self::new(StatusCode::NOT_FOUND, "not_found", message)
    }

    pub fn too_many_logins() -> Self {
        let mut error = Self::new(
            StatusCode::SERVICE_UNAVAILABLE,
            "too_many_logins",
            "too many logins at the moment; retry shortly",
        );
        error.retry_after = Some(1);
        error
    }

    /// A 500 with a generic message. The real error goes to standard error only.
    pub fn internal(error: &dyn Display) -> Self {
        // Standard error is the server's own log; the client learns nothing.
        // Ignore a failed write: a closed standard error must not panic a request.
        let _ = std::io::Write::write_fmt(
            &mut std::io::stderr(),
            format_args!("internal error: {error}\n"),
        );
        Self::new(
            StatusCode::INTERNAL_SERVER_ERROR,
            "internal",
            "internal server error",
        )
    }

    pub(crate) fn validation(details: Vec<String>) -> Self {
        let mut error = Self::new(
            StatusCode::UNPROCESSABLE_ENTITY,
            "validation_failed",
            "validation failed",
        );
        error.details = details;
        error
    }
}

/// The longest message or detail an error response carries, in characters.
pub const MAX_MESSAGE_CHARS: usize = 512;

/// `text` cut to [`MAX_MESSAGE_CHARS`] characters on a character boundary, with `…` if cut.
fn cut(text: &str) -> String {
    match text.char_indices().nth(MAX_MESSAGE_CHARS) {
        None => text.to_owned(),
        Some((end, _)) => format!("{}…", &text[..end]),
    }
}

/// A list of problems bounded for a response: at most [`MAX_REPORTED_ERRORS`] entries and an
/// "and N more" entry, each cut to [`MAX_MESSAGE_CHARS`].
pub(crate) fn capped(mut problems: Vec<String>) -> Vec<String> {
    if problems.len() > MAX_REPORTED_ERRORS {
        let more = problems.len() - MAX_REPORTED_ERRORS;
        problems.truncate(MAX_REPORTED_ERRORS);
        problems.push(format!("and {more} more"));
    }
    problems.iter().map(|p| cut(p)).collect()
}

/// `details` as an object, bounded the same way: strings cut, arrays capped, recursively.
fn bound(value: Value) -> Value {
    match value {
        Value::String(text) if text.len() > MAX_MESSAGE_CHARS => Value::String(cut(&text)),
        Value::Array(mut items) => {
            let more = items.len().saturating_sub(MAX_REPORTED_ERRORS);
            items.truncate(MAX_REPORTED_ERRORS);
            let mut items: Vec<Value> = items.into_iter().map(bound).collect();
            if more > 0 {
                items.push(Value::String(format!("and {more} more")));
            }
            Value::Array(items)
        }
        Value::Object(map) => Value::Object(map.into_iter().map(|(k, v)| (k, bound(v))).collect()),
        other => other,
    }
}

/// The one place where an error becomes a response body, so **every** error response of the API
/// is bounded whatever the request echoed into it: the message and each detail are cut to
/// [`MAX_MESSAGE_CHARS`] characters and a list of details has at most [`MAX_REPORTED_ERRORS`]
/// entries and an "and N more" line (arrays inside an object `details` too).
impl IntoResponse for ApiError {
    fn into_response(self) -> Response {
        let mut error = json!({"code": self.code, "message": cut(&self.message)});
        if let Some(details) = self.details_json {
            error["details"] = bound(details);
        } else if !self.details.is_empty() {
            error["details"] = json!(capped(self.details));
        }
        let mut response = (self.status, Json(json!({ "error": error }))).into_response();
        if let Some(seconds) = self.retry_after {
            response
                .headers_mut()
                .insert(RETRY_AFTER, HeaderValue::from(seconds));
        }
        response
    }
}

impl From<DbError> for ApiError {
    fn from(error: DbError) -> Self {
        use StatusCode as S;
        let text = error.to_string();
        match error {
            DbError::Validation(errors) => {
                Self::validation(errors.iter().map(ToString::to_string).collect())
            }
            DbError::Naming(message) => Self::validation(vec![message]),
            DbError::InvalidView(message) => Self::validation(vec![message]),
            DbError::ViewExists { .. } => Self::new(S::CONFLICT, "conflict", text),
            DbError::ViewDenied(_) => Self::new(S::FORBIDDEN, "permission_denied", text),
            DbError::NotFound { .. } => Self::not_found(text),
            // The same answer for a report that does not exist and one the caller may not run.
            DbError::ReportNotFound { .. } => Self::not_found("report not found"),
            DbError::ReportTimeout { .. } => Self::new(
                S::GATEWAY_TIMEOUT,
                "report_timeout",
                "the report took too long; narrow the parameters",
            ),
            // A report that writes or does not fit its declaration is a mistake of the app. The
            // message names the report and never the SQL; it is logged, not sent.
            DbError::ReportReadOnly { .. } | DbError::Report(_) => Self::internal(&text),
            DbError::UnknownDocType { .. } | DbError::ChildDocType { .. } => {
                Self::not_found("unknown doctype")
            }
            DbError::PermissionDenied { .. }
            | DbError::TransitionDenied { .. }
            | DbError::EditDenied { .. } => Self::new(S::FORBIDDEN, "permission_denied", text),
            DbError::WorkflowRequired { .. } => Self::new(S::CONFLICT, "workflow_required", text),
            DbError::NoWorkflow { .. } => Self::new(S::CONFLICT, "no_workflow", text),
            DbError::UnknownTransition { .. } => Self::new(S::CONFLICT, "unknown_transition", text),
            DbError::InvalidQuery(_) => Self::invalid_query(text),
            DbError::ExportTooLarge { count, limit } => Self::export_too_large(count, limit),
            DbError::NoChanges => Self::bad_request(text),
            DbError::Conflict { .. } | DbError::ChangedWhileReading { .. } => {
                Self::new(S::CONFLICT, "conflict", text)
            }
            DbError::InvalidDocStatus { .. } => Self::new(S::CONFLICT, "invalid_docstatus", text),
            DbError::AlreadyAmended { .. } => Self::new(S::CONFLICT, "already_amended", text),
            DbError::FieldLocked { .. } => Self::new(S::CONFLICT, "field_locked", text),
            DbError::LinkedDocumentExists { .. } => {
                Self::new(S::CONFLICT, "linked_document_exists", text)
            }
            // The server's message names the constraint and the conflicting key: not sent.
            DbError::UniqueViolation { .. } => Self::new(
                S::CONFLICT,
                "unique_violation",
                "a document with the same unique value already exists",
            ),
            DbError::NotSubmittable { .. } => {
                Self::new(S::UNPROCESSABLE_ENTITY, "not_submittable", text)
            }
            DbError::Hook { .. } => Self::new(S::UNPROCESSABLE_ENTITY, "hook_rejected", text),
            // A job that a hook enqueued has an unknown name or a payload that is too large: a
            // mistake of the app, not of the caller. Logged, and answered like any internal error.
            DbError::Job(_) => Self::internal(&text),
            // Postgres, Pool, Decode, LockTimeout and everything else: the caller cannot help.
            other => Self::internal(&other),
        }
    }
}

impl From<AuthError> for ApiError {
    fn from(error: AuthError) -> Self {
        match error {
            AuthError::InvalidCredentials => Self::new(
                StatusCode::UNAUTHORIZED,
                "unauthenticated",
                "invalid username or password",
            ),
            AuthError::Db(error) => error.into(),
            other => Self::internal(&other),
        }
    }
}

impl From<JsonError> for ApiError {
    fn from(error: JsonError) -> Self {
        match error {
            JsonError::Syntax(_) => Self::bad_request("the body is not valid JSON"),
            JsonError::NotAnObject => Self::bad_request("the data must be a JSON object"),
            JsonError::Field { .. } => Self::validation(vec![error.to_string()]),
            JsonError::Query(message) => Self::invalid_query(message),
            JsonError::Serialize(_) | JsonError::Output { .. } => Self::internal(&error),
        }
    }
}

impl From<JsonRejection> for ApiError {
    fn from(rejection: JsonRejection) -> Self {
        match rejection.status() {
            StatusCode::PAYLOAD_TOO_LARGE => Self::new(
                StatusCode::PAYLOAD_TOO_LARGE,
                "payload_too_large",
                "the request body is too large",
            ),
            StatusCode::UNSUPPORTED_MEDIA_TYPE => Self::new(
                StatusCode::UNSUPPORTED_MEDIA_TYPE,
                "unsupported_media_type",
                "the body must be JSON (Content-Type: application/json)",
            ),
            _ => Self::bad_request("the body is not valid JSON"),
        }
    }
}

impl From<PathRejection> for ApiError {
    fn from(_: PathRejection) -> Self {
        Self::bad_request("invalid path")
    }
}

impl From<QueryRejection> for ApiError {
    fn from(_: QueryRejection) -> Self {
        Self::invalid_query("invalid query string")
    }
}

#[cfg(test)]
mod bound_tests {
    use super::*;

    #[test]
    fn a_long_message_is_cut_on_a_character_boundary() {
        assert_eq!(cut("short"), "short");
        let long = "é".repeat(5_000);
        let cut = cut(&long);
        assert_eq!(cut.chars().count(), MAX_MESSAGE_CHARS + 1);
        assert!(cut.ends_with('…'));
    }

    #[test]
    fn a_long_list_keeps_a_hundred_entries_and_counts_the_rest() {
        let many: Vec<String> = (0..1_000_000)
            .map(|i| format!("{i}{}", "x".repeat(2_000)))
            .collect();
        let capped = capped(many);
        assert_eq!(capped.len(), MAX_REPORTED_ERRORS + 1);
        assert_eq!(capped.last().unwrap(), "and 999900 more");
        assert!(
            capped
                .iter()
                .all(|c| c.chars().count() <= MAX_MESSAGE_CHARS + 1)
        );
        let few = capped_vec(&["a", "b"]);
        assert_eq!(few, ["a", "b"]);
    }

    fn capped_vec(items: &[&str]) -> Vec<String> {
        capped(items.iter().map(|s| (*s).to_owned()).collect())
    }

    #[test]
    fn an_object_has_its_arrays_capped_and_its_strings_cut_recursively() {
        let long = "y".repeat(10_000);
        let value = json!({
            "count": 5,
            "name": long,
            "errors": (0..300).map(|i| json!({"row": i, "details": vec![long.clone(); 300]})).collect::<Vec<_>>(),
        });
        let bounded = bound(value);
        assert_eq!(bounded["count"], 5);
        assert!(bounded["name"].as_str().unwrap().chars().count() <= MAX_MESSAGE_CHARS + 1);
        let errors = bounded["errors"].as_array().unwrap();
        assert_eq!(errors.len(), MAX_REPORTED_ERRORS + 1);
        assert_eq!(errors[100], "and 200 more");
        let details = errors[0]["details"].as_array().unwrap();
        assert_eq!(details.len(), MAX_REPORTED_ERRORS + 1);
        assert!(bounded.to_string().len() < 100_000_000);
    }

    #[test]
    fn a_response_body_is_small_whatever_the_error_holds() {
        let mut error = ApiError::validation(vec!["z".repeat(1_000_000); 1000]);
        error.message = "m".repeat(1_000_000);
        let response = error.into_response();
        let (_, body) = response.into_parts();
        let bytes = futures_body(body);
        assert!(bytes < 100_000, "{bytes} bytes");
    }

    fn futures_body(body: axum::body::Body) -> usize {
        let runtime = tokio::runtime::Builder::new_current_thread()
            .build()
            .unwrap();
        runtime
            .block_on(axum::body::to_bytes(body, usize::MAX))
            .unwrap()
            .len()
    }
}
