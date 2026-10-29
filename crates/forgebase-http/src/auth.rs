//! Authentication and CSRF for the routes that need a user (see the crate documentation).

use axum::extract::FromRequestParts;
use axum::http::header::{AUTHORIZATION, COOKIE};
use axum::http::request::Parts;
use axum::http::{HeaderMap, Method};
use forgebase_db::Caller;
use forgebase_db::auth::{self, AuthUser};

use crate::AppState;
use crate::error::ApiError;

/// Name of the session cookie.
pub const SESSION_COOKIE: &str = "fb_session";
/// Name of the CSRF header.
const CSRF_HEADER: &str = "x-forgebase-csrf";

/// How the request proved who it is.
pub enum Credential {
    /// `Authorization: token <key>:<secret>`.
    ApiToken,
    /// The session cookie (its value is the session token).
    Session(String),
}

/// An authenticated request: the user and how they authenticated. Extracting it does the whole
/// check, so a handler that has it needs nothing else: authentication first, then CSRF.
pub struct Authenticated {
    pub user: AuthUser,
    pub credential: Credential,
}

impl Authenticated {
    /// The caller for the document functions. Always a user, never the system context.
    pub fn caller(&self) -> Caller {
        self.user.caller()
    }
}

impl FromRequestParts<AppState> for Authenticated {
    type Rejection = ApiError;

    async fn from_request_parts(
        parts: &mut Parts,
        state: &AppState,
    ) -> Result<Self, Self::Rejection> {
        // An Authorization header decides alone: a bad one is a 401, never a fallback to the
        // cookie.
        if parts.headers.contains_key(AUTHORIZATION) {
            let (key, secret) =
                token_credentials(&parts.headers).ok_or_else(ApiError::unauthenticated)?;
            let client = state.client().await?;
            let user = auth::validate_api_token(&**client, key, secret)
                .await?
                .ok_or_else(ApiError::unauthenticated)?;
            return Ok(Self {
                user,
                credential: Credential::ApiToken,
            });
        }

        let token = session_cookie(&parts.headers).ok_or_else(ApiError::unauthenticated)?;
        let client = state.client().await?;
        let user = auth::validate_session(&**client, token)
            .await?
            .ok_or_else(ApiError::unauthenticated)?;
        if !matches!(parts.method, Method::GET | Method::HEAD) {
            // Exactly one value, like the `Authorization` header.
            let mut values = parts.headers.get_all(CSRF_HEADER).iter();
            let (Some(value), None) = (values.next(), values.next()) else {
                return Err(ApiError::csrf_failed());
            };
            let csrf = value.to_str().map_err(|_| ApiError::csrf_failed())?;
            if !auth::check_csrf(&**client, token, csrf).await? {
                return Err(ApiError::csrf_failed());
            }
        }
        Ok(Self {
            user,
            credential: Credential::Session(token.to_owned()),
        })
    }
}

/// `key` and `secret` of exactly one `Authorization: token <key>:<secret>` header.
fn token_credentials(headers: &HeaderMap) -> Option<(&str, &str)> {
    let mut values = headers.get_all(AUTHORIZATION).iter();
    let (Some(value), None) = (values.next(), values.next()) else {
        return None;
    };
    let (scheme, rest) = value.to_str().ok()?.split_once(' ')?;
    if !scheme.eq_ignore_ascii_case("token") {
        return None;
    }
    rest.trim().split_once(':')
}

/// The value of the first `fb_session` cookie.
fn session_cookie(headers: &HeaderMap) -> Option<&str> {
    headers
        .get_all(COOKIE)
        .iter()
        .filter_map(|v| v.to_str().ok())
        .flat_map(|v| v.split(';'))
        .filter_map(|pair| pair.trim().split_once('='))
        .find(|(name, _)| *name == SESSION_COOKIE)
        .map(|(_, value)| value)
}

#[cfg(test)]
mod tests {
    use axum::http::HeaderValue;

    use super::*;

    fn headers(pairs: &[(&'static str, &'static str)]) -> HeaderMap {
        let mut map = HeaderMap::new();
        for (name, value) in pairs {
            map.append(*name, HeaderValue::from_static(value));
        }
        map
    }

    #[test]
    fn token_header_parsing() {
        let ok = headers(&[("authorization", "Token abc:def")]);
        assert_eq!(token_credentials(&ok), Some(("abc", "def")));
        for bad in ["Bearer abc:def", "token abc", "token", "abc:def"] {
            let h = headers(&[("authorization", bad)]);
            assert_eq!(token_credentials(&h), None, "{bad}");
        }
        let twice = headers(&[
            ("authorization", "token a:b"),
            ("authorization", "token a:b"),
        ]);
        assert_eq!(token_credentials(&twice), None);
    }

    #[test]
    fn cookie_parsing() {
        let h = headers(&[("cookie", "a=1; fb_session=tok; b=2")]);
        assert_eq!(session_cookie(&h), Some("tok"));
        let h = headers(&[("cookie", "xfb_session=tok")]);
        assert_eq!(session_cookie(&h), None);
        let h = headers(&[("cookie", "a=1"), ("cookie", "fb_session=t2")]);
        assert_eq!(session_cookie(&h), Some("t2"));
    }
}
