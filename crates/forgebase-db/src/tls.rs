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
    // Find the query exactly where `tokio-postgres` does: the credentials end at the first `@`
    // of the string, the authority and the path end at the first `?` after that. A raw `/`, `?`
    // or `#` before that `@` is refused below, so the credentials never hide the start of the
    // query.
    let scheme_len = if lower.starts_with("postgresql://") {
        13
    } else {
        11
    };
    let creds_end = match url[scheme_len..].find('@') {
        // Userinfo (RFC 3986) cannot hold a raw `/`, `?` or `#`. With one before the first `@`,
        // libpq and `tokio-postgres` can read different hosts and options (for example an `@` in a
        // later query value: libpq sees no credentials, `tokio-postgres` takes everything before
        // the `@` as credentials and loses the `sslmode`), and a TLS choice could go unseen. Refuse
        // it, whatever the URL looks like otherwise.
        Some(at) if url[scheme_len..scheme_len + at].contains(['/', '?', '#']) => {
            return Err(err(
                "the database URL has a `/`, `?` or `#` before its first `@`: percent-encode \
                 them in the user name or password (`%2F`, `%3F`, `%23`), and an `@` in a query \
                 value as `%40`",
            ));
        }
        Some(at) => scheme_len + at + 1,
        None => scheme_len,
    };
    let (base, query) = match url[creds_end..].find('?') {
        Some(q) => (&url[..creds_end + q], Some(&url[creds_end + q + 1..])),
        None => (url, None),
    };
    let mut mode: Option<String> = None;
    let mut root: Option<String> = None;
    let mut kept = Vec::new();
    for param in query.into_iter().flat_map(|q| q.split('&')) {
        if param.is_empty() {
            continue;
        }
        let (raw_key, value) = param.split_once('=').unwrap_or((param, ""));
        // `tokio-postgres` decodes the key before it looks at it: so do we.
        let key = percent_decode(raw_key)?;
        match key.as_str() {
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
            k if k.starts_with("ssl") || k.eq_ignore_ascii_case("requiressl") => {
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
    // Belt and braces: `tokio-postgres` must see the same TLS choice that we found. What is left
    // of the URL must not set an `sslmode` of its own (it would be one that our scan missed), and
    // the whole URL must give the `sslmode` that matches ours. Otherwise a URL that asks for TLS
    // could connect without it.
    if let Ok(left) = rest.parse::<Config>()
        && left.get_ssl_mode() != SslMode::Prefer
    {
        return Err(err(
            "the database URL has an `sslmode` that could not be read safely; \
             check the URL (a `/`, `?`, `#`, `@` or `%` in the password must be percent-encoded)",
        ));
    }
    if let Ok(whole) = url.parse::<Config>() {
        let expected = match (&ssl, &mode) {
            (Ssl::Verify(_), _) => SslMode::Require,
            (Ssl::Disable, Some(_)) => SslMode::Disable,
            (Ssl::Disable, None) => SslMode::Prefer,
        };
        if whole.get_ssl_mode() != expected {
            return Err(err(
                "the database URL has an `sslmode` that could not be read safely; \
                 check the URL (a `/`, `?`, `#`, `@` or `%` in the password must be percent-encoded)",
            ));
        }
    }
    Ok((rest, ssl))
}

fn percent_decode(value: &str) -> Result<String, TlsError> {
    let bad = || err("a query parameter of the database URL is not valid percent-encoding");
    let bytes = value.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%' {
            // Exactly two hex digits (`from_str_radix` would also take a sign).
            let hex = bytes.get(i + 1..i + 3).ok_or_else(bad)?;
            if !hex.iter().all(u8::is_ascii_hexdigit) {
                return Err(bad());
            }
            let text = std::str::from_utf8(hex).map_err(|_| bad())?;
            out.push(u8::from_str_radix(text, 16).map_err(|_| bad())?);
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
            check_tls_host(&config)?;
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

/// TLS needs a host name to verify, and is not offered over a Unix socket. Used for the
/// connections of this crate and for `pg_environment`, so `pg_dump` and `pg_restore` refuse the
/// same URLs.
pub(crate) fn check_tls_host(config: &Config) -> Result<(), TlsError> {
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
    Ok(())
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
    fn special_characters_in_the_credentials_must_be_percent_encoded() {
        // Encoded, they work and stay in the password.
        for (url, rest) in [
            (
                "postgres://u:a%3Fb@h/db?sslmode=require",
                "postgres://u:a%3Fb@h/db",
            ),
            (
                "postgres://u:a%23b@h/db?sslmode=require",
                "postgres://u:a%23b@h/db",
            ),
            (
                "postgres://u:a%2Fb@h/db?sslmode=require",
                "postgres://u:a%2Fb@h/db",
            ),
            (
                "postgresql://u:x%3Fsslmode=disable@h/db?sslmode=require",
                "postgresql://u:x%3Fsslmode=disable@h/db",
            ),
            (
                "postgres://u:a%3Fb@h/db?connect_timeout=5&sslmode=require",
                "postgres://u:a%3Fb@h/db?connect_timeout=5",
            ),
        ] {
            let (kept, ssl) = parsed(url);
            assert_eq!(kept, rest, "{url}");
            assert_eq!(ssl, Ssl::Verify(RootCerts::System), "{url}");
            let config: Config = kept.parse().expect(url);
            assert_eq!(config.get_ssl_mode(), SslMode::Prefer, "{url}");
        }
        assert_eq!(parsed("postgres://u:a%3Fb@h/db").1, Ssl::Disable);
        // Raw, they are refused, with TLS asked for or not.
        for url in [
            "postgres://u:a?b@h/db?sslmode=require",
            "postgres://u:a#b@h/db?sslmode=require",
            "postgres://u:a/b@h/db?sslmode=require",
            "postgres://u:x?sslmode=disable@h/db?sslmode=require",
            "postgres://u:a?b@h/db",
        ] {
            let message = refused(url);
            assert!(message.contains("before its first `@`"), "{url}: {message}");
            assert!(!message.contains("a?b"), "{message}");
            assert!(prepare(url).is_err(), "{url}");
        }
    }

    #[test]
    fn encoded_keys_are_decoded_before_they_are_matched() {
        assert_eq!(
            parsed("postgres://u@h/db?%73slmode=require").1,
            Ssl::Verify(RootCerts::System)
        );
        assert_eq!(
            parsed("postgres://u@h/db?sslmode=require&%73slrootcert=%2Fca.pem").1,
            Ssl::Verify(RootCerts::File("/ca.pem".to_owned()))
        );
        assert!(refused("postgres://u@h/db?%73slcert=x").contains("`sslcert`"));
        assert!(refused("postgres://u@h/db?sslmode=require&%73slkey=x").contains("`sslkey`"));
        assert!(
            refused("postgres://u@h/db?sslmode=disable&%73slmode=require")
                .contains("more than once")
        );
        assert!(refused("postgres://u:a%3Fb@h/db?%73slcert=x").contains("`sslcert`"));
    }

    #[test]
    fn an_at_sign_after_the_host_is_refused() {
        // With a path: the first `@` is in a query value, after a `/`.
        let with_path =
            "postgres://db.example.com/app?sslmode=verify-full&application_name=svc@db.example.com";
        // Without a path: the first `@` follows a `?` directly.
        let without_path =
            "postgres://db.example.com?sslmode=verify-full&application_name=svc@db.example.com";
        for url in [
            with_path,
            without_path,
            "postgres://h/db?sslmode=require&x=a@b",
        ] {
            let message = refused(url);
            assert!(
                message.contains("`@` in a query value as `%40`"),
                "{url}: {message}"
            );
            assert!(prepare(url).is_err(), "{url}");
        }
        // The same URLs with the `@` encoded are fine.
        for url in [
            "postgres://db.example.com/app?sslmode=verify-full&application_name=svc%40db",
            "postgres://db.example.com?sslmode=verify-full&application_name=svc%40db",
        ] {
            assert_eq!(parsed(url).1, Ssl::Verify(RootCerts::System), "{url}");
        }
    }

    #[test]
    fn percent_encoding_needs_two_hex_digits() {
        for bad in ["%+1", "%-1", "%1", "%", "%zz", "%ff"] {
            let message = refused(&format!("postgres://u@h/db?a{bad}=1"));
            assert!(
                message.contains("not valid percent-encoding"),
                "{bad}: {message}"
            );
        }
        parsed("postgres://u@h/db?a%41=1");
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
