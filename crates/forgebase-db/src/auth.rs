//! Users, roles, sessions, API tokens and login lockout (M3.1, ADR 0010).
//!
//! Everything here works on the framework tables of migration 0003 (`fb_user`, `fb_user_role`,
//! `fb_session`, `fb_api_token`, `fb_login_failure`). There is no HTTP, cookie or permission
//! handling yet (M3.2 and M3.3): this module only stores and checks credentials.
//!
//! **Secrets.** A password is stored as an Argon2id PHC string (m = 19456 KiB, t = 2, p = 1,
//! random 16-byte salt). Verification uses the parameters inside the stored string, so the
//! parameters can be raised later without breaking old hashes. Session tokens, CSRF tokens and
//! API secrets are 32 random bytes (from the operating system through `getrandom`) written as 64
//! lowercase hex characters; only their SHA-256 digest (computed by PostgreSQL) is stored. An API
//! key (the public identifier of an API token) is 16 random bytes as 32 hex characters. No
//! password, token or secret is ever stored in plaintext, logged, or part of an error, `Debug`
//! or `Display` output; the only places that return a secret are [`login`] and
//! [`create_api_token`], once.
//!
//! **Rules.** A username is 1 to 140 characters, ASCII letters, digits and `_ . @ -`, and is
//! case-sensitive (`Alice` and `alice` are two users). A role is 1 to 140 characters, not blank,
//! without control characters. A password is 8 to 1024 bytes: the upper bound keeps a client
//! from making the server hash megabytes. Argon2 runs on tokio's blocking thread pool
//! (`spawn_blocking`), never on the async executor.
//!
//! **Transactions.** Every function that changes more than one row runs in `client.transaction()`
//! (a transaction on a pooled connection, a savepoint inside a caller's transaction), like the
//! document functions. [`login`] additionally must be called on a **pooled client**, not inside a
//! caller-owned transaction: its failure accounting is committed together with the failed login,
//! and a caller's transaction that rolls back (which a failed login usually causes) would undo
//! it, so the lockout would never take effect.

use std::fmt;
use std::sync::OnceLock;

use argon2::password_hash::{PasswordHasher, PasswordVerifier};
use argon2::{Algorithm, Argon2, Params, Version};
use time::OffsetDateTime;
use tokio_postgres::GenericClient;
use tokio_postgres::error::SqlState;
use uuid::Uuid;

use crate::error::DbError;

/// The id of a user.
pub type UserId = Uuid;

/// Failed logins of one username before it is locked.
const MAX_FAILURES: i32 = 5;
/// How long a locked username cannot log in, in seconds (15 minutes).
const LOCK_SECS: f64 = 15.0 * 60.0;
/// Absolute lifetime of a session, in seconds (7 days from login).
const SESSION_ABSOLUTE_SECS: f64 = 7.0 * 24.0 * 3600.0;
/// A session with no use for this long expires, in seconds (24 hours).
const SESSION_IDLE_SECS: f64 = 24.0 * 3600.0;

const USERNAME_MAX: usize = 140;
const ROLE_MAX: usize = 140;
/// Password length bounds in bytes.
const PASSWORD_MIN: usize = 8;
const PASSWORD_MAX: usize = 1024;

/// Hex length of a session token, CSRF token and API secret (32 bytes).
const SECRET_HEX_LEN: usize = 64;
/// Hex length of an API key (16 bytes).
const KEY_HEX_LEN: usize = 32;

/// SQL expression for the SHA-256 digest of text parameter `$n`.
macro_rules! digest {
    ($n:literal) => {
        concat!("sha256(convert_to($", $n, "::text, 'UTF8'))")
    };
}

