//! The HTTP API (M3.3): routes, authentication and the error envelope, on axum.
//!
//! **Internal crate, not a public API:** applications use the `forgebase` crate, and nothing here
//! has a compatibility promise (ADR 0012). Depends on `forgebase-core` and `forgebase-db`; axum types stay inside this crate (ADR 0005).
//! See `docs/blueprint/architecture.md` (HTTP layer) and `docs/blueprint/security-model.md`.
//!
//! # Request pipeline
//!
//! 1. **Host routing** ([`Sites`]) runs first, on every route and every error, including login,
//!    unknown paths and wrong methods. A missing, repeated or malformed `Host` is
//!    `400 invalid_host`. The `Host` then selects the site, and the request is handled by that
//!    site's own router, state and database: with a config file a well-formed host that no site
//!    owns is `404 not_found` ("no such site"); in the implicit single-site mode
//!    ([`Sites::implicit`], no config file) a host outside [`AppConfig::allowed_hosts`] stays
//!    `400 invalid_host`, the protection against DNS rebinding. `X-Forwarded-Host` and
//!    `Forwarded` are never read: a reverse proxy must pass the original `Host`.
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
//! | `GET /session` (cookie session only, header `X-Forgebase-CSRF: refresh`, else 400) | 200 `{"data": {"username", "roles", "csrf_token"}}`, a **new** CSRF token (the old one stops working) |
//! | `GET /meta` | 200 `{"data": [{"name", "label", "app", "submittable", "rights"}]}`: the DocTypes the caller can read; `app` is the owning app (M5.1, `null` if the registry records none) |
//! | `GET /meta/{doctype}` | 200 `{"data": {"name", "label", "app", "naming", "submittable", "fields", "rights", "workflow"}}`; `workflow` is `null` or `{"states": [{"name", "docstatus", "style"}], "transitions": [{"action", "from", "to"}]}` (never the roles) |
//! | `GET /resource/{doctype}` (`filters`, `sort`, `limit`, `offset`) | 200 `{"data": [..]}` |
//! | `POST /resource/{doctype}` | 201 `{"data": doc}` |
//! | `GET /resource/{doctype}/{name}` | 200 `{"data": doc}` |
//! | `PATCH /resource/{doctype}/{name}` body `{"version", "data"}` | 200 `{"data": doc}` |
//! | `DELETE /resource/{doctype}/{name}?version=N` | 204 |
//! | `POST /resource/{doctype}/{name}/submit` and `/cancel` body `{"version"}` | 200 `{"data": doc}` |
//! | `POST /resource/{doctype}/{name}/amend` body `{"version"}` | 201 `{"data": new doc}` |
//! | `POST /resource/{doctype}/{name}/transition` body `{"action", "version"}` (M8.1) | 200 `{"data": doc}`; DocTypes with a workflow only |
//! | `GET /resource/{doctype}/{name}/transitions` (M8.1) | 200 `{"data": {"state", "transitions": [{"action", "to"}]}}`: what the caller may take now (role and right); hooks are not run |
//! | `GET /resource/{doctype}/{name}/history` (M8.1) | 200 `{"data": {"history": [{"action", "from", "to", "actor", "at"}]}}`, oldest first, the newest 500 at most |
//!
//! | `GET /views/{doctype}` (M8.3) | 200 `{"data": [{"id", "label", "shared", "owner", "definition", "version", "modified_at"}]}`: the caller's own private views and the shared ones, shared first |
//! | `POST /views/{doctype}` body `{"label", "definition", "shared"?}` (M8.3) | 201 `{"data": view}`; `shared` needs the Administrator role |
//! | `PATCH /views/{doctype}/{id}` body `{"version", "label"?, "definition"?}` (M8.3) | 200 `{"data": view}`; another user's private view is 404 |
//! | `DELETE /views/{doctype}/{id}?version=N` (M8.3) | 204 |
//! | `GET /summary/{doctype}?group_by=FIELD&aggregates=sum:amount,...&filters=...` (M8.3) | 200 `{"data": {"groups": [{"key", "count", "values"}], "truncated"}}`, at most 500 groups, over the documents the caller may read |
//!
//! Anything else is 404 (`/api/v1/method` is not implemented), a wrong method is 405.
//!
//! # The desk
//!
//! With [`AppConfig::desk_dir`] set, `GET /desk/`, `/desk/index.html` and `/desk/assets/{name}`
//! serve the built desk from that directory (see the `desk` module for the allow-list and the
//! headers). The Host routing applies to them like to every route.

