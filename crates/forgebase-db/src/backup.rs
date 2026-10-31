//! What `site backup` and `site restore` need from the database (M7.3, ADR 0014): the
//! connection of a site as `PG*` environment variables for `pg_dump` and `pg_restore`, and a
//! look at the server (its version, and whether the database is empty).
//!
//! The tools are started by the `forgebase` crate. Nothing here puts a URL, a password or any
//! other secret into a message: an error names an option, never its value.

use tokio_postgres::Config;
use tokio_postgres::config::{
    ChannelBinding, Host, LoadBalanceHosts, SslNegotiation, TargetSessionAttrs,
};

use crate::database::name_problem;
use crate::{DbError, SimpleQueryMessage};

/// The `PG*` variables that give `pg_dump` or `pg_restore` the connection of `database_url`:
/// `PGHOST`, `PGPORT`, `PGUSER`, `PGPASSWORD`, `PGDATABASE`, `PGSSLMODE` and, with TLS,
/// `PGSSLROOTCERT`. A part the URL does not have is left out. This keeps the URL and the
/// password off the command line, where `ps` shows them.
///
/// TLS follows ADR 0018: no `sslmode` or `disable` gives `PGSSLMODE=disable`; `require`,
/// `verify-ca` and `verify-full` give `PGSSLMODE=verify-full` (the verification this crate's own
/// connections do) and `PGSSLROOTCERT` is the `sslrootcert` path, or `system` (PostgreSQL 16 or
/// later); `prefer` and other values are errors.
///
/// A URL option that these variables cannot express (more than one host, `options`,
/// `application_name`, `connect_timeout`, the keepalive and target-session settings, ...) is an
/// error that names the option; the database name must be usable as a site database name (the
/// rule of `create_database`), which also means it can never be read as a connection string.
/// No error contains a value of the URL.
pub fn pg_environment(database_url: &str) -> Result<Vec<(&'static str, String)>, DbError> {
    let (rest, ssl) = crate::tls::parse(database_url).map_err(|e| {
        e.into_db_error(|| {
            DbError::Backup("the database URL is not a valid PostgreSQL URL".to_owned())
        })
    })?;
    let config: Config = rest.parse().map_err(|_| {
        DbError::Backup("the database URL is not a valid PostgreSQL URL".to_owned())
    })?;
    if let Some(option) = unsupported_option(&config) {
        return Err(DbError::Backup(format!(
            "the database URL uses the option `{option}`, which cannot be passed to pg_dump or \
             pg_restore through the PG* environment variables; remove it from the URL"
        )));
    }
    let mut env = Vec::new();
    match config.get_hosts() {
        [] => {}
        [Host::Tcp(host)] => env.push(("PGHOST", host.clone())),
        #[cfg(unix)]
        [Host::Unix(path)] => env.push(("PGHOST", path.to_string_lossy().into_owned())),
        _ => unreachable!("`unsupported_option` refused more than one host"),
    }
    if let [port] = config.get_ports() {
        env.push(("PGPORT", port.to_string()));
    }
    if let Some(user) = config.get_user() {
        env.push(("PGUSER", user.to_owned()));
    }
    if let Some(password) = config.get_password() {
        let password = String::from_utf8(password.to_vec()).map_err(|_| {
            DbError::Backup("the password of the database URL is not valid UTF-8".to_owned())
        })?;
        env.push(("PGPASSWORD", password));
    }
    let name = config
        .get_dbname()
        .ok_or_else(|| DbError::Backup("the database URL does not name a database".to_owned()))?;
    if let Some(problem) = name_problem(name) {
        return Err(DbError::Backup(format!(
            "the database name {name:?} of the URL {problem}"
        )));
    }
    env.push(("PGDATABASE", name.to_owned()));
    env.extend(crate::tls::pg_ssl_environment(&ssl));
    Ok(env)
}