/// Everything that can go wrong in the auth functions.
#[derive(Debug, thiserror::Error)]
pub enum AuthError {
    /// The login failed. The same error for an unknown user, a wrong password, a disabled user
    /// and a locked username, so it reveals nothing about which usernames exist.
    #[error("invalid username or password")]
    InvalidCredentials,
    /// The username breaks the username rule.
    #[error("invalid username: {0}")]
    InvalidUsername(&'static str),
    /// The role breaks the role rule.
    #[error("invalid role: {0}")]
    InvalidRole(&'static str),
    /// The password breaks the password rule.
    #[error("invalid password: {0}")]
    InvalidPassword(&'static str),
    /// A user with this username exists already.
    #[error("a user with this username already exists")]
    UserExists,
    /// No user has this username.
    #[error("user `{0}` not found")]
    UserNotFound(String),
    /// No API token has this key.
    #[error("API token `{0}` not found")]
    TokenNotFound(String),
    /// Hashing or verifying a password failed for a reason other than a wrong password.
    #[error("password hashing failed")]
    Hash,
    /// The operating system could not provide random bytes.
    #[error("cannot get random bytes from the operating system")]
    Random,
    /// A database error.
    #[error(transparent)]
    Db(#[from] DbError),
}

impl From<tokio_postgres::Error> for AuthError {
    fn from(error: tokio_postgres::Error) -> Self {
        AuthError::Db(error.into())
    }
}

/// An authenticated user.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AuthUser {
    /// The user id.
    pub id: UserId,
    /// The username.
    pub username: String,
    /// The user's roles, sorted.
    pub roles: Vec<String>,
}

/// The result of a successful [`login`]. The tokens are returned only here and cannot be read
/// back later: the database holds their digests only. `Debug` redacts them.
pub struct LoginSession {
    /// The session token: send it to the client (M3.3: in a cookie), never log it.
    pub token: String,
    /// The CSRF token that belongs to this session.
    pub csrf_token: String,
    /// The user the session belongs to.
    pub user: AuthUser,
    /// When the session expires at the latest (7 days after login), regardless of use.
    pub expires_at: OffsetDateTime,
}

impl fmt::Debug for LoginSession {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("LoginSession")
            .field("token", &"<redacted>")
            .field("csrf_token", &"<redacted>")
            .field("user", &self.user)
            .field("expires_at", &self.expires_at)
            .finish()
    }
}

/// A new API token. The secret is returned only here. `Debug` redacts it.
pub struct ApiTokenSecret {
    /// The public identifier of the token.
    pub key: String,
    /// The secret. Show it to the owner once and never log it.
    pub secret: String,
}

impl fmt::Debug for ApiTokenSecret {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("ApiTokenSecret")
            .field("key", &self.key)
            .field("secret", &"<redacted>")
            .finish()
    }
}

// ---------------------------------------------------------------------------------------------
// Rules
// ---------------------------------------------------------------------------------------------

fn check_username(username: &str) -> Result<(), AuthError> {
    let valid_chars = username
        .chars()
        .all(|c| c.is_ascii_alphanumeric() || matches!(c, '_' | '.' | '@' | '-'));
    if username.is_empty() || username.len() > USERNAME_MAX {
        Err(AuthError::InvalidUsername("must be 1 to 140 characters"))
    } else if !valid_chars {
        Err(AuthError::InvalidUsername(
            "may only contain ASCII letters, digits and _ . @ -",
        ))
    } else {
        Ok(())
    }
}

fn check_role(role: &str) -> Result<(), AuthError> {
    let length = role.chars().count();
    if length == 0 || length > ROLE_MAX {
        Err(AuthError::InvalidRole("must be 1 to 140 characters"))
    } else if role.trim().is_empty() {
        Err(AuthError::InvalidRole("must not be blank"))
    } else if role.chars().any(char::is_control) {
        Err(AuthError::InvalidRole(
            "must not contain control characters",
        ))
    } else {
        Ok(())
    }
}

fn check_password(password: &str) -> Result<(), AuthError> {
    if password.len() < PASSWORD_MIN || password.len() > PASSWORD_MAX {
        Err(AuthError::InvalidPassword("must be 8 to 1024 bytes long"))
    } else {
        Ok(())
    }
}

// ---------------------------------------------------------------------------------------------
// Randomness and password hashing
// ---------------------------------------------------------------------------------------------

/// `bytes` random bytes as lowercase hex.
fn random_hex(bytes: usize) -> Result<String, AuthError> {
    let mut buffer = vec![0u8; bytes];
    getrandom::fill(&mut buffer).map_err(|_| AuthError::Random)?;
    let mut text = String::with_capacity(bytes * 2);
    for byte in buffer {
        text.push(char::from_digit(u32::from(byte >> 4), 16).unwrap_or('0'));
        text.push(char::from_digit(u32::from(byte & 0xf), 16).unwrap_or('0'));
    }
    Ok(text)
}

/// Whether `text` has exactly `len` lowercase hex characters (what [`random_hex`] produces).
/// Anything else cannot be one of our tokens, so it is rejected without touching the database.
fn is_hex(text: &str, len: usize) -> bool {
    text.len() == len && text.bytes().all(|b| matches!(b, b'0'..=b'9' | b'a'..=b'f'))
}

fn argon2() -> Result<Argon2<'static>, AuthError> {
    let params = Params::new(19456, 2, 1, None).map_err(|_| AuthError::Hash)?;
    Ok(Argon2::new(Algorithm::Argon2id, Version::V0x13, params))
}