mod auth;
mod desk;
mod error;
pub mod json;
mod meta;
mod routes;
mod views;

use std::collections::HashMap;
use std::future::Future;
use std::ops::DerefMut;
use std::path::PathBuf;
use std::sync::Arc;

use axum::Router;
use axum::extract::{DefaultBodyLimit, Request, State};
use axum::http::header::{CACHE_CONTROL, HOST, X_CONTENT_TYPE_OPTIONS};
use axum::http::{HeaderMap, HeaderValue, StatusCode, Uri};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use forgebase_core::meta::Registry;
use forgebase_db::Pool;
use tokio::sync::Semaphore;
use tower::ServiceExt;

use crate::error::ApiError;

/// Settings of the HTTP API.
#[derive(Debug, Clone)]
pub struct AppConfig {
    /// The host names the implicit single site answers to (case-insensitive, port ignored), for
    /// example `localhost` or `erp.example.com`. An IPv6 address is written with brackets
    /// (`[::1]`). Read only by [`Sites::implicit`]; with a config file each site has its own
    /// hosts (see [`Sites::site`]).
    pub allowed_hosts: Vec<String>,
    /// Add `Secure` to the session cookie. `false` is for local development over plain HTTP only.
    pub secure_cookies: bool,
    /// The largest request body in bytes; a larger one is `413 payload_too_large`.
    pub body_limit_bytes: usize,
    /// How many logins may hash a password at the same time; more are `503 too_many_logins`
    /// with `Retry-After`. Argon2 uses about 19 MiB each, so this bounds memory. The effective
    /// limit is at most half of the pool's maximum size (at least 1), whatever is set here, so
    /// that with a pool of 2 or more connections logins can never hold all of them.
    pub max_concurrent_logins: usize,
    /// The directory of the built desk (`desk/dist`) to serve under `/desk/`, or `None` (the
    /// default) to serve no desk: `/desk/*` is then a 404. See the `desk` module.
    pub desk_dir: Option<PathBuf>,
}

impl Default for AppConfig {
    fn default() -> Self {
        Self {
            allowed_hosts: vec!["localhost".to_owned(), "127.0.0.1".to_owned()],
            secure_cookies: true,
            body_limit_bytes: 1024 * 1024,
            max_concurrent_logins: 8,
            desk_dir: None,
        }
    }
}

/// The state of **one site**: its connection pool, the DocType registry and the settings. A
/// router built from it can only reach that site's database.
#[derive(Clone)]
pub struct AppState {
    inner: Arc<Inner>,
}

struct Inner {
    pool: Pool,
    registry: Arc<Registry>,
    config: AppConfig,
    logins: Semaphore,
}

