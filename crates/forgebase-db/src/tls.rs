//! TLS to PostgreSQL (M9.0, ADR 0018): the `sslmode` and `sslrootcert` parameters of the
//! database URL, and the one rustls connector that every connection of this crate uses.
//!
//! `tokio-postgres` knows `disable`, `prefer` and `require` and never verifies a certificate, so
//! the two TLS parameters are taken out of the URL here, before it reaches
//! `tokio_postgres::Config`:
//!
//! | `sslmode` | behaviour |
//! |---|---|
//! | absent, `disable` | no TLS |
//! | `require`, `verify-ca`, `verify-full` | TLS, always verifying the chain **and** the host name |
//! | `prefer`, `allow`, anything else | an error: there is never a silent downgrade |
//!
//! The roots are the operating system's trust store, or only the certificates of the PEM file
//! `sslrootcert=<path>`; `sslrootcert=system` means the operating system's store. Client
//! certificates (`sslcert`, `sslkey`) and the other `ssl*` parameters are errors, not ignored.
//! No error contains the URL or the password.

use std::sync::Arc;

use rustls::RootCertStore;
use rustls::pki_types::CertificateDer;
use rustls::pki_types::pem::PemObject;
use tokio_postgres::Config;
use tokio_postgres::config::{Host, SslMode};
use tokio_postgres_rustls::MakeRustlsConnect;

use crate::DbError;

/// Where the trusted root certificates come from.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RootCerts {
    /// The operating system's trust store (the default, and `sslrootcert=system`).
    System,
    /// Only the certificates of this PEM file (`sslrootcert=<path>`).
    File(String),
}

/// What the URL asks for.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Ssl {
    /// No TLS (`sslmode` absent or `disable`).
    Disable,
    /// TLS with full verification (`sslmode` `require`, `verify-ca` or `verify-full`).
    Verify(RootCerts),
}

/// Why a URL cannot be used. Kept apart from [`DbError`] so that each caller keeps its own error
/// for a URL that does not parse at all (`connect`: [`DbError::CreatePool`]; `create_database`
/// and the backup functions: their own variant, without the text of the URL).
#[derive(Debug)]
pub(crate) enum TlsError {
    /// Not a valid PostgreSQL URL (the parse error of `tokio-postgres`).
    InvalidUrl(tokio_postgres::Error),
    /// A TLS setting that is refused or cannot be used. Never contains the URL.
    Refused(String),
}

impl std::fmt::Display for TlsError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            TlsError::InvalidUrl(_) => {
                f.write_str("the database URL is not a valid PostgreSQL URL")
            }
            TlsError::Refused(message) => f.write_str(message),
        }
    }
}

impl From<TlsError> for DbError {
    /// For `connect`: an unparseable URL is a pool configuration error, as before M9.0.
    fn from(error: TlsError) -> Self {
        match error {
            TlsError::InvalidUrl(e) => {
                DbError::CreatePool(deadpool_postgres::CreatePoolError::Config(
                    deadpool_postgres::ConfigError::InvalidUrl(e),
                ))
            }
            TlsError::Refused(message) => DbError::Tls(message),
        }
    }
}

impl TlsError {
    /// For the callers that never show the URL: the message, with `invalid` for a bad URL.
    pub(crate) fn into_db_error(self, invalid: impl FnOnce() -> DbError) -> DbError {
        match self {
            TlsError::InvalidUrl(_) => invalid(),
            TlsError::Refused(message) => DbError::Tls(message),
        }
    }
}

fn err(message: impl Into<String>) -> TlsError {
    TlsError::Refused(message.into())
}