/// The name of the first URL option that is not one of host, port, user, password, database and
/// the TLS parameters (`sslmode`, `sslrootcert`; they are handled apart), if the URL sets one (it differs from the default of an empty configuration).
fn unsupported_option(config: &Config) -> Option<&'static str> {
    let default = Config::new();
    let checks: [(&'static str, bool); 15] = [
        ("host (more than one)", config.get_hosts().len() > 1),
        ("port (more than one)", config.get_ports().len() > 1),
        ("hostaddr", !config.get_hostaddrs().is_empty()),
        ("options", config.get_options().is_some()),
        ("application_name", config.get_application_name().is_some()),
        (
            "sslnegotiation",
            config.get_ssl_negotiation() != SslNegotiation::Postgres,
        ),
        (
            "connect_timeout",
            config.get_connect_timeout() != default.get_connect_timeout(),
        ),
        (
            "tcp_user_timeout",
            config.get_tcp_user_timeout() != default.get_tcp_user_timeout(),
        ),
        (
            "keepalives",
            config.get_keepalives() != default.get_keepalives(),
        ),
        (
            "keepalives_idle",
            config.get_keepalives_idle() != default.get_keepalives_idle(),
        ),
        (
            "keepalives_interval",
            config.get_keepalives_interval() != default.get_keepalives_interval(),
        ),
        (
            "keepalives_retries",
            config.get_keepalives_retries() != default.get_keepalives_retries(),
        ),
        (
            "target_session_attrs",
            config.get_target_session_attrs() != TargetSessionAttrs::Any,
        ),
        (
            "channel_binding",
            config.get_channel_binding() != ChannelBinding::Prefer,
        ),
        (
            "load_balance_hosts",
            config.get_load_balance_hosts() != LoadBalanceHosts::Disable,
        ),
    ];
    checks
        .into_iter()
        .find_map(|(name, set)| set.then_some(name))
}

/// What `site backup` and `site restore` need to know about the server and the database.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DatabaseInfo {
    /// `server_version_num`, for example `160004`.
    pub server_version_num: i32,
    /// No relation, extra schema, extension, function or type in the database (see
    /// [`EMPTY_CHECK_SQL`]).
    pub is_empty: bool,
}

impl DatabaseInfo {
    /// The major version of the server: 16 for `160004`.
    pub fn server_major(&self) -> u32 {
        u32::try_from(self.server_version_num / 10_000).unwrap_or(0)
    }
}

/// The SQL of the emptiness check, one row with one boolean. Conservative: the database is empty
/// only when it has
///
/// - no relation (table, index, sequence, view, composite type, ...) in any schema except
///   `pg_catalog`, `information_schema` and the `pg_toast` and `pg_temp` schemas;
/// - no schema other than `public` (and those);
/// - no extension other than `plpgsql`;
/// - no function, and no domain, enum, range or multirange type, outside those schemas.
///
/// Objects of other kinds (for example a publication or an event trigger) are not checked.
/// Anything the check finds makes the database "not empty", so a restore never merges into
/// existing data of the kinds checked.
pub const EMPTY_CHECK_SQL: &str = "\
WITH user_schemas AS (
    SELECT oid, nspname FROM pg_namespace
    WHERE nspname NOT IN ('pg_catalog', 'information_schema', 'pg_toast')
      AND nspname NOT LIKE 'pg\\_temp\\_%' AND nspname NOT LIKE 'pg\\_toast\\_temp\\_%'
)
SELECT NOT EXISTS (SELECT 1 FROM pg_class c JOIN user_schemas s ON s.oid = c.relnamespace)
   AND NOT EXISTS (SELECT 1 FROM user_schemas WHERE nspname <> 'public')
   AND NOT EXISTS (SELECT 1 FROM pg_extension WHERE extname <> 'plpgsql')
   AND NOT EXISTS (SELECT 1 FROM pg_proc p JOIN user_schemas s ON s.oid = p.pronamespace)
   AND NOT EXISTS (SELECT 1 FROM pg_type t JOIN user_schemas s ON s.oid = t.typnamespace
                   WHERE t.typtype IN ('d', 'e', 'r', 'm'))";

/// Connects to the database of `database_url` once (no pool; TLS as the URL says) and reads its
/// [`DatabaseInfo`]. The error is the server's or the connection's, never the URL.
pub async fn inspect_database(database_url: &str) -> Result<DatabaseInfo, DbError> {
    let (config, tls) = crate::tls::prepare(database_url).map_err(|e| {
        e.into_db_error(|| {
            DbError::Backup("the database URL is not a valid PostgreSQL URL".to_owned())
        })
    })?;
    let (client, connection) = config.connect(tls).await?;
    let connection = tokio::spawn(connection);
    let info = async {
        let version = single_value(&client, "SHOW server_version_num").await?;
        let empty = single_value(&client, EMPTY_CHECK_SQL).await?;
        Ok::<_, DbError>(DatabaseInfo {
            server_version_num: version.parse().map_err(|_| {
                DbError::Backup("the server reports no usable version number".to_owned())
            })?,
            is_empty: empty == "t",
        })
    }
    .await;
    drop(client);
    let _ = connection.await;
    info
}