/// Hashes a password into a PHC string. CPU and memory heavy: call from a blocking thread.
fn hash_blocking(password: &[u8]) -> Result<String, AuthError> {
    let mut salt = [0u8; 16];
    getrandom::fill(&mut salt).map_err(|_| AuthError::Random)?;
    let hash = argon2()?
        .hash_password_with_salt(password, &salt)
        .map_err(|_| AuthError::Hash)?;
    Ok(hash.to_string())
}

/// A hash of a random password nobody knows, verified for unknown and disabled users so that
/// they cost the same time as a real check. Computed once.
fn dummy_hash() -> &'static str {
    static DUMMY: OnceLock<String> = OnceLock::new();
    DUMMY.get_or_init(|| {
        let password = random_hex(32).unwrap_or_default();
        hash_blocking(password.as_bytes()).unwrap_or_default()
    })
}

async fn hash_password(password: &str) -> Result<String, AuthError> {
    let password = password.to_owned();
    tokio::task::spawn_blocking(move || hash_blocking(password.as_bytes()))
        .await
        .map_err(|_| AuthError::Hash)?
}

/// Verifies `password` against `hash`; with `None` it verifies the dummy hash and is always
/// `false`. A malformed stored hash is also `false` (the login then fails like any other).
async fn verify_password(password: &str, hash: Option<String>) -> Result<bool, AuthError> {
    let password = password.to_owned();
    tokio::task::spawn_blocking(move || {
        let real = hash.is_some();
        let hash = hash.unwrap_or_else(|| dummy_hash().to_owned());
        let matches = argon2().is_ok_and(|argon2| {
            argon2
                .verify_password(password.as_bytes(), hash.as_str())
                .is_ok()
        });
        matches && real
    })
    .await
    .map_err(|_| AuthError::Hash)
}

// ---------------------------------------------------------------------------------------------
// Users and roles
// ---------------------------------------------------------------------------------------------

/// Creates an enabled user with the given roles (duplicates are ignored) and returns its id.
///
/// Fails with [`AuthError::UserExists`] if the username is taken, and with the rule errors for a
/// bad username, role or password (nothing is hashed or written then).
pub async fn create_user(
    client: &mut impl GenericClient,
    username: &str,
    password: &str,
    roles: &[&str],
) -> Result<UserId, AuthError> {
    check_username(username)?;
    check_password(password)?;
    for role in roles {
        check_role(role)?;
    }
    let hash = hash_password(password).await?;
    let id = Uuid::now_v7();
    let tx = client.transaction().await?;
    let inserted = tx
        .execute(
            "INSERT INTO fb_user (id, username, password_hash) VALUES ($1, $2, $3)",
            &[&id, &username, &hash],
        )
        .await;
    if let Err(error) = inserted {
        return Err(if error.code() == Some(&SqlState::UNIQUE_VIOLATION) {
            AuthError::UserExists
        } else {
            error.into()
        });
    }
    for role in roles {
        tx.execute(
            "INSERT INTO fb_user_role (user_id, role) VALUES ($1, $2) ON CONFLICT DO NOTHING",
            &[&id, role],
        )
        .await?;
    }
    tx.commit().await?;
    Ok(id)
}