/// Splits the TLS parameters off `url`: the URL without them (for `tokio_postgres::Config`) and
/// what they ask for. The URL must be a `postgres://` or `postgresql://` URL.
pub(crate) fn parse(url: &str) -> Result<(String, Ssl), TlsError> {
    let lower = url.get(..13).unwrap_or(url).to_ascii_lowercase();
    if !(lower.starts_with("postgres://") || lower.starts_with("postgresql://")) {
        // Anything that is not a URL: if it parses (the `key=value` form), it is refused because
        // its `sslmode` could not be checked here; if it does not, it is simply an invalid URL.
        return Err(match url.parse::<Config>() {
            Ok(_) => err("the database URL must start with postgres:// or postgresql://"),
            Err(e) => TlsError::InvalidUrl(e),
        });
    }
    let (base, query) = match url.split_once('?') {
        Some((base, query)) => (base, Some(query)),
        None => (url, None),
    };
    let mut mode: Option<String> = None;
    let mut root: Option<String> = None;
    let mut kept = Vec::new();
    for param in query.into_iter().flat_map(|q| q.split('&')) {
        if param.is_empty() {
            continue;
        }
        let (key, value) = param.split_once('=').unwrap_or((param, ""));
        match key {
            "sslmode" => {
                if mode.replace(percent_decode(value)?).is_some() {
                    return Err(err("the database URL sets `sslmode` more than once"));
                }
            }
            "sslrootcert" => {
                if root.replace(percent_decode(value)?).is_some() {
                    return Err(err("the database URL sets `sslrootcert` more than once"));
                }
            }
            // `sslnegotiation` is a plain `tokio-postgres` option.
            "sslnegotiation" => kept.push(param),
            k if k.starts_with("ssl") || k == "requiressl" => {
                return Err(err(format!(
                    "the database URL uses the TLS option `{k}`, which is not supported; \
                     supported are `sslmode` and `sslrootcert`"
                )));
            }
            _ => kept.push(param),
        }
    }
    let ssl = match mode.as_deref() {
        None | Some("disable") => {
            if root.is_some() {
                return Err(err("the database URL has `sslrootcert` but TLS is off; \
                     set `sslmode=require` or remove `sslrootcert`"));
            }
            Ssl::Disable
        }
        Some("require" | "verify-ca" | "verify-full") => Ssl::Verify(match root.as_deref() {
            None | Some("system") => RootCerts::System,
            Some("") => return Err(err("the `sslrootcert` of the database URL is empty")),
            Some(path) => RootCerts::File(path.to_owned()),
        }),
        Some("prefer" | "allow") => {
            return Err(err(
                "`sslmode=prefer` is not supported: use `disable` or `require` \
                 (`allow` is not supported either)",
            ));
        }
        Some(_) => {
            return Err(err(
                "the `sslmode` of the database URL is not supported: use `disable`, `require`, \
                 `verify-ca` or `verify-full`",
            ));
        }
    };
    let mut rest = base.to_owned();
    if !kept.is_empty() {
        rest.push('?');
        rest.push_str(&kept.join("&"));
    }
    Ok((rest, ssl))
}

fn percent_decode(value: &str) -> Result<String, TlsError> {
    let bad = || err("a TLS parameter of the database URL is not valid percent-encoded UTF-8");
    let bytes = value.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%' {
            let hex = value.get(i + 1..i + 3).ok_or_else(bad)?;
            out.push(u8::from_str_radix(hex, 16).map_err(|_| bad())?);
            i += 3;
        } else {
            out.push(bytes[i]);
            i += 1;
        }
    }
    String::from_utf8(out).map_err(|_| bad())
}

/// The connection settings of `url` and the rustls connector to connect with. With no TLS the
/// connector is never used (`ssl_mode` is `Disable`, and `tokio-postgres` returns before it
/// touches the connector), so every connection has one concrete TLS type.
pub(crate) fn prepare(url: &str) -> Result<(Config, MakeRustlsConnect), TlsError> {
    let (rest, ssl) = parse(url)?;
    let mut config: Config = rest.parse().map_err(TlsError::InvalidUrl)?;
    let roots = match &ssl {
        Ssl::Disable => {
            config.ssl_mode(SslMode::Disable);
            RootCertStore::empty()
        }
        Ssl::Verify(source) => {
            config.ssl_mode(SslMode::Require);
            if config.get_hosts().is_empty() {
                return Err(err("TLS needs a host name in the database URL"));
            }
            #[cfg(unix)]
            if config
                .get_hosts()
                .iter()
                .any(|h| matches!(h, Host::Unix(_)))
            {
                return Err(err("TLS is not supported over a Unix socket host"));
            }
            load_roots(source)?
        }
    };
    let provider = Arc::new(rustls::crypto::ring::default_provider());
    let client = rustls::ClientConfig::builder_with_provider(provider)
        .with_safe_default_protocol_versions()
        .map_err(|e| err(format!("cannot set up TLS: {e}")))?
        .with_root_certificates(roots)
        .with_no_client_auth();
    Ok((config, MakeRustlsConnect::new(client)))
}

