//! The HTTP API (M3.3): routes, authentication and the error envelope, on axum.
//!
//! Depends on `forgebase-core` and `forgebase-db`; axum types stay inside this crate (ADR 0005).
//! See `docs/blueprint/architecture.md` (HTTP layer) and `docs/blueprint/security-model.md`.
//!
//! # Request pipeline
//!
//! 1. **Host validation** ([`AppConfig::allowed_hosts`]) runs first, on every route and every
//!    error, including login, unknown paths and wrong methods. A missing, repeated or unknown
//!    `Host` is `400 invalid_host`.
//! 2. **Authentication** (resource routes and logout): `Authorization: token <key>:<secret>` is
//!    checked with `validate_api_token`; **if the header is present the cookie is ignored**.
//!    Without it the `fb_session` cookie is checked with `validate_session`. Anything else is the
//!    same `401 unauthenticated`.
//! 3. **CSRF**: a cookie-authenticated request that is not `GET`/`HEAD` must carry the session's
//!    `X-Forgebase-CSRF` header (`403 csrf_failed`). Token-authenticated requests are exempt.
//! 4. The user becomes a `Caller::User`; the HTTP layer never builds `Caller::System`.
//!
//! No CORS headers are ever sent. Every error, including the rejections of axum's extractors,
//! uses the envelope `{"error": {"code", "message", "details"?}}`; internal errors are logged to
//! standard error and answered with a generic message.
//!
//! # Routes (prefix `/api/v1`)
//!
//! | Route | Success |
//! |---|---|
//! | `POST /login` | 200 `{"data": {"username", "roles", "csrf_token"}}` and `Set-Cookie` |
//! | `POST /logout` (cookie session only) | 204, cookie cleared |
//! | `GET /resource/{doctype}` (`filters`, `sort`, `limit`, `offset`) | 200 `{"data": [..]}` |
//! | `POST /resource/{doctype}` | 201 `{"data": doc}` |
//! | `GET /resource/{doctype}/{name}` | 200 `{"data": doc}` |
//! | `PATCH /resource/{doctype}/{name}` body `{"version", "data"}` | 200 `{"data": doc}` |
//! | `DELETE /resource/{doctype}/{name}?version=N` | 204 |
//! | `POST /resource/{doctype}/{name}/submit` and `/cancel` body `{"version"}` | 200 `{"data": doc}` |
//! | `POST /resource/{doctype}/{name}/amend` body `{"version"}` | 201 `{"data": new doc}` |
//!
//! Anything else is 404 (`/api/v1/method` is not implemented), a wrong method is 405.

mod auth;
mod error;
pub mod json;
mod routes;

use std::future::Future;
use std::ops::DerefMut;
use std::sync::Arc;

use axum::Router;
use axum::extract::{DefaultBodyLimit, Request, State};
use axum::http::header::{CACHE_CONTROL, HOST, X_CONTENT_TYPE_OPTIONS};
use axum::http::{HeaderValue, StatusCode};
use axum::middleware::{self, Next};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use forgebase_core::meta::Registry;
use forgebase_db::Pool;
use tokio::sync::Semaphore;

use crate::error::ApiError;

/// Settings of the HTTP API.
#[derive(Debug, Clone)]
pub struct AppConfig {
    /// The host names the server answers to (case-insensitive, port ignored), for example
    /// `localhost` or `erp.example.com`. An IPv6 address is written with brackets (`[::1]`).
    pub allowed_hosts: Vec<String>,
    /// Add `Secure` to the session cookie. `false` is for local development over plain HTTP only.
    pub secure_cookies: bool,
    /// The largest request body in bytes; a larger one is `413 payload_too_large`.
    pub body_limit_bytes: usize,
    /// How many logins may hash a password at the same time; more are `503 too_many_logins`
    /// with `Retry-After`. Argon2 uses about 19 MiB each, so this bounds memory and keeps
    /// database connections free for other requests.
    pub max_concurrent_logins: usize,
}

impl Default for AppConfig {
    fn default() -> Self {
        Self {
            allowed_hosts: vec!["localhost".to_owned(), "127.0.0.1".to_owned()],
            secure_cookies: true,
            body_limit_bytes: 1024 * 1024,
            max_concurrent_logins: 8,
        }
    }
}

/// Shared state of the router: the connection pool, the DocType registry and the settings.
#[derive(Clone)]
pub struct AppState {
    inner: Arc<Inner>,
}

struct Inner {
    pool: Pool,
    registry: Arc<Registry>,
    config: AppConfig,
    /// Allowed hosts, lower-cased.
    hosts: Vec<String>,
    logins: Semaphore,
}

impl AppState {
    /// Creates the state. The registry must be the one the database schema was synced with.
    pub fn new(pool: Pool, registry: Arc<Registry>, config: AppConfig) -> Self {
        let hosts = config
            .allowed_hosts
            .iter()
            .map(|h| h.trim().to_ascii_lowercase())
            .collect();
        let logins = Semaphore::new(config.max_concurrent_logins.max(1));
        Self {
            inner: Arc::new(Inner {
                pool,
                registry,
                config,
                hosts,
                logins,
            }),
        }
    }