/// Changes a user's password and ends all of the user's sessions. API tokens stay valid.
pub async fn set_password(
    client: &mut impl GenericClient,
    username: &str,
    password: &str,
) -> Result<(), AuthError> {
    check_username(username)?;
    check_password(password)?;
    let hash = hash_password(password).await?;
    let tx = client.transaction().await?;
    let id = user_id(&tx, username).await?;
    tx.execute(
        "UPDATE fb_user SET password_hash = $1 WHERE id = $2",
        &[&hash, &id],
    )
    .await?;
    tx.execute("DELETE FROM fb_session WHERE user_id = $1", &[&id])
        .await?;
    tx.commit().await?;
    Ok(())
}

/// Enables or disables a user. A disabled user cannot log in, and its sessions and API tokens
/// stop working; disabling also deletes its sessions.
pub async fn set_enabled(
    client: &mut impl GenericClient,
    username: &str,
    enabled: bool,
) -> Result<(), AuthError> {
    check_username(username)?;
    let tx = client.transaction().await?;
    let id = user_id(&tx, username).await?;
    tx.execute(
        "UPDATE fb_user SET enabled = $1 WHERE id = $2",
        &[&enabled, &id],
    )
    .await?;
    if !enabled {
        tx.execute("DELETE FROM fb_session WHERE user_id = $1", &[&id])
            .await?;
    }
    tx.commit().await?;
    Ok(())
}

/// Gives a user a role. Returns `false` if the user had it already.
pub async fn add_role(
    client: &mut impl GenericClient,
    username: &str,
    role: &str,
) -> Result<bool, AuthError> {
    check_username(username)?;
    check_role(role)?;
    let tx = client.transaction().await?;
    let id = user_id(&tx, username).await?;
    let added = tx
        .execute(
            "INSERT INTO fb_user_role (user_id, role) VALUES ($1, $2) ON CONFLICT DO NOTHING",
            &[&id, &role],
        )
        .await?;
    tx.commit().await?;
    Ok(added > 0)
}

/// Takes a role from a user. Returns `false` if the user did not have it.
pub async fn remove_role(
    client: &mut impl GenericClient,
    username: &str,
    role: &str,
) -> Result<bool, AuthError> {
    check_username(username)?;
    check_role(role)?;
    let tx = client.transaction().await?;
    let id = user_id(&tx, username).await?;
    let removed = tx
        .execute(
            "DELETE FROM fb_user_role WHERE user_id = $1 AND role = $2",
            &[&id, &role],
        )
        .await?;
    tx.commit().await?;
    Ok(removed > 0)
}

async fn user_id(client: &impl GenericClient, username: &str) -> Result<UserId, AuthError> {
    client
        .query_opt("SELECT id FROM fb_user WHERE username = $1", &[&username])
        .await?
        .map(|row| row.get(0))
        .ok_or_else(|| AuthError::UserNotFound(username.to_owned()))
}

async fn load_roles(client: &impl GenericClient, id: UserId) -> Result<Vec<String>, AuthError> {
    let rows = client
        .query("SELECT role FROM fb_user_role WHERE user_id = $1", &[&id])
        .await?;
    let mut roles: Vec<String> = rows.iter().map(|row| row.get(0)).collect();
    roles.sort();
    Ok(roles)
}

async fn load_user(
    client: &impl GenericClient,
    id: UserId,
    username: String,
) -> Result<AuthUser, AuthError> {
    Ok(AuthUser {
        id,
        username,
        roles: load_roles(client, id).await?,
    })
}

// ---------------------------------------------------------------------------------------------
// Login, sessions, CSRF
// ---------------------------------------------------------------------------------------------