impl AppState {
    /// Creates the state. The registry must be the one the database schema was synced with.
    pub fn new(pool: Pool, registry: Arc<Registry>, config: AppConfig) -> Self {
        let logins = Semaphore::new(login_permits(
            config.max_concurrent_logins,
            pool.status().max_size,
        ));
        Self {
            inner: Arc::new(Inner {
                pool,
                registry,
                config,
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

/// The number of concurrent logins: `configured`, but at most half of the pool's maximum size
/// and at least 1, so that with a pool of 2 or more logins never take all the connections
/// other requests need.
fn login_permits(configured: usize, pool_max_size: usize) -> usize {
    configured.min((pool_max_size / 2).max(1)).max(1)
}

/// The sites a server answers for, by `Host` (M7.1, ADR 0014). Every site has its own
/// [`AppState`]: pool, registry and login limit. There is no shared pool, so a request for one
/// site can only reach that site's database.
pub struct Sites {
    /// Lower-cased host name to the router of its site.
    routes: HashMap<String, Router>,
    /// What a well-formed `Host` that no site owns gets.
    unknown_host: UnknownHost,
}

#[derive(Clone, Copy)]
enum UnknownHost {
    /// Implicit single-site mode: `400 invalid_host` (the DNS-rebinding allow-list).
    Invalid,
    /// Config file: `404 not_found`, "no such site".
    NotFound,
}

impl Sites {
    /// The implicit single site (no config file): it answers to
    /// [`AppConfig::allowed_hosts`] of `state`, and any other host is `400 invalid_host`.
    pub fn implicit(state: AppState) -> Self {
        let hosts = state.config().allowed_hosts.clone();
        let router = site_router(state);
        let routes = hosts
            .iter()
            .map(|h| (h.trim().to_ascii_lowercase(), router.clone()))
            .collect();
        Self {
            routes,
            unknown_host: UnknownHost::Invalid,
        }
    }

    /// No site yet, for a config file: add them with [`Sites::site`]. A well-formed host that no
    /// site owns is `404 not_found`.
    pub fn configured() -> Self {
        Self {
            routes: HashMap::new(),
            unknown_host: UnknownHost::NotFound,
        }
    }

    /// Adds a site that answers to `hosts` (case-insensitive, no port). A host that another
    /// site already owns is an error: one host is one site.
    pub fn site(mut self, hosts: &[String], state: AppState) -> Result<Self, String> {
        let router = site_router(state);
        for host in hosts {
            let host = host.trim().to_ascii_lowercase();
            if self.routes.insert(host.clone(), router.clone()).is_some() {
                return Err(format!("host {host} belongs to more than one site"));
            }
        }
        Ok(self)
    }

    /// The router of the site `headers` name, or the error response. A request target in
    /// absolute form (`GET http://host/path`) must name the same host as the `Host` header: the
    /// site is chosen by `Host` alone, so a target that disagrees is refused (`400 invalid_host`)
    /// instead of being routed by one and answered for the other.
    fn pick(&self, uri: &Uri, headers: &HeaderMap) -> Result<Router, ApiError> {
        let invalid = || {
            ApiError::new(
                StatusCode::BAD_REQUEST,
                "invalid_host",
                "invalid Host header",
            )
        };
        let mut values = headers.get_all(HOST).iter();
        let (Some(value), None) = (values.next(), values.next()) else {
            return Err(invalid());
        };
        let host = host_part(value.to_str().map_err(|_| invalid())?);
        if let Some(authority) = uri.authority()
            && authority.host().to_ascii_lowercase() != host
        {
            return Err(invalid());
        }
        if let Some(router) = self.routes.get(&host) {
            return Ok(router.clone());
        }
        match self.unknown_host {
            UnknownHost::NotFound if is_wellformed_host(&host) => Err(ApiError::new(
                StatusCode::NOT_FOUND,
                "not_found",
                "no such site",
            )),
            _ => Err(invalid()),
        }
    }
}

/// Builds the router of the implicit single site (see [`Sites::implicit`]).
pub fn router(state: AppState) -> Router {
    sites_router(Sites::implicit(state))
}

/// Builds the router that routes by `Host` to the router of each site, and adds the headers
/// every response gets.
pub fn sites_router(sites: Sites) -> Router {
    Router::new().fallback(dispatch).with_state(Arc::new(sites))
}

/// The routes of one site, without the Host routing (that is [`dispatch`]).
fn site_router(state: AppState) -> Router {
    let limit = state.config().body_limit_bytes;
    let mut router = Router::new()
        .route("/api/v1/login", post(routes::login))
        .route("/api/v1/logout", post(routes::logout))
        .route("/api/v1/session", get(routes::session))
        .route("/api/v1/meta", get(meta::list))
        .route("/api/v1/meta/{doctype}", get(meta::read))
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
        .route(
            "/api/v1/resource/{doctype}/{name}/transition",
            post(routes::transition),
        )
        .route(
            "/api/v1/resource/{doctype}/{name}/transitions",
            get(routes::transitions),
        )
        .route(
            "/api/v1/resource/{doctype}/{name}/history",
            get(routes::history),
        )
        .route(
            "/api/v1/views/{doctype}",
            get(views::list).post(views::create),
        )
        .route(
            "/api/v1/views/{doctype}/{id}",
            axum::routing::patch(views::update).delete(views::delete),
        )
        .route("/api/v1/summary/{doctype}", get(views::summary));
    // The desk routes exist only when a directory is configured.
    if state.config().desk_dir.is_some() {
        router = router
            .route("/desk/", get(desk::index))
            .route("/desk/index.html", get(desk::index))
            .route("/desk/assets/{name}", get(desk::asset));
    }
    router
        .fallback(|| async { ApiError::new(StatusCode::NOT_FOUND, "not_found", "no such route") })
        .method_not_allowed_fallback(|| async {
            ApiError::new(
                StatusCode::METHOD_NOT_ALLOWED,
                "method_not_allowed",
                "method not allowed",
            )
        })
        .layer(DefaultBodyLimit::max(limit))
        .with_state(state)
}

/// Serves the API on `listener` until `shutdown` completes (in-flight requests finish).
pub async fn serve(
    listener: tokio::net::TcpListener,
    sites: Sites,
    shutdown: impl Future<Output = ()> + Send + 'static,
) -> std::io::Result<()> {
    axum::serve(listener, sites_router(sites))
        .with_graceful_shutdown(shutdown)
        .await
}

/// The outermost layer, for every request: picks the site by `Host` (before anything else runs,
/// authentication included), hands the request to that site's router and adds the headers every
/// response gets.
async fn dispatch(State(sites): State<Arc<Sites>>, request: Request) -> Response {
    let mut response = match sites.pick(request.uri(), request.headers()) {
        Ok(router) => match router.oneshot(request).await {
            Ok(response) => response,
            Err(infallible) => match infallible {},
        },
        Err(error) => error.into_response(),
    };
    let headers = response.headers_mut();
    // `no-store` unless the handler chose a policy (only the hashed desk assets do).
    if !headers.contains_key(CACHE_CONTROL) {
        headers.insert(CACHE_CONTROL, HeaderValue::from_static("no-store"));
    }
    headers.insert(X_CONTENT_TYPE_OPTIONS, HeaderValue::from_static("nosniff"));
    response
}

/// True for a host (as `host_part` leaves it) that is a plausible name or address: letters,
/// digits, `-`, `.` and `_`, or a bracketed IPv6 address. Anything else is a malformed `Host`.
/// The config file uses the same rule for the hosts of a site, so a host that could never match
/// a request is refused when the file is loaded.
pub fn is_wellformed_host(host: &str) -> bool {
    if let Some(inner) = host.strip_prefix('[').and_then(|h| h.strip_suffix(']')) {
        return !inner.is_empty()
            && inner
                .bytes()
                .all(|b| b.is_ascii_hexdigit() || b == b':' || b == b'.');
    }
    !host.is_empty()
        && host
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'-' | b'.' | b'_'))
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
    use super::{AppConfig, AppState, Sites, host_part, login_permits, router, sites_router};
    use axum::body::Body;
    use axum::http::{Request, StatusCode};
    use forgebase_core::meta::Registry;
    use std::sync::Arc;
    use tower::ServiceExt;

    #[test]
    fn login_permits_never_take_every_connection() {
        assert_eq!(login_permits(8, 16), 8);
        assert_eq!(login_permits(8, 8), 4);
        assert_eq!(login_permits(8, 9), 4);
        assert_eq!(login_permits(8, 4), 2);
        // A pool of one or two still allows one login at a time.
        assert_eq!(login_permits(8, 2), 1);
        assert_eq!(login_permits(8, 1), 1);
        assert_eq!(login_permits(8, 0), 1);
        // 0 configured still means 1; a small setting is kept.
        assert_eq!(login_permits(0, 16), 1);
        assert_eq!(login_permits(2, 64), 2);
    }

    /// A pool that never connects: deadpool creates it lazily.
    fn lazy_pool(max_size: usize) -> forgebase_db::Pool {
        use deadpool_postgres::{Config, PoolConfig, Runtime, tokio_postgres::NoTls};
        let mut config = Config::new();
        config.url = Some("postgres://nobody@127.0.0.1:1/none".to_owned());
        config.pool = Some(PoolConfig::new(max_size));
        config
            .create_pool(Some(Runtime::Tokio1), NoTls)
            .expect("lazy pool")
    }

    #[tokio::test]
    async fn a_login_over_the_bound_is_503_without_touching_the_database() {
        let config = AppConfig {
            max_concurrent_logins: 1,
            ..AppConfig::default()
        };
        let state = AppState::new(lazy_pool(2), Arc::new(Registry::new()), config);
        assert_eq!(state.logins().available_permits(), 1);
        let _taken = state.logins().try_acquire().expect("the only permit");
        let request = Request::builder()
            .method("POST")
            .uri("/api/v1/login")
            .header("host", "localhost")
            .header("content-type", "application/json")
            .body(Body::from(r#"{"username":"a","password":"b"}"#))
            .expect("request");
        let response = router(state.clone())
            .oneshot(request)
            .await
            .expect("infallible");
        assert_eq!(response.status(), StatusCode::SERVICE_UNAVAILABLE);
        assert_eq!(response.headers()["retry-after"], "1");
        let body = axum::body::to_bytes(response.into_body(), usize::MAX)
            .await
            .expect("body");
        assert!(String::from_utf8_lossy(&body).contains("too_many_logins"));
    }

    async fn status(
        router: &axum::Router,
        host: Option<&str>,
        extra: &[(&str, &str)],
    ) -> (StatusCode, String) {
        let mut builder = Request::builder().uri("/api/v1/nothing");
        if let Some(host) = host {
            builder = builder.header("host", host);
        }
        for (name, value) in extra {
            builder = builder.header(*name, *value);
        }
        let response = router
            .clone()
            .oneshot(builder.body(Body::empty()).expect("request"))
            .await
            .expect("infallible");
        // Every answer, the refusals included, carries the response headers.
        assert_eq!(response.headers()["cache-control"], "no-store");
        assert_eq!(response.headers()["x-content-type-options"], "nosniff");
        let status = response.status();
        let body = axum::body::to_bytes(response.into_body(), usize::MAX)
            .await
            .expect("body");
        (status, String::from_utf8_lossy(&body).into_owned())
    }

    fn state() -> AppState {
        AppState::new(
            lazy_pool(2),
            Arc::new(Registry::new()),
            AppConfig::default(),
        )
    }

    #[tokio::test]
    async fn the_implicit_site_answers_400_to_every_host_it_does_not_own() {
        let router = router(state());
        // An owned host reaches the site's router (an unknown route is its own 404).
        let (owned, body) = status(&router, Some("LocalHost:8000"), &[]).await;
        assert_eq!(owned, StatusCode::NOT_FOUND);
        assert!(body.contains("no such route"), "{body}");
        for host in [
            None,
            Some("evil.example"),
            Some("bad host"),
            Some("localhost:80@evil.com"),
        ] {
            let (code, body) = status(&router, host, &[]).await;
            assert_eq!(code, StatusCode::BAD_REQUEST, "{host:?}");
            assert!(body.contains("invalid_host"), "{body}");
        }
    }

    #[tokio::test]
    async fn with_sites_an_unknown_host_is_404_and_a_malformed_one_is_400() {
        let sites = Sites::configured()
            .site(&["a.test".to_owned(), "A2.test".to_owned()], state())
            .expect("a")
            .site(&["b.test".to_owned()], state())
            .expect("b");
        let router = sites_router(sites);
        // Both sites' hosts reach a site router (whose unknown route is "no such route").
        for host in ["a.test", "a2.test:8000", "B.TEST"] {
            let (code, body) = status(&router, Some(host), &[]).await;
            assert_eq!(code, StatusCode::NOT_FOUND, "{host}");
            assert!(body.contains("no such route"), "{body}");
        }
        // No site owns it: a generic 404 envelope.
        for host in ["c.test", "localhost", "[::1]", "10.0.0.1:80"] {
            let (code, body) = status(&router, Some(host), &[]).await;
            assert_eq!(code, StatusCode::NOT_FOUND, "{host}");
            assert!(
                body.contains("\"not_found\"") && body.contains("no such site"),
                "{body}"
            );
        }
        // Missing, malformed and non-UTF-8 hosts stay 400.
        for host in [
            None,
            Some(""),
            Some("bad host"),
            Some("a.test:80@evil.com"),
            Some("a.test/x"),
        ] {
            let (code, body) = status(&router, host, &[]).await;
            assert_eq!(code, StatusCode::BAD_REQUEST, "{host:?}");
            assert!(body.contains("invalid_host"), "{body}");
        }
        // Forwarding headers never choose the site.
        let forwarded = [("x-forwarded-host", "a.test"), ("forwarded", "host=a.test")];
        let (code, body) = status(&router, Some("c.test"), &forwarded).await;
        assert_eq!(code, StatusCode::NOT_FOUND);
        assert!(
            body.contains("\"not_found\"") && !body.contains("no such route"),
            "{body}"
        );
    }

    #[test]
    fn one_host_is_one_site() {
        let result = Sites::configured()
            .site(&["a.test".to_owned()], state_sync())
            .and_then(|s| s.site(&["A.test".to_owned()], state_sync()));
        assert!(result.err().expect("refused").contains("a.test"));
    }

    fn state_sync() -> AppState {
        AppState::new(
            lazy_pool(2),
            Arc::new(Registry::new()),
            AppConfig::default(),
        )
    }

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
