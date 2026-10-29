//! Serves the built desk (the React app of `desk/`) from a directory on the same origin under
//! `/desk/` (M4.1). The routes exist only when [`AppConfig::desk_dir`](crate::AppConfig) is set;
//! otherwise `/desk/*` is the ordinary 404.
//!
//! **Strict allow-list.** Only `index.html` (at `/desk/` and `/desk/index.html`) and files
//! directly in `assets/` are served. The file name is taken from the **raw** request path (no
//! percent-decoding), must match `^[A-Za-z0-9][A-Za-z0-9._-]*$` and has an extension with a known
//! content type; anything else, including `..`, `%2e%2e`, `%2f`, backslashes, NUL bytes and
//! sub-directories, is a 404 that never touches the file system. A file is read whole into
//! memory (the assets of a built desk are small); a directory, a missing file and an unknown
//! extension are 404, another read error is a generic 500 (logged to standard error).
//!
//! **Headers.** On top of the headers every response gets (`nosniff`, from the Host check): a
//! strict `Content-Security-Policy` (same-origin scripts and styles only, no inline code, no
//! framing), `X-Frame-Options: DENY` and `Referrer-Policy: same-origin`. `index.html` is
//! `no-store` (it names the hashed assets); the hashed files under `assets/` are
//! `public, max-age=31536000, immutable`.

use std::path::Path;

use axum::extract::State;
use axum::http::header::{
    CACHE_CONTROL, CONTENT_SECURITY_POLICY, CONTENT_TYPE, REFERRER_POLICY, X_FRAME_OPTIONS,
};
use axum::http::{StatusCode, Uri};
use axum::response::{IntoResponse, Response};

use crate::AppState;
use crate::error::ApiError;

/// The Content-Security-Policy of every desk response.
const CSP: &str = "default-src 'self'; script-src 'self'; style-src 'self'; \
                   img-src 'self' data:; connect-src 'self'; font-src 'self'; object-src 'none'; \
                   base-uri 'none'; form-action 'self'; frame-ancestors 'none'";
const NO_STORE: &str = "no-store";
const IMMUTABLE: &str = "public, max-age=31536000, immutable";
const ASSETS_PREFIX: &str = "/desk/assets/";

/// The content type of a file name by its extension; `None` (so 404) for an unknown one.
fn content_type(name: &str) -> Option<&'static str> {
    let (_, extension) = name.rsplit_once('.')?;
    Some(match extension.to_ascii_lowercase().as_str() {
        "js" => "text/javascript; charset=utf-8",
        "css" => "text/css; charset=utf-8",
        "html" => "text/html; charset=utf-8",
        "svg" => "image/svg+xml",
        "png" => "image/png",
        "ico" => "image/x-icon",
        "woff2" => "font/woff2",
        "json" | "map" => "application/json",
        _ => return None,
    })
}

/// `^[A-Za-z0-9][A-Za-z0-9._-]*$`, at most 255 bytes.
fn valid_asset_name(name: &str) -> bool {
    let bytes = name.as_bytes();
    bytes.len() <= 255
        && bytes.first().is_some_and(u8::is_ascii_alphanumeric)
        && bytes
            .iter()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'.' | b'_' | b'-'))
}

async fn serve_file(path: &Path, content_type: &'static str, cache: &'static str) -> Response {
    match tokio::fs::read(path).await {
        Ok(bytes) => (
            StatusCode::OK,
            [
                (CONTENT_TYPE, content_type),
                (CACHE_CONTROL, cache),
                (CONTENT_SECURITY_POLICY, CSP),
                (X_FRAME_OPTIONS, "DENY"),
                (REFERRER_POLICY, "same-origin"),
            ],
            bytes,
        )
            .into_response(),
        Err(e)
            if matches!(
                e.kind(),
                std::io::ErrorKind::NotFound | std::io::ErrorKind::IsADirectory
            ) =>
        {
            not_found()
        }
        Err(e) => ApiError::internal(&e).into_response(),
    }
}

fn not_found() -> Response {
    ApiError::new(StatusCode::NOT_FOUND, "not_found", "no such route").into_response()
}

/// `GET /desk/` and `GET /desk/index.html`.
pub async fn index(State(state): State<AppState>) -> Response {
    match &state.config().desk_dir {
        Some(dir) => {
            serve_file(
                &dir.join("index.html"),
                "text/html; charset=utf-8",
                NO_STORE,
            )
            .await
        }
        None => not_found(),
    }
}

/// `GET /desk/assets/{name}`; the name is checked on the raw path (see the module docs).
pub async fn asset(State(state): State<AppState>, uri: Uri) -> Response {
    let Some(dir) = &state.config().desk_dir else {
        return not_found();
    };
    let Some(name) = uri.path().strip_prefix(ASSETS_PREFIX) else {
        return not_found();
    };
    let Some(content_type) = content_type(name).filter(|_| valid_asset_name(name)) else {
        return not_found();
    };
    serve_file(&dir.join("assets").join(name), content_type, IMMUTABLE).await
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn asset_names_are_a_strict_allow_list() {
        for good in ["index-AbC123.js", "a.css", "Z9_-.x.map", "0.png"] {
            assert!(valid_asset_name(good), "{good}");
        }
        for bad in [
            "", ".hidden", "-a.js", "_a.js", "..", "../a.js", "a/b.js", "a\\b.js", "a%2fb.js",
            "%2e%2e", "a b.js", "a\0.js", "é.js", "a.js/",
        ] {
            assert!(!valid_asset_name(bad), "{bad:?}");
        }
        assert!(!valid_asset_name(&"a".repeat(256)));
    }

    #[test]
    fn content_types_by_extension() {
        assert_eq!(content_type("a.js"), Some("text/javascript; charset=utf-8"));
        assert_eq!(content_type("a.CSS"), Some("text/css; charset=utf-8"));
        assert_eq!(content_type("a.woff2"), Some("font/woff2"));
        assert_eq!(content_type("a.exe"), None);
        assert_eq!(content_type("noextension"), None);
    }
}