/// Checks a username and password and starts a new session.
///
/// **Uniform failure.** An unknown user, a disabled user, a wrong password and a locked username
/// all fail with [`AuthError::InvalidCredentials`], and each of them spends one Argon2
/// verification (a dummy hash when there is no real one to check), so neither the error nor the
/// time reveals which usernames exist. A username or password that breaks the rules cannot
/// match anything and fails the same way, without being stored. Database and internal errors
/// are other errors.
///
/// **Lockout.** Failed attempts are counted per submitted username in `fb_login_failure`, also
/// for names that do not exist. The fifth failure locks the username for 15 minutes and resets
/// the counter; while it is locked every attempt fails without checking the password (the
/// correct password too) and does not extend the lock. A successful login deletes the counter.
/// The counting runs in a transaction of its own that holds the username's row lock until the
/// outcome is known, so concurrent attempts for one username are serialized and cannot exceed
/// the limit. The counting is committed even though the login fails. For that this function must
/// get a pooled client, **not** a caller-owned transaction (see the module documentation).
/// Rows for unknown usernames are not cleaned up automatically.
///
/// **Session.** A successful login always creates a new session (new token, new CSRF token);
/// it is valid until 7 days after login, and expires earlier after
/// 24 hours without use (see [`validate_session`]).
pub async fn login(
    client: &mut impl GenericClient,
    username: &str,
    password: &str,
) -> Result<LoginSession, AuthError> {
    if check_username(username).is_err() || check_password(password).is_err() {
        verify_password("", None).await?;
        return Err(AuthError::InvalidCredentials);
    }
    let tx = client.transaction().await?;
    // Insert-or-lock in one statement: the row exists afterwards and is locked by this
    // transaction, even if another transaction deletes or creates it at the same moment.
    let locked: bool = tx
        .query_one(
            "INSERT INTO fb_login_failure (username, failures) VALUES ($1, 0) \
             ON CONFLICT (username) DO UPDATE SET failures = fb_login_failure.failures \
             RETURNING coalesce(locked_until > now(), false)",
            &[&username],
        )
        .await?
        .get(0);
    if locked {
        tx.commit().await?;
        verify_password("", None).await?;
        return Err(AuthError::InvalidCredentials);
    }

    let user = tx
        .query_opt(
            "SELECT id, password_hash, enabled FROM fb_user WHERE username = $1",
            &[&username],
        )
        .await?;
    let (found, hash) = match &user {
        Some(row) if row.get::<_, bool>(2) => (Some(row.get::<_, Uuid>(0)), Some(row.get(1))),
        _ => (None, None),
    };
    let verified = verify_password(password, hash).await?;
    let Some(id) = found.filter(|_| verified) else {
        tx.execute(
            "UPDATE fb_login_failure SET \
             failures = CASE WHEN failures + 1 >= $2 THEN 0 ELSE failures + 1 END, \
             locked_until = CASE WHEN failures + 1 >= $2 \
                 THEN now() + make_interval(secs => $3::float8) ELSE locked_until END \
             WHERE username = $1",
            &[&username, &MAX_FAILURES, &LOCK_SECS],
        )
        .await?;
        tx.commit().await?;
        return Err(AuthError::InvalidCredentials);
    };

    tx.execute(
        "DELETE FROM fb_login_failure WHERE username = $1",
        &[&username],
    )
    .await?;
    let token = random_hex(32)?;
    let csrf_token = random_hex(32)?;
    let expires_at: OffsetDateTime = tx
        .query_one(
            concat!(
                "INSERT INTO fb_session (token_hash, csrf_hash, user_id, expires_at) \
                 VALUES (",
                digest!("1"),
                ", ",
                digest!("2"),
                ", $3, now() + make_interval(secs => $4::float8)) RETURNING expires_at"
            ),
            &[&token, &csrf_token, &id, &SESSION_ABSOLUTE_SECS],
        )
        .await?
        .get(0);
    let user = load_user(&tx, id, username.to_owned()).await?;
    tx.commit().await?;
    Ok(LoginSession {
        token,
        csrf_token,
        user,
        expires_at,
    })
}

