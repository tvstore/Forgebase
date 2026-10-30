//! Creating a database and reading which apps it records (M7.2, ADR 0014).

use tokio_postgres::NoTls;
use tokio_postgres::error::SqlState;

use crate::{Client, DbError, quote_ident};

/// The database `create_database` connects to when none is named.
pub const DEFAULT_MAINTENANCE_DB: &str = "postgres";

/// The longest database name PostgreSQL accepts, in bytes.
const MAX_NAME_BYTES: usize = 63;

/// Creates the database that `database_url` names, connecting to the maintenance database
/// `maintenance_db` (default [`DEFAULT_MAINTENANCE_DB`]) of the same server with the same user
/// and password. Returns `true` when it created the database and `false` when it already
/// existed (nothing is changed then).
///
/// `CREATE DATABASE` cannot run in a transaction, so this uses one plain connection of its own,
/// closed before it returns. The name must be 1 to 63 ASCII letters, digits, `_` or `-`, and is
/// quoted with [`quote_ident`]. The role needs the `CREATEDB` privilege.
///
/// No error contains the URL: an unusable URL is reported without its text.
pub async fn create_database(
    database_url: &str,
    maintenance_db: Option<&str>,
) -> Result<bool, DbError> {
    let config: tokio_postgres::Config = database_url.parse().map_err(|_| {
        DbError::CreateDatabase("the database URL is not a valid PostgreSQL URL".to_owned())
    })?;
    let name = config.get_dbname().ok_or_else(|| {
        DbError::CreateDatabase("the database URL does not name a database".to_owned())
    })?;
    if let Some(problem) = name_problem(name) {
        return Err(DbError::CreateDatabase(format!(
            "the database name {name:?} of the URL {problem}"
        )));
    }
    let mut maintenance = config.clone();
    maintenance.dbname(maintenance_db.unwrap_or(DEFAULT_MAINTENANCE_DB));
    let (client, connection) = maintenance.connect(NoTls).await?;
    let connection = tokio::spawn(connection);
    let created = client
        .batch_execute(&format!("CREATE DATABASE {}", quote_ident(name)))
        .await;
    drop(client);
    // The connection task ends when the client is dropped; its own error (if any) came with the
    // result above.
    let _ = connection.await;
    match created {
        Ok(()) => Ok(true),
        Err(e) if e.code() == Some(&SqlState::DUPLICATE_DATABASE) => Ok(false),
        Err(e) => Err(e.into()),
    }
}

/// Why `name` cannot be created as a site database, or `None` when it can.
fn name_problem(name: &str) -> Option<&'static str> {
    let valid = !name.is_empty()
        && name.len() <= MAX_NAME_BYTES
        && name
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'_' || b == b'-');
    (!valid).then_some("must be 1 to 63 characters: ASCII letters, digits, `_` and `-`")
}

/// The names recorded in `fb_app`: the apps whose migrations ran on this database, in byte
/// order. Empty while the table does not exist (the database is not migrated yet).
pub async fn recorded_apps(client: &Client) -> Result<Vec<String>, DbError> {
    let exists = client
        .query_one("SELECT to_regclass('fb_app') IS NOT NULL", &[])
        .await?
        .try_get::<_, bool>(0)?;
    if !exists {
        return Ok(Vec::new());
    }
    let rows = client
        .query("SELECT name FROM fb_app ORDER BY name COLLATE \"C\"", &[])
        .await?;
    rows.iter()
        .map(|row| row.try_get(0).map_err(DbError::from))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::name_problem;

    #[test]
    fn database_names_are_checked() {
        for good in ["a", "forgebase-test", "Acme_2", &"x".repeat(63)] {
            assert_eq!(name_problem(good), None, "{good}");
        }
        for bad in ["", "a b", "a;b", "a\"b", "é", &"x".repeat(64)] {
            assert!(name_problem(bad).is_some(), "{bad:?}");
        }
    }
}
