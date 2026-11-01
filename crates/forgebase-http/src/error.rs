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
//! Every error body, and every import summary, is bounded where it is built: the message, the details and
//! the keys share a budget of [`MAX_ERROR_BODY_BYTES`] (64 KiB of JSON-escaped bytes), each string is cut to
//! [`MAX_MESSAGE_CHARS`] characters and a list has at most 100 entries and an "and N more" entry. So a body is about
//! 64 KiB plus the envelope at most, whatever the request echoed into it.
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

/// The longest message, detail or key of an error body, in characters, `…` included.
pub const MAX_MESSAGE_CHARS: usize = 512;

/// The total of serialized (JSON-escaped) bytes that the message, the details and the keys and
/// strings of an error body or an import summary may use: 64 KiB. The numbers, the punctuation
/// and the "and N more" entries are paid from the budget too; only the envelope and a small
/// overshoot after the last entry admitted (a cut string's `…`, one "and N more" per container
/// left unfinished) come on top, so the body is about 64 KiB plus the envelope, whatever the
/// request.
pub const MAX_ERROR_BODY_BYTES: usize = 64 * 1024;

/// What is left of the budget of one body.
pub(crate) struct Budget(usize);

impl Budget {
    pub(crate) fn new() -> Self {
        Self(MAX_ERROR_BODY_BYTES)
    }
}

/// The bytes a character takes in a JSON string, as `serde_json` writes it.
fn escaped(c: char) -> usize {
    match c {
        '"' | '\\' | '\n' | '\r' | '\t' | '\u{8}' | '\u{c}' => 2,
        c if c < '\u{20}' => 6,
        c => c.len_utf8(),
    }
}

/// `text` for a body: whole if it has at most [`MAX_MESSAGE_CHARS`] characters and fits the
/// budget, else its first characters (at most 511, fewer if the budget is short) and `…`. The
/// budget is charged the escaped bytes and the quotes. Reads at most 513 characters.
fn cut(text: &str, budget: &mut Budget) -> String {
    let room = budget.0.saturating_sub(2);
    let (mut used, mut count, mut whole) = (0, 0, true);
    for c in text.chars() {
        if count >= MAX_MESSAGE_CHARS || used + escaped(c) > room {
            whole = false;
            break;
        }
        used += escaped(c);
        count += 1;
    }
    if whole {
        budget.0 = room.saturating_sub(used);
        return text.to_owned();
    }
    let room = room.saturating_sub(3);
    let (mut out, mut used) = (String::new(), 0);
    for c in text.chars().take(MAX_MESSAGE_CHARS - 1) {
        if used + escaped(c) > room {
            break;
        }
        used += escaped(c);
        out.push(c);
    }
    out.push('…');
    budget.0 = room.saturating_sub(used);
    out
}

/// A string cut to [`MAX_MESSAGE_CHARS`] (for the text of a report row and the like).
pub(crate) fn cut_text(text: &str) -> String {
    cut(text, &mut Budget::new())
}

/// "and N more", for an array that stops early.
fn more(n: usize, budget: &mut Budget) -> Value {
    let text = format!("and {n} more");
    budget.0 = budget.0.saturating_sub(text.len() + 3);
    Value::String(text)
}

/// Whether an array may take another entry: fewer than [`MAX_REPORTED_ERRORS`] so far and some
/// budget left.
fn room(taken: usize, budget: &Budget) -> bool {
    taken < MAX_REPORTED_ERRORS && budget.0 >= 8
}

/// A list of problems bounded for a response: at most [`MAX_REPORTED_ERRORS`] entries and an
/// "and N more" entry, within the budget, each cut to [`MAX_MESSAGE_CHARS`].
fn capped_in(problems: Vec<String>, budget: &mut Budget) -> Vec<String> {
    let total = problems.len();
    spend(budget, 2);
    let mut out = Vec::new();
    for problem in problems {
        if !room(out.len(), budget) {
            break;
        }
        out.push(cut(&problem, budget));
        spend(budget, 1);
    }
    if out.len() < total {
        let Value::String(text) = more(total - out.len(), budget) else {
            unreachable!("`more` is a string")
        };
        out.push(text);
    }
    out
}

/// [`capped_in`] with a budget of its own, for the lists that a source builds.
pub(crate) fn capped(problems: Vec<String>) -> Vec<String> {
    capped_in(problems, &mut Budget::new())
}

