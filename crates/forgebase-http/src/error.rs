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
//! | `bad_request` | 400 | malformed body or parameters |
//! | `invalid_query` | 400 | a filter, sort, limit or offset cannot be used |
//! | `invalid_host` | 400 | |
//! | `payload_too_large` | 413 | |
//! | `unsupported_media_type` | 415 | the body is not `application/json` |
//! | `too_many_logins` | 503 | login concurrency bound; `Retry-After` is set |
//! | `internal` | 500 | generic message; the real error goes to standard error |
//!
//! Messages never contain passwords, tokens, secrets or the database URL. Database errors that
//! are not the caller's fault are logged and replaced by a generic message.

use std::fmt::Display;

use axum::Json;
use axum::extract::rejection::{JsonRejection, PathRejection, QueryRejection};
use axum::http::header::RETRY_AFTER;
use axum::http::{HeaderValue, StatusCode};
use axum::response::{IntoResponse, Response};
use forgebase_db::DbError;
use forgebase_db::auth::AuthError;
use serde_json::json;

use crate::json::JsonError;

/// An error response.
#[derive(Debug)]
pub struct ApiError {
    status: StatusCode,
    code: &'static str,
    message: String,
    details: Vec<String>,
    retry_after: Option<u32>,
}

impl ApiError {
    pub fn new(status: StatusCode, code: &'static str, message: impl Into<String>) -> Self {
        Self {
            status,
            code,
            message: message.into(),
            details: Vec::new(),
            retry_after: None,
        }
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

impl IntoResponse for ApiError {
    fn into_response(self) -> Response {
        let mut error = json!({"code": self.code, "message": self.message});
        if !self.details.is_empty() {
            error["details"] = json!(self.details);
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