fn load_roots(source: &RootCerts) -> Result<RootCertStore, TlsError> {
    let mut store = RootCertStore::empty();
    match source {
        RootCerts::System => {
            let loaded = rustls_native_certs::load_native_certs();
            let (added, _ignored) = store.add_parsable_certificates(loaded.certs);
            if added == 0 {
                let cause = loaded
                    .errors
                    .first()
                    .map_or_else(|| "it is empty".to_owned(), ToString::to_string);
                return Err(err(format!(
                    "no root certificate in the operating system's trust store ({cause}); \
                     install the CA certificates (for example the `ca-certificates` package) \
                     or set `sslrootcert=<path to a PEM file>`"
                )));
            }
        }
        RootCerts::File(path) => {
            let certs = CertificateDer::pem_file_iter(path)
                .and_then(|iter| iter.collect::<Result<Vec<_>, _>>())
                .map_err(|e| err(format!("cannot read the `sslrootcert` file {path:?}: {e}")))?;
            let (added, _ignored) = store.add_parsable_certificates(certs);
            if added == 0 {
                return Err(err(format!(
                    "the `sslrootcert` file {path:?} has no usable PEM certificate"
                )));
            }
        }
    }
    Ok(store)
}

/// The `PGSSLMODE` and `PGSSLROOTCERT` values that give `pg_dump` and `pg_restore` the same
/// TLS behaviour as this crate's own connections: `disable`, or `verify-full` with the roots
/// (a path, or `system`, which needs PostgreSQL 16 or later).
pub(crate) fn pg_ssl_environment(ssl: &Ssl) -> Vec<(&'static str, String)> {
    match ssl {
        Ssl::Disable => vec![("PGSSLMODE", "disable".to_owned())],
        Ssl::Verify(roots) => vec![
            ("PGSSLMODE", "verify-full".to_owned()),
            (
                "PGSSLROOTCERT",
                match roots {
                    RootCerts::System => "system".to_owned(),
                    RootCerts::File(path) => path.clone(),
                },
            ),
        ],
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parsed(url: &str) -> (String, Ssl) {
        parse(url).unwrap_or_else(|e| panic!("{url}: {e}"))
    }

    fn refused(url: &str) -> String {
        parse(url).expect_err(url).to_string()
    }

    #[test]
    fn no_sslmode_and_disable_mean_no_tls() {
        assert_eq!(
            parsed("postgres://u:p@h/db"),
            ("postgres://u:p@h/db".to_owned(), Ssl::Disable)
        );
        assert_eq!(
            parsed("postgresql://u@h/db?sslmode=disable").1,
            Ssl::Disable
        );
    }

    #[test]
    fn require_and_the_verify_modes_mean_full_verification() {
        for mode in ["require", "verify-ca", "verify-full"] {
            assert_eq!(
                parsed(&format!("postgres://u@h/db?sslmode={mode}")).1,
                Ssl::Verify(RootCerts::System),
                "{mode}"
            );
        }
    }

    #[test]
    fn sslrootcert_is_a_path_or_system() {
        assert_eq!(
            parsed("postgres://u@h/db?sslmode=require&sslrootcert=/etc/ca%20x.pem").1,
            Ssl::Verify(RootCerts::File("/etc/ca x.pem".to_owned()))
        );
        assert_eq!(
            parsed("postgres://u@h/db?sslmode=require&sslrootcert=system").1,
            Ssl::Verify(RootCerts::System)
        );
        assert!(refused("postgres://u@h/db?sslrootcert=/x.pem").contains("TLS is off"));
        assert!(refused("postgres://u@h/db?sslmode=require&sslrootcert=").contains("empty"));
    }

    #[test]
    fn other_query_parameters_are_kept_in_order() {
        let (rest, ssl) = parsed(
            "postgres://u@h/db?connect_timeout=5&sslmode=require&sslrootcert=system&application_name=x",
        );
        assert_eq!(
            rest,
            "postgres://u@h/db?connect_timeout=5&application_name=x"
        );
        assert_eq!(ssl, Ssl::Verify(RootCerts::System));
        assert_eq!(
            parsed("postgres://u@h/db?sslmode=require").0,
            "postgres://u@h/db"
        );
        assert_eq!(
            parsed("postgres://u@h/db?sslnegotiation=direct&sslmode=require").0,
            "postgres://u@h/db?sslnegotiation=direct"
        );
    }

    #[test]
    fn prefer_allow_and_unknown_modes_are_refused() {
        for mode in ["prefer", "allow"] {
            let message = refused(&format!("postgres://u@h/db?sslmode={mode}"));
            assert!(
                message.contains("`sslmode=prefer` is not supported: use `disable` or `require`"),
                "{message}"
            );
        }
        assert!(refused("postgres://u@h/db?sslmode=bogus").contains("not supported"));
        assert!(refused("postgres://u@h/db?sslmode=").contains("not supported"));
    }

    #[test]
    fn client_certificates_and_other_ssl_options_are_refused() {
        for key in ["sslcert", "sslkey", "sslcrl", "sslpassword", "requiressl"] {
            let message = refused(&format!("postgres://u@h/db?sslmode=require&{key}=x"));
            assert!(message.contains(&format!("`{key}`")), "{message}");
        }
    }

    #[test]
    fn repeated_parameters_and_other_url_forms_are_refused() {
        assert!(
            refused("postgres://u@h/db?sslmode=require&sslmode=disable").contains("more than once")
        );
        assert!(refused("host=h dbname=db sslmode=require").contains("must start with"));
    }

    #[test]
    fn errors_do_not_contain_the_url_or_the_password() {
        for url in [
            "postgres://u:hunter2@h/db?sslmode=prefer",
            "postgres://u:hunter2@h/db?sslmode=bogus",
            "postgres://u:hunter2@h/db?sslcert=x",
            "hunter2",
        ] {
            assert!(!refused(url).contains("hunter2"), "{url}");
        }
    }

    #[test]
    fn prepare_sets_the_ssl_mode_and_checks_the_hosts() {
        let (config, _) = prepare("postgres://u@localhost/db").expect("plain");
        assert_eq!(config.get_ssl_mode(), SslMode::Disable);
        // An explicit file that is missing is an error that names the file, not the URL.
        let message = prepare(
            "postgres://u:hunter2@localhost/db?sslmode=require&sslrootcert=/nonexistent/ca.pem",
        )
        .err()
        .expect("missing file")
        .to_string();
        assert!(message.contains("/nonexistent/ca.pem") && !message.contains("hunter2"));
        #[cfg(unix)]
        {
            let message = prepare(
                "postgres://u@%2Fvar%2Frun%2Fpostgresql/db?sslmode=require&sslrootcert=system",
            )
            .err()
            .expect("unix host")
            .to_string();
            assert!(message.contains("Unix socket"), "{message}");
        }
    }

    #[test]
    fn the_pg_environment_matches_the_verification() {
        assert_eq!(
            pg_ssl_environment(&Ssl::Disable),
            [("PGSSLMODE", "disable".to_owned())]
        );
        assert_eq!(
            pg_ssl_environment(&Ssl::Verify(RootCerts::System)),
            [
                ("PGSSLMODE", "verify-full".to_owned()),
                ("PGSSLROOTCERT", "system".to_owned())
            ]
        );
        assert_eq!(
            pg_ssl_environment(&Ssl::Verify(RootCerts::File("/ca.pem".to_owned()))),
            [
                ("PGSSLMODE", "verify-full".to_owned()),
                ("PGSSLROOTCERT", "/ca.pem".to_owned())
            ]
        );
    }
}