/// `value` within the budget. The budget is exact up to the last entry admitted: it is charged the strings and keys as escaped
/// with their quotes, the numbers, booleans and null as written, 2 bytes per container (the
/// brackets), 1 per array entry and 2 per object entry (the comma, and the colon). Strings and keys
/// are cut, arrays take entries in order while the count and the budget last and end with "and N
/// more". An object is walked with its keys sorted, in three groups: numbers, booleans and null
/// first (the counts of a summary), then strings (each at most [`MAX_MESSAGE_CHARS`] characters, so
/// `mode`, `code` and `message` survive), then arrays and objects; an entry that finds no room is
/// left out. The entries of an object are sorted (O(n log n) in the object); the rest is linear in
/// what it keeps, and what is dropped is only counted.
fn bound_in(value: Value, budget: &mut Budget) -> Value {
    match value {
        Value::String(text) => Value::String(cut(&text, budget)),
        Value::Array(items) => {
            spend(budget, 2);
            let total = items.len();
            let mut out = Vec::new();
            for item in items {
                if !room(out.len(), budget) {
                    break;
                }
                out.push(bound_in(item, budget));
                spend(budget, 1);
            }
            if out.len() < total {
                out.push(more(total - out.len(), budget));
            }
            Value::Array(out)
        }
        Value::Object(map) => {
            spend(budget, 2);
            let group = |v: &Value| match v {
                Value::String(_) => 1,
                Value::Array(_) | Value::Object(_) => 2,
                _ => 0,
            };
            let mut entries: Vec<_> = map.into_iter().collect();
            entries.sort_by_key(|(_, v)| group(v));
            let mut out = serde_json::Map::new();
            for (key, value) in entries {
                if !room(out.len(), budget) {
                    break;
                }
                let key = cut(&key, budget);
                spend(budget, 2);
                out.insert(key, bound_in(value, budget));
            }
            Value::Object(out)
        }
        other => {
            spend(budget, other.to_string().len());
            other
        }
    }
}

/// Takes `bytes` from the budget (to zero at most).
fn spend(budget: &mut Budget, bytes: usize) {
    budget.0 = budget.0.saturating_sub(bytes);
}

/// [`bound_in`] with a budget of its own: the bound of an import summary that is answered as a
/// success (a dry run lists the errors of its rows too).
pub(crate) fn bounded(value: Value) -> Value {
    bound_in(value, &mut Budget::new())
}

