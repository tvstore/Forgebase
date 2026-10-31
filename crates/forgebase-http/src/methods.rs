//! App method endpoints (M9.1, ADR 0019): `POST /api/v1/method/{app}.{method}`.
//!
//! This crate knows **names and access only** (the [`Registry`] of the site) and nothing of
//! handlers: the `forgebase` crate implements [`MethodDispatch`], runs the handler in one
//! transaction as the caller, and is injected with [`AppState::with_methods`]. A method that
//! does not exist, that is not installed on the site, or that the caller may not call is the
//! same `404 not_found`, so that method names are not revealed.

use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;

use axum::body::Bytes;
use axum::extract::rejection::PathRejection;
use axum::extract::{FromRequest, Path, Query, Request, State};
use axum::http::{HeaderMap, StatusCode, header::CONTENT_TYPE};
use forgebase_core::meta::Registry;
use forgebase_core::permission::{Caller, can_call_method};
use forgebase_db::{DbError, Pool};
use serde_json::{Map, Value as Json};

use crate::AppState;
use crate::auth::Authenticated;
use crate::error::ApiError;
use crate::routes::{RawQuery, Reply, data, params};

/// A boxed future, as the dispatcher returns it (there is no async-trait crate).
pub type BoxFuture<'a, T> = Pin<Box<dyn Future<Output = T> + Send + 'a>>;

/// Why a method call failed. The HTTP layer turns it into the error envelope; **nothing internal
/// reaches the client**: [`MethodFailure::Db`] goes through the same mapping as every other
/// database error (a refusal keeps its status and code, anything else is a generic 500), and
/// [`MethodFailure::Internal`] is logged and answered with a generic 500.
#[derive(Debug)]
pub enum MethodFailure {
    /// A database or document error, mapped like the document API maps it.
    Db(DbError),
    /// An error the handler chose, with a message meant for users.
    Handler {
        /// The HTTP status (400 or 422 today).
        status: u16,
        /// The error code in the envelope.
        code: &'static str,
        /// The message, as the app wrote it.
        message: String,
    },
    /// A failure of the framework or a handler that panicked. Only logged.
    Internal(String),
}

impl From<MethodFailure> for ApiError {
    fn from(failure: MethodFailure) -> Self {
        match failure {
            MethodFailure::Db(error) => error.into(),
            MethodFailure::Handler {
                status,
                code,
                message,
            } => match StatusCode::from_u16(status) {
                Ok(status) if status.is_client_error() => Self::new(status, code, message),
                _ => Self::internal(&format!("method error with status {status}: {message}")),
            },
            MethodFailure::Internal(message) => Self::internal(&message),
        }
    }
}

/// Runs a method. Implemented by the `forgebase` crate: one transaction, the documents handle as
/// the caller, the handler, then commit on `Ok` and rollback on every error (a panic included).
///
/// `name` is the full name (`<app>.<method>`) of a method that the site's `registry` lists and
/// that `caller` may call; `body` is a JSON object.
pub trait MethodDispatch: Send + Sync {
    /// Calls the method and returns the handler's JSON.
    fn call<'a>(
        &'a self,
        pool: &'a Pool,
        registry: &'a Arc<Registry>,
        name: &'a str,
        caller: Caller,
        body: Json,
    ) -> BoxFuture<'a, Result<Json, MethodFailure>>;
}

/// The JSON object body of a method call. An empty body is `{}` (and needs no content type); a
/// body that is not empty must be `application/json` (415) and a JSON object (400). Over the
/// body limit is 413.
pub(crate) struct MethodBody(pub(crate) Json);

impl FromRequest<AppState> for MethodBody {
    type Rejection = ApiError;

    async fn from_request(request: Request, state: &AppState) -> Result<Self, Self::Rejection> {
        let json_type = is_json(request.headers());
        let bytes = Bytes::from_request(request, state).await.map_err(|r| {
            if r.status() == StatusCode::PAYLOAD_TOO_LARGE {
                ApiError::new(
                    StatusCode::PAYLOAD_TOO_LARGE,
                    "payload_too_large",
                    "the request body is too large",
                )
            } else {
                ApiError::bad_request("the body could not be read")
            }
        })?;
        if bytes.is_empty() {
            return Ok(Self(Json::Object(Map::new())));
        }
        if !json_type {
            return Err(ApiError::new(
                StatusCode::UNSUPPORTED_MEDIA_TYPE,
                "unsupported_media_type",
                "the body must be JSON (Content-Type: application/json)",
            ));
        }
        match serde_json::from_slice::<Json>(&bytes) {
            Ok(json @ Json::Object(_)) => Ok(Self(json)),
            Ok(_) => Err(ApiError::bad_request("the body must be a JSON object")),
            Err(_) => Err(ApiError::bad_request("the body is not valid JSON")),
        }
    }
}

/// `Content-Type: application/json` (or `application/*+json`), parameters ignored.
fn is_json(headers: &HeaderMap) -> bool {
    let Some(value) = headers.get(CONTENT_TYPE).and_then(|v| v.to_str().ok()) else {
        return false;
    };
    let essence = value
        .split(';')
        .next()
        .unwrap_or_default()
        .trim()
        .to_ascii_lowercase();
    essence == "application/json"
        || (essence.starts_with("application/") && essence.ends_with("+json"))
}

/// `POST /api/v1/method/{app}.{method}` with a JSON object body (optional): calls the method and
/// returns `200 {"data": <what the handler returned>}`.
pub async fn call(
    State(state): State<AppState>,
    authenticated: Authenticated,
    path: Result<Path<String>, PathRejection>,
    query: RawQuery,
    MethodBody(body): MethodBody,
) -> Reply {
    let (Path(name), Query(query)) = (path?, query?);
    let caller = authenticated.caller();
    // Whether the method exists on this site and whether the caller may call it are one answer.
    // This asks the one evaluator of `forgebase-core`; the handler's documents are checked again,
    // per call, by `forgebase-db`.
    let not_found = || ApiError::not_found("method not found");
    let method = state
        .registry()
        .method(&name)
        .filter(|method| can_call_method(&method.access, &caller))
        .ok_or_else(not_found)?;
    params(query, &[])?;
    let dispatch = state.methods().ok_or_else(not_found)?;
    let result = dispatch
        .call(
            state.pool(),
            state.registry_arc(),
            &method.full_name(),
            caller,
            body,
        )
        .await?;
    Ok(data(StatusCode::OK, result))
}