/// Looks up the user of a session token.
///
/// The session is valid if its token matches, the user is enabled, it is younger than 7 days
/// (`expires_at`) and was used less than 24 hours ago (`last_seen_at`). A valid session is
/// touched (`last_seen_at = now()`: one small write per call); an expired one is deleted.
/// Everything else, including a token of the wrong shape, returns `Ok(None)`.
pub async fn validate_session(
    client: &impl GenericClient,
    token: &str,
) -> Result<Option<AuthUser>, AuthError> {
    if !is_hex(token, SECRET_HEX_LEN) {
        return Ok(None);
    }
    let row = client
        .query_opt(
            concat!(
                "UPDATE fb_session s SET last_seen_at = now() FROM fb_user u \
                 WHERE s.token_hash = ",
                digest!("1"),
                " AND u.id = s.user_id AND u.enabled AND now() < s.expires_at \
                 AND now() < s.last_seen_at + make_interval(secs => $2::float8) \
                 RETURNING u.id, u.username"
            ),
            &[&token, &SESSION_IDLE_SECS],
        )
        .await?;
    let Some(row) = row else {
        client
            .execute(
                concat!(
                    "DELETE FROM fb_session WHERE token_hash = ",
                    digest!("1"),
                    " AND (now() >= expires_at \
                     OR now() >= last_seen_at + make_interval(secs => $2::float8))"
                ),
                &[&token, &SESSION_IDLE_SECS],
            )
            .await?;
        return Ok(None);
    };
    Ok(Some(load_user(client, row.get(0), row.get(1)).await?))
}

/// Whether `csrf_token` is the CSRF token of the session `session_token`, and that session is
/// still valid (same rules as [`validate_session`]; it does not touch the session). The digests
/// are compared by PostgreSQL.
pub async fn check_csrf(
    client: &impl GenericClient,
    session_token: &str,
    csrf_token: &str,
) -> Result<bool, AuthError> {
    if !is_hex(session_token, SECRET_HEX_LEN) || !is_hex(csrf_token, SECRET_HEX_LEN) {
        return Ok(false);
    }
    let row = client
        .query_one(
            concat!(
                "SELECT EXISTS (SELECT 1 FROM fb_session s JOIN fb_user u ON u.id = s.user_id \
                 WHERE s.token_hash = ",
                digest!("1"),
                " AND s.csrf_hash = ",
                digest!("2"),
                " AND u.enabled AND now() < s.expires_at \
                 AND now() < s.last_seen_at + make_interval(secs => $3::float8))"
            ),
            &[&session_token, &csrf_token, &SESSION_IDLE_SECS],
        )
        .await?;
    Ok(row.get(0))
}

/// Deletes a session. Returns `false` if there was none for this token.
pub async fn logout(client: &impl GenericClient, token: &str) -> Result<bool, AuthError> {
    if !is_hex(token, SECRET_HEX_LEN) {
        return Ok(false);
    }
    let deleted = client
        .execute(
            concat!("DELETE FROM fb_session WHERE token_hash = ", digest!("1")),
            &[&token],
        )
        .await?;
    Ok(deleted > 0)
}

// ---------------------------------------------------------------------------------------------
// API tokens
// ---------------------------------------------------------------------------------------------

/// Creates an API token for a user. The secret is returned here only; the database keeps its
/// SHA-256 digest. A token does not expire; revoke it with [`revoke_api_token`].
pub async fn create_api_token(
    client: &impl GenericClient,
    username: &str,
) -> Result<ApiTokenSecret, AuthError> {
    check_username(username)?;
    let key = random_hex(16)?;
    let secret = random_hex(32)?;
    let inserted = client
        .execute(
            concat!(
                "INSERT INTO fb_api_token (key, secret_hash, user_id) \
                 SELECT $1, ",
                digest!("2"),
                ", id FROM fb_user WHERE username = $3"
            ),
            &[&key, &secret, &username],
        )
        .await?;
    if inserted == 0 {
        return Err(AuthError::UserNotFound(username.to_owned()));
    }
    Ok(ApiTokenSecret { key, secret })
}