/// The one place where an error becomes a response body. The message, the details and the keys
/// share one budget of [`MAX_ERROR_BODY_BYTES`] escaped bytes, each string is cut to
/// [`MAX_MESSAGE_CHARS`] characters and a list keeps at most [`MAX_REPORTED_ERRORS`] entries
/// (then "and N more"), so the body of an error is about 64 KiB at most plus the envelope,
/// whatever the request echoed into it.
impl IntoResponse for ApiError {
    fn into_response(self) -> Response {
        let mut budget = Budget::new();
        let message = cut(&self.message, &mut budget);
        let mut error = json!({"code": self.code, "message": message});
        if let Some(details) = self.details_json {
            error["details"] = bound_in(details, &mut budget);
        } else if !self.details.is_empty() {
            error["details"] = json!(capped_in(self.details, &mut budget));
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

    fn size(value: &Value) -> usize {
        serde_json::to_string(value).unwrap().len()
    }

    #[test]
    fn a_long_message_is_511_characters_and_an_ellipsis() {
        let mut budget = Budget::new();
        assert_eq!(cut("short", &mut budget), "short");
        let exactly = "é".repeat(MAX_MESSAGE_CHARS);
        assert_eq!(cut(&exactly, &mut Budget::new()), exactly);
        let long = "é".repeat(5_000);
        let cut = cut(&long, &mut Budget::new());
        assert_eq!(cut.chars().count(), MAX_MESSAGE_CHARS);
        assert!(cut.ends_with('…'));
        assert_eq!(
            cut.chars().filter(|c| *c == 'é').count(),
            MAX_MESSAGE_CHARS - 1
        );
    }

    #[test]
    fn control_bytes_and_four_byte_characters_are_counted_as_they_are_serialized() {
        let controls = "\u{1}".repeat(10_000);
        let out = cut(&controls, &mut Budget::new());
        assert_eq!(out.chars().count(), MAX_MESSAGE_CHARS);
        assert!(size(&Value::String(out)) <= 6 * MAX_MESSAGE_CHARS + 8);
        let wide = "😀".repeat(10_000);
        let out = cut(&wide, &mut Budget::new());
        assert_eq!(out.chars().count(), MAX_MESSAGE_CHARS);
        // A short budget cuts earlier: 100 bytes is 1 quote pair, the ellipsis and 23 characters.
        let mut small = Budget(100);
        let out = cut(&controls, &mut small);
        assert!(size(&Value::String(out)) <= 100, "the budget holds");
    }

    #[test]
    fn a_long_list_keeps_a_hundred_entries_and_counts_the_rest() {
        let many: Vec<String> = (0..1_000_000).map(|i| format!("{i}")).collect();
        let capped = capped(many);
        assert_eq!(capped.len(), MAX_REPORTED_ERRORS + 1);
        assert_eq!(capped.last().unwrap(), "and 999900 more");
        let few = super::capped(vec!["a".into(), "b".into()]);
        assert_eq!(few, ["a", "b"]);
        // Long entries run out of the budget before the count.
        let long: Vec<String> = vec!["x".repeat(2_000); 1_000];
        let capped = super::capped(long);
        assert!(capped.len() <= MAX_REPORTED_ERRORS + 1);
        assert!(capped.last().unwrap().starts_with("and "));
        assert!(
            size(&json!(capped)) <= MAX_ERROR_BODY_BYTES + 100,
            "{}",
            size(&json!(capped))
        );
    }

    #[test]
    fn nested_arrays_share_one_budget() {
        let long = "𝄞".repeat(10_000);
        let value = json!({
            "count": 5,
            "name": long,
            "errors": (0..300).map(|i| json!({"row": i, "details": vec![long.clone(); 300]})).collect::<Vec<_>>(),
        });
        let bounded = bounded(value);
        assert_eq!(bounded["count"], 5);
        let errors = bounded["errors"].as_array().unwrap();
        assert!(errors.len() <= MAX_REPORTED_ERRORS + 1);
        assert!(
            size(&bounded) < MAX_ERROR_BODY_BYTES + 2_048,
            "{} bytes",
            size(&bounded)
        );
        // The first row is kept first.
        assert_eq!(errors[0]["row"], 0);
    }

    #[test]
    fn the_counts_of_a_summary_survive_an_exhausted_budget() {
        let value = json!({
            "errors": (0..100).map(|_| json!({
                "row": 3, "code": "validation_failed", "message": "bad", "name": "n",
                "details": vec!["\u{1}".repeat(600); 100],
            })).collect::<Vec<_>>(),
            "mode": "upsert", "inserted": 7, "updated": 2, "rows": 100, "error_count": 100,
            "dry_run": true,
        });
        let bounded = bounded(value);
        assert_eq!(bounded["mode"], "upsert");
        assert_eq!(bounded["dry_run"], true);
        for (key, count) in [
            ("inserted", 7),
            ("updated", 2),
            ("rows", 100),
            ("error_count", 100),
        ] {
            assert_eq!(bounded[key], count, "{key}");
        }
        // A row error keeps its code and message ahead of its details.
        let first = &bounded["errors"][0];
        assert_eq!(first["code"], "validation_failed");
        assert_eq!(first["message"], "bad");
        assert_eq!(first["row"], 3);
        assert!(
            size(&bounded) <= MAX_ERROR_BODY_BYTES + 2_048,
            "{}",
            size(&bounded)
        );
    }

    #[test]
    fn keys_are_cut_and_counted() {
        let mut map = serde_json::Map::new();
        map.insert("k".repeat(100_000), json!(1));
        let bounded = bounded(Value::Object(map));
        let key = bounded.as_object().unwrap().keys().next().unwrap();
        assert_eq!(key.chars().count(), MAX_MESSAGE_CHARS);
    }

    #[tokio::test]
    async fn a_response_body_is_small_whatever_the_error_holds() {
        let mut error = ApiError::validation(vec!["z".repeat(1_000_000); 1000]);
        error.message = "m".repeat(1_000_000);
        let (_, body) = error.into_response().into_parts();
        let bytes = axum::body::to_bytes(body, usize::MAX).await.unwrap();
        assert!(
            bytes.len() < MAX_ERROR_BODY_BYTES + 2_000,
            "{} bytes",
            bytes.len()
        );
        let mut error = ApiError::import_failed(json!({
            "error_count": 3,
            "errors": (0..1000).map(|_| json!({"row": 1, "details": vec!["\u{1}".repeat(900); 500]})).collect::<Vec<_>>(),
        }));
        error.message = "m".into();
        let (_, body) = error.into_response().into_parts();
        let bytes = axum::body::to_bytes(body, usize::MAX).await.unwrap();
        assert!(
            bytes.len() < MAX_ERROR_BODY_BYTES + 2_048,
            "{} bytes",
            bytes.len()
        );
    }
}
