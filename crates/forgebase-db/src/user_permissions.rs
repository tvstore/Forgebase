//! Managing user permissions (M10.3, ADR 0025): add, remove and list the documents a user is
//! restricted to. The rows are in `fb_user_permission` (migration 0013) and are read with the
//! roles of every authenticated request ([`crate::auth::validate_session`]), so a change applies
//! on the user's next request, with no reload. The command line and the Administrator-only API
//! both call these functions.
//!
//! A restriction says: of this DocType, the user sees only these documents (and only documents
//! of other DocTypes whose Links to it are empty or point at them). What it means for every
//! read and write is decided by [`forgebase_core::permission::row_scope`] and applied in SQL.

use forgebase_core::meta::Registry;
use forgebase_core::permission::{Caller, MAX_USER_PERMISSIONS};
use time::OffsetDateTime;
use tokio_postgres::GenericClient;

use crate::auth::{AuthError, check_username, user_id};
use crate::document;

/// One restriction of a user: they may see the document `value` of `doctype` (and, when it is the
/// only one they have for the DocType, nothing else of it).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UserPermission {
    /// The username.
    pub user: String,
    /// The DocType the restriction is on.
    pub doctype: String,
    /// The name of the allowed document.
    pub value: String,
    /// When the row was added.
    pub created_at: OffsetDateTime,
}

fn invalid(message: String) -> AuthError {
    AuthError::InvalidUserPermission(message)
}

/// Restricts `username` to the document `value` of `doctype` (in addition to the ones they
/// have). Returns `false` when the row existed already.
///
/// Checked in this order, with nothing written on failure: the username rule; the DocType must
/// be in `registry` (the site's effective registry) and not a child DocType; the value must not
/// be empty or contain NUL; the user must exist ([`AuthError::UserNotFound`]); the document
/// must exist (looked up as the system context, so a user permission names a document that is
/// there when it is added); a row that exists already then succeeds (`false`); and the user
/// must have fewer than [`MAX_USER_PERMISSIONS`] for the DocType (the names are bound as one
/// array in every read, so the number is kept small). The user row is locked, so two adds cannot
/// both pass the limit.
pub async fn add_user_permission(
    client: &mut impl GenericClient,
    registry: &Registry,
    username: &str,
    doctype: &str,
    value: &str,
) -> Result<bool, AuthError> {
    check_username(username)?;
    let target = registry.get(doctype).ok_or_else(|| {
        invalid(format!(
            "unknown DocType `{}`",
            forgebase_core::shorten(doctype)
        ))
    })?;
    if target.is_child {
        return Err(invalid(format!(
            "`{doctype}` is a child DocType: its rows follow the parent, restrict the parent"
        )));
    }
    if value.is_empty() || value.contains('\0') {
        return Err(invalid(
            "the value must be a document name: not empty, no NUL character".to_owned(),
        ));
    }
    let mut tx = client.transaction().await?;
    // Locks the user for the whole add (and fails for a user that does not exist).
    let found = tx
        .query_opt(
            "SELECT id FROM fb_user WHERE username = $1 FOR NO KEY UPDATE",
            &[&username],
        )
        .await?;
    let Some(found) = found else {
        return Err(AuthError::UserNotFound(username.to_owned()));
    };
    let id: uuid::Uuid = found.try_get(0)?;
    let exists = document::get(&mut tx, registry, doctype, value, &Caller::system("cli")).await?;
    if exists.is_none() {
        return Err(invalid(format!(
            "no `{doctype}` document named `{}`",
            forgebase_core::shorten(value)
        )));
    }
    let present = tx
        .query_opt(
            "SELECT 1 FROM fb_user_permission WHERE user_id = $1 AND doctype = $2 AND value = $3",
            &[&id, &doctype, &value],
        )
        .await?;
    if present.is_some() {
        return Ok(false);
    }
    let count: i64 = tx
        .query_one(
            "SELECT count(*) FROM fb_user_permission WHERE user_id = $1 AND doctype = $2",
            &[&id, &doctype],
        )
        .await?
        .try_get(0)?;
    if usize::try_from(count).unwrap_or(usize::MAX) >= MAX_USER_PERMISSIONS {
        return Err(invalid(format!(
            "user `{username}` is restricted to {MAX_USER_PERMISSIONS} `{doctype}` documents already, the most allowed"
        )));
    }
    tx.execute(
        "INSERT INTO fb_user_permission (user_id, doctype, value) VALUES ($1, $2, $3)",
        &[&id, &doctype, &value],
    )
    .await?;
    tx.commit().await?;
    Ok(true)
}

/// Takes one restriction from a user. Returns `false` when the user did not have it. The DocType
/// and the document are not checked: a row for a DocType or a document that is gone can always
/// be removed. An unknown user is [`AuthError::UserNotFound`].
pub async fn remove_user_permission(
    client: &mut impl GenericClient,
    username: &str,
    doctype: &str,
    value: &str,
) -> Result<bool, AuthError> {
    check_username(username)?;
    let tx = client.transaction().await?;
    let id = user_id(&tx, username).await?;
    let removed = tx
        .execute(
            "DELETE FROM fb_user_permission WHERE user_id = $1 AND doctype = $2 AND value = $3",
            &[&id, &doctype, &value],
        )
        .await?;
    tx.commit().await?;
    Ok(removed > 0)
}

/// The restrictions, sorted by username, DocType and value; of one user and/or one DocType, or
/// all. Rows for a DocType the site no longer has are listed too (they are ignored when reading,
/// and can be removed). An unknown `user` is [`AuthError::UserNotFound`].
pub async fn list_user_permissions(
    client: &impl GenericClient,
    user: Option<&str>,
    doctype: Option<&str>,
) -> Result<Vec<UserPermission>, AuthError> {
    if let Some(user) = user {
        check_username(user)?;
        user_id(client, user).await?;
    }
    let rows = client
        .query(
            "SELECT u.username, p.doctype, p.value, p.created_at \
             FROM fb_user_permission p JOIN fb_user u ON u.id = p.user_id \
             WHERE ($1::text IS NULL OR u.username = $1) AND ($2::text IS NULL OR p.doctype = $2) \
             ORDER BY u.username, p.doctype, p.value",
            &[&user, &doctype],
        )
        .await?;
    rows.iter()
        .map(|row| {
            Ok(UserPermission {
                user: row.try_get(0)?,
                doctype: row.try_get(1)?,
                value: row.try_get(2)?,
                created_at: row.try_get(3)?,
            })
        })
        .collect()
}