/// The first column of the first row of a simple query, as text.
async fn single_value(client: &tokio_postgres::Client, sql: &str) -> Result<String, DbError> {
    let messages = client.simple_query(sql).await?;
    messages
        .iter()
        .find_map(|message| match message {
            SimpleQueryMessage::Row(row) => row.get(0).map(str::to_owned),
            _ => None,
        })
        .ok_or_else(|| DbError::Backup("the server answered with no value".to_owned()))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn env_of(url: &str) -> Vec<(&'static str, String)> {
        pg_environment(url).expect("a usable URL")
    }

    #[test]
    fn the_url_becomes_pg_variables() {
        let env = env_of("postgres://ann:s3cret%20pw@db.example.com:6543/shop_1");
        assert_eq!(
            env,
            [
                ("PGHOST", "db.example.com".to_owned()),
                ("PGPORT", "6543".to_owned()),
                ("PGUSER", "ann".to_owned()),
                ("PGPASSWORD", "s3cret pw".to_owned()),
                ("PGDATABASE", "shop_1".to_owned()),
                ("PGSSLMODE", "disable".to_owned()),
            ]
        );
        // Parts the URL lacks are left out; TLS is off unless the URL asks for it (ADR 0018).
        assert_eq!(
            env_of("postgres:///shop?sslmode=disable"),
            [
                ("PGDATABASE", "shop".to_owned()),
                ("PGSSLMODE", "disable".to_owned())
            ]
        );
        assert!(env_of("postgres://h/shop").contains(&("PGSSLMODE", "disable".to_owned())));
        // `require` and the stricter modes verify the host name, so pg_dump gets `verify-full`.
        for mode in ["require", "verify-ca", "verify-full"] {
            let env = env_of(&format!("postgres://h/shop?sslmode={mode}"));
            assert!(
                env.contains(&("PGSSLMODE", "verify-full".to_owned())),
                "{mode}"
            );
            assert!(
                env.contains(&("PGSSLROOTCERT", "system".to_owned())),
                "{mode}"
            );
        }
        let env = env_of("postgres://h/shop?sslmode=require&sslrootcert=/etc/ca%20x.pem");
        assert!(env.contains(&("PGSSLROOTCERT", "/etc/ca x.pem".to_owned())));
        // `prefer` is refused, with a message that names neither the URL nor the password.
        let message = pg_environment("postgres://u:pw@h/shop?sslmode=prefer")
            .expect_err("prefer")
            .to_string();
        assert!(message.contains("`sslmode=prefer` is not supported") && !message.contains("pw"));
    }

    #[test]
    fn tls_is_found_behind_a_raw_question_mark_and_an_encoded_key() {
        for url in [
            "postgres://u:a?b@h/shop?sslmode=require",
            "postgres://u:x?sslmode=disable@h/shop?sslmode=require",
            "postgres://u@h/shop?%73slmode=require",
        ] {
            let env = env_of(url);
            assert!(
                env.contains(&("PGSSLMODE", "verify-full".to_owned())),
                "{url}"
            );
        }
        let env = env_of("postgres://u:a?b@h/shop?sslmode=require");
        assert!(env.contains(&("PGPASSWORD", "a?b".to_owned())));
        let message = pg_environment("postgres://u:pw@h/shop?%73slcert=x")
            .expect_err("sslcert")
            .to_string();
        assert!(
            message.contains("`sslcert`") && !message.contains("pw"),
            "{message}"
        );
    }

    #[test]
    fn unsupported_options_are_errors_that_name_the_option_and_not_the_value() {
        for (url, option) in [
            ("postgres://u:pw@h1,h2/db", "host (more than one)"),
            ("postgres://u:pw@h/db?options=-c%20xyzzy%3D1", "options"),
            (
                "postgres://u:pw@h/db?application_name=xyzzy",
                "application_name",
            ),
            ("postgres://u:pw@h/db?connect_timeout=77", "connect_timeout"),
            ("postgres://u:pw@h/db?keepalives=0", "keepalives"),
            (
                "postgres://u:pw@h/db?target_session_attrs=read-write",
                "target_session_attrs",
            ),
        ] {
            let message = pg_environment(url).expect_err(url).to_string();
            assert!(message.contains(&format!("`{option}`")), "{url}: {message}");
            for secret in ["xyzzy", "77", "pw", "read-write"] {
                assert!(!message.contains(secret), "{url}: {message}");
            }
        }
    }

    #[test]
    fn a_bad_url_or_database_name_is_an_error_without_the_text() {
        for url in [
            "not a url ::",
            "postgres://u:pw@h/",
            "postgres://u:pw@h/a%20b",
            "postgres://u:pw@h/host%3Dxyzzy",
        ] {
            let message = pg_environment(url).expect_err(url).to_string();
            assert!(
                !message.contains("pw") && !message.contains("not a url"),
                "{message}"
            );
        }
    }

    #[test]
    fn the_major_version_comes_from_the_version_number() {
        let major = |n| {
            DatabaseInfo {
                server_version_num: n,
                is_empty: true,
            }
            .server_major()
        };
        assert_eq!(major(160_004), 16);
        assert_eq!(major(170_000), 17);
        assert_eq!(major(0), 0);
    }
}