    /// A pooled database connection (`&mut **client` is the `Client`). A failure is a generic
    /// 500; the cause goes to standard error.
    pub(crate) async fn client(
        &self,
    ) -> Result<impl DerefMut<Target = impl DerefMut<Target = forgebase_db::Client>>, ApiError>
    {
        self.inner
            .pool
            .get()
            .await
            .map_err(|e| ApiError::internal(&e))
    }

    pub(crate) fn registry(&self) -> &Registry {
        &self.inner.registry
    }

    pub(crate) fn config(&self) -> &AppConfig {
        &self.inner.config
    }

    pub(crate) fn logins(&self) -> &Semaphore {
        &self.inner.logins
    }
}

/// Builds the router with every route and the Host check.
pub fn router(state: AppState) -> Router {
    let limit = state.config().body_limit_bytes;
    Router::new()
        .route("/api/v1/login", post(routes::login))
        .route("/api/v1/logout", post(routes::logout))
        .route(
            "/api/v1/resource/{doctype}",
            get(routes::list).post(routes::create),
        )
        .route(
            "/api/v1/resource/{doctype}/{name}",
            get(routes::read)
                .patch(routes::update)
                .delete(routes::delete),
        )
        .route(
            "/api/v1/resource/{doctype}/{name}/submit",
            post(routes::submit),
        )
        .route(
            "/api/v1/resource/{doctype}/{name}/cancel",
            post(routes::cancel),
        )
        .route(
            "/api/v1/resource/{doctype}/{name}/amend",
            post(routes::amend),
        )
        .fallback(|| async { ApiError::new(StatusCode::NOT_FOUND, "not_found", "no such route") })
        .method_not_allowed_fallback(|| async {
            ApiError::new(
                StatusCode::METHOD_NOT_ALLOWED,
                "method_not_allowed",
                "method not allowed",
            )
        })
        .layer(DefaultBodyLimit::max(limit))
        // Added last, so it is the outermost layer: it runs before everything else.
        .layer(middleware::from_fn_with_state(state.clone(), check_host))
        .with_state(state)
}

/// Serves the API on `listener` until `shutdown` completes (in-flight requests finish).
pub async fn serve(
    listener: tokio::net::TcpListener,
    state: AppState,
    shutdown: impl Future<Output = ()> + Send + 'static,
) -> std::io::Result<()> {
    axum::serve(listener, router(state))
        .with_graceful_shutdown(shutdown)
        .await
}

/// Rejects a request whose `Host` is missing, repeated or not allowed, then adds the headers
/// every response gets.
async fn check_host(State(state): State<AppState>, request: Request, next: Next) -> Response {
    let mut hosts = request.headers().get_all(HOST).iter();
    let allowed = match (hosts.next(), hosts.next()) {
        (Some(value), None) => value
            .to_str()
            .is_ok_and(|v| state.inner.hosts.iter().any(|h| *h == host_part(v))),
        _ => false,
    };
    let mut response = if allowed {
        next.run(request).await
    } else {
        ApiError::new(
            StatusCode::BAD_REQUEST,
            "invalid_host",
            "invalid Host header",
        )
        .into_response()
    };
    let headers = response.headers_mut();
    headers.insert(CACHE_CONTROL, HeaderValue::from_static("no-store"));
    headers.insert(X_CONTENT_TYPE_OPTIONS, HeaderValue::from_static("nosniff"));
    response
}

/// The host of a `Host` header value, lower-cased, without the port.
fn host_part(value: &str) -> String {
    let value = value.trim().to_ascii_lowercase();
    if let Some(rest) = value.strip_prefix('[') {
        // `[::1]` or `[::1]:8000`
        return match rest.split_once(']') {
            Some((address, "")) => format!("[{address}]"),
            Some((address, port)) if is_port(port.strip_prefix(':')) => format!("[{address}]"),
            _ => value,
        };
    }
    match value.rsplit_once(':') {
        Some((host, port)) if is_port(Some(port)) => host.to_owned(),
        _ => value,
    }
}

fn is_port(text: Option<&str>) -> bool {
    text.is_some_and(|t| !t.is_empty() && t.bytes().all(|b| b.is_ascii_digit()))
}

#[cfg(test)]
mod tests {
    use super::host_part;

    #[test]
    fn host_part_drops_port_and_case() {
        assert_eq!(host_part("Example.COM"), "example.com");
        assert_eq!(host_part("example.com:8000"), "example.com");
        assert_eq!(host_part("[::1]"), "[::1]");
        assert_eq!(host_part("[::1]:8000"), "[::1]");
        // Not a port: stays whole and so never matches an allowed host.
        assert_eq!(host_part("localhost:80@evil.com"), "localhost:80@evil.com");
        assert_eq!(host_part("localhost:"), "localhost:");
        assert_eq!(host_part("[::1]x"), "[::1]x");
    }
}