/// Looks up the user of an API token: the key must exist, the secret must match, the token must
/// not be revoked and the user must be enabled. Otherwise `Ok(None)`.
pub async fn validate_api_token(
    client: &impl GenericClient,
    key: &str,
    secret: &str,
) -> Result<Option<AuthUser>, AuthError> {
    if !is_hex(key, KEY_HEX_LEN) || !is_hex(secret, SECRET_HEX_LEN) {
        return Ok(None);
    }
    let row = client
        .query_opt(
            concat!(
                "SELECT u.id, u.username FROM fb_api_token t JOIN fb_user u ON u.id = t.user_id \
                 WHERE t.key = $1 AND t.secret_hash = ",
                digest!("2"),
                " AND t.revoked_at IS NULL AND u.enabled"
            ),
            &[&key, &secret],
        )
        .await?;
    match row {
        Some(row) => Ok(Some(load_user(client, row.get(0), row.get(1)).await?)),
        None => Ok(None),
    }
}

/// Revokes an API token. Revoking a revoked token succeeds; an unknown key is
/// [`AuthError::TokenNotFound`].
pub async fn revoke_api_token(client: &impl GenericClient, key: &str) -> Result<(), AuthError> {
    let updated = client
        .execute(
            "UPDATE fb_api_token SET revoked_at = coalesce(revoked_at, now()) WHERE key = $1",
            &[&key],
        )
        .await?;
    if updated == 0 {
        return Err(AuthError::TokenNotFound(key.to_owned()));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn username_rules() {
        assert!(check_username("alice").is_ok());
        assert!(check_username("a.b_c@d-e1").is_ok());
        assert!(check_username(&"a".repeat(140)).is_ok());
        assert!(check_username("").is_err());
        assert!(check_username(&"a".repeat(141)).is_err());
        for bad in ["a b", "a/b", "é", "a\nb", "a;b"] {
            assert!(check_username(bad).is_err(), "{bad:?}");
        }
    }

    #[test]
    fn role_rules() {
        assert!(check_role("Sales Manager").is_ok());
        assert!(check_role("Verkäufer").is_ok());
        assert!(check_role(&"r".repeat(140)).is_ok());
        for bad in ["", "   ", "a\tb", "a\0b"] {
            assert!(check_role(bad).is_err(), "{bad:?}");
        }
        assert!(check_role(&"r".repeat(141)).is_err());
    }

    #[test]
    fn password_rules() {
        assert!(check_password(&"p".repeat(8)).is_ok());
        assert!(check_password(&"p".repeat(1024)).is_ok());
        assert!(check_password(&"p".repeat(7)).is_err());
        assert!(check_password(&"p".repeat(1025)).is_err());
    }

    #[test]
    fn random_hex_is_lowercase_hex_of_the_right_length() {
        let a = random_hex(32).unwrap();
        let b = random_hex(32).unwrap();
        assert!(is_hex(&a, 64));
        assert_ne!(a, b);
        assert!(!is_hex(&a.to_uppercase(), 64));
        assert!(!is_hex("abc", 64));
    }

    #[test]
    fn hashes_verify_and_use_the_documented_parameters() {
        let hash = hash_blocking(b"correct horse").unwrap();
        assert!(
            hash.starts_with("$argon2id$v=19$m=19456,t=2,p=1$"),
            "{hash}"
        );
        let argon2 = argon2().unwrap();
        assert!(
            argon2
                .verify_password(b"correct horse", hash.as_str())
                .is_ok()
        );
        assert!(
            argon2
                .verify_password(b"wrong horse", hash.as_str())
                .is_err()
        );
        assert_ne!(hash, hash_blocking(b"correct horse").unwrap());
    }

    #[test]
    fn debug_output_redacts_secrets() {
        let session = LoginSession {
            token: "tok-secret".to_owned(),
            csrf_token: "csrf-secret".to_owned(),
            user: AuthUser {
                id: Uuid::nil(),
                username: "alice".to_owned(),
                roles: vec![],
            },
            expires_at: OffsetDateTime::UNIX_EPOCH,
        };
        let text = format!("{session:?}");
        assert!(
            !text.contains("tok-secret") && !text.contains("csrf-secret"),
            "{text}"
        );
        let token = ApiTokenSecret {
            key: "the-key".to_owned(),
            secret: "api-secret".to_owned(),
        };
        let text = format!("{token:?}");
        assert!(
            text.contains("the-key") && !text.contains("api-secret"),
            "{text}"
        );
    }
}
