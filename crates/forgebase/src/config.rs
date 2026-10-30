//! Sites and the config file (M7.1, ADR 0014).
//!
//! A [`Site`] is one database and the host names that reach it. Without a config file there is
//! one implicit site called `default`, whose database URL is `FORGEBASE_DATABASE_URL`. With a
//! config file (`--config PATH` or `FORGEBASE_CONFIG`; never discovered implicitly) the sites
//! are listed in it, and each names the environment variable that holds its database URL: the
//! file has no secrets.
//!
//! There is no process-global pool or URL. A command picks exactly one [`Site`] (or `serve`
//! picks all of them), turns it into a [`SiteContext`] (its own pool and registry) and passes
//! that context down.

use std::path::Path;
use std::sync::Arc;

use forgebase_core::meta::Registry;
use forgebase_db::Pool;
use serde::Deserialize;

use crate::error::{CliError, bullets};

/// The environment variable of the implicit site's database URL. This is the only place that
/// names it (a test checks that).
const IMPLICIT_URL_VAR: &str = "FORGEBASE_DATABASE_URL";

/// The name of the implicit site.
pub(crate) const IMPLICIT_SITE: &str = "default";

/// The largest `pool_size` of a site.
const MAX_POOL_SIZE: i64 = 256;

/// One site: a name, the hosts that reach it and where its database URL comes from.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Site {
    pub name: String,
    /// Lower-cased host names, no port. Empty for the implicit site (see `serve`).
    pub hosts: Vec<String>,
    /// The environment variable that holds the PostgreSQL URL.
    database_url_env: String,
    /// The pool maximum, or `None` for the default of `forgebase_db::connect`.
    pub pool_size: Option<usize>,
}

impl Site {
    /// The implicit site of a setup without a config file.
    fn implicit() -> Self {
        Self {
            name: IMPLICIT_SITE.to_owned(),
            hosts: Vec::new(),
            database_url_env: IMPLICIT_URL_VAR.to_owned(),
            pool_size: None,
        }
    }

    /// The database URL, read from the environment when the site is used. The error names the
    /// site and the variable, never a value.
    fn database_url(&self) -> Result<String, CliError> {
        self.database_url_from(|name| std::env::var(name).ok())
    }

    /// [`Site::database_url`] with the environment given as a function, for the tests.
    fn database_url_from(&self, env: impl Fn(&str) -> Option<String>) -> Result<String, CliError> {
        match env(&self.database_url_env) {
            Some(url) if !url.is_empty() => Ok(url),
            _ if self.name == IMPLICIT_SITE && self.database_url_env == IMPLICIT_URL_VAR => {
                Err(CliError::MissingDatabaseUrl(IMPLICIT_URL_VAR))
            }
            _ => Err(CliError::Config(format!(
                "site `{}`: the environment variable {} is not set or is empty; set it to the \
                 PostgreSQL URL of the site",
                self.name, self.database_url_env
            ))),
        }
    }

    /// Opens the pool of this site (`pool_size` overrides the configured size) and joins it with
    /// `registry`. This is the only way a command gets a database connection.
    pub(crate) async fn context(
        &self,
        registry: Arc<Registry>,
        pool_size: Option<usize>,
    ) -> Result<SiteContext, CliError> {
        let url = self.database_url()?;
        let pool = forgebase_db::connect_sized(&url, pool_size.or(self.pool_size)).await?;
        Ok(SiteContext {
            site: self.clone(),
            pool,
            registry,
        })
    }
}

/// Everything a command needs for one site: the site, its pool and its registry. In M7.1 the
/// registry is the same `Arc` for every site; M7.2 builds one per site (its apps).
pub(crate) struct SiteContext {
    // Read by the tests; M7.2 uses it to report the site.
    #[allow(dead_code)]
    pub site: Site,
    pub pool: Pool,
    pub registry: Arc<Registry>,
}

/// The sites of this run: those of the config file, or the implicit `default`.
#[derive(Debug)]
pub(crate) struct Config {
    sites: Vec<Site>,
    configured: bool,
}

impl Config {
    /// The setup without a config file: one implicit site.
    pub fn implicit() -> Self {
        Self {
            sites: vec![Site::implicit()],
            configured: false,
        }
    }

    /// Reads and validates the config file at `path`. Every validation problem is in the error.
    pub fn load(path: &Path) -> Result<Self, CliError> {
        let text = std::fs::read_to_string(path).map_err(|e| {
            CliError::Config(format!(
                "cannot read the config file {}: {e}",
                path.display()
            ))
        })?;
        Self::parse(&text).map_err(|message| {
            CliError::Config(format!(
                "invalid config file {}:\n{message}",
                path.display()
            ))
        })
    }

    /// Parses and validates config text. The error is one message with every problem.
    fn parse(text: &str) -> Result<Self, String> {
        let raw: RawConfig = toml::from_str(text).map_err(|e| bullets(&[e.to_string()]))?;
        let sites = validate(raw).map_err(|errors| bullets(&errors))?;
        Ok(Self {
            sites,
            configured: true,
        })
    }

    /// True when the sites come from a config file.
    pub fn is_configured(&self) -> bool {
        self.configured
    }

    /// The names of the sites, comma separated.
    fn names(&self) -> String {
        self.sites
            .iter()
            .map(|s| s.name.as_str())
            .collect::<Vec<_>>()
            .join(", ")
    }

    fn find(&self, name: &str) -> Result<&Site, CliError> {
        self.sites.iter().find(|s| s.name == name).ok_or_else(|| {
            if self.configured {
                CliError::Usage(format!(
                    "unknown site `{name}`; the sites of the config file are: {}",
                    self.names()
                ))
            } else {
                CliError::Usage(format!(
                    "unknown site `{name}`; without a config file (--config) the only site is `{IMPLICIT_SITE}`"
                ))
            }
        })
    }

    /// The one site of a database command. `--site` is required when the config file has more
    /// than one site, and optional (the only site) otherwise.
    pub fn select(&self, name: Option<&str>) -> Result<&Site, CliError> {
        match (name, self.sites.as_slice()) {
            (Some(name), _) => self.find(name),
            (None, [only]) => Ok(only),
            (None, _) => Err(CliError::Usage(format!(
                "the config file has several sites, so this command needs --site NAME; the sites are: {}",
                self.names()
            ))),
        }
    }

    /// The sites `serve` serves: the one named by `--site`, else every site.
    pub fn serve_set(&self, name: Option<&str>) -> Result<Vec<&Site>, CliError> {
        match name {
            Some(name) => Ok(vec![self.find(name)?]),
            None => Ok(self.sites.iter().collect()),
        }
    }
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct RawConfig {
    #[serde(default)]
    sites: Vec<RawSite>,
}

/// Missing keys read as empty, so that validation reports them with the other problems.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct RawSite {
    #[serde(default)]
    name: String,
    #[serde(default)]
    hosts: Vec<String>,
    #[serde(default)]
    database_url_env: String,
    pool_size: Option<i64>,
}

fn validate(raw: RawConfig) -> Result<Vec<Site>, Vec<String>> {
    let mut errors = Vec::new();
    if raw.sites.is_empty() {
        errors.push("no sites: add at least one [[sites]] table".to_owned());
    }
    let mut sites: Vec<Site> = Vec::new();
    for (index, raw) in raw.sites.into_iter().enumerate() {
        let label = if raw.name.is_empty() {
            format!("site #{}", index + 1)
        } else {
            format!("site `{}`", raw.name)
        };
        if !valid_site_name(&raw.name) {
            errors.push(format!(
                "{label}: the name must be 1 to 63 characters: lower-case ASCII letters, digits \
                 and underscores, starting with a letter"
            ));
        } else if sites.iter().any(|s| s.name == raw.name) {
            errors.push(format!("{label}: the site name is used twice"));
        }
        if raw.hosts.is_empty() {
            errors.push(format!("{label}: `hosts` must list at least one host name"));
        }
        let mut hosts: Vec<String> = Vec::new();
        for host in &raw.hosts {
            let host = host.to_ascii_lowercase();
            if let Some(problem) = host_problem(&host) {
                errors.push(format!("{label}: host {host:?} {problem}"));
            } else if let Some(other) = sites.iter().find(|s| s.hosts.contains(&host)) {
                errors.push(format!(
                    "{label}: host {host:?} is already used by site `{}`",
                    other.name
                ));
            } else if !hosts.contains(&host) {
                hosts.push(host);
            }
        }
        if !valid_env_name(&raw.database_url_env) {
            errors.push(format!(
                "{label}: `database_url_env` must be the name of an environment variable \
                 (letters, digits and underscores, not starting with a digit)"
            ));
        }
        let pool_size = match raw.pool_size {
            None => None,
            Some(n) if (1..=MAX_POOL_SIZE).contains(&n) => usize::try_from(n).ok(),
            Some(n) => {
                errors.push(format!(
                    "{label}: `pool_size` is {n}, it must be 1 to {MAX_POOL_SIZE}"
                ));
                None
            }
        };
        sites.push(Site {
            name: raw.name,
            hosts,
            database_url_env: raw.database_url_env,
            pool_size,
        });
    }
    if errors.is_empty() {
        Ok(sites)
    } else {
        Err(errors)
    }
}

/// The rules of an app name: `[a-z][a-z0-9_]*`, at most 63 bytes.
fn valid_site_name(name: &str) -> bool {
    let mut bytes = name.bytes();
    name.len() <= 63
        && bytes.next().is_some_and(|b| b.is_ascii_lowercase())
        && bytes.all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'_')
}

fn valid_env_name(name: &str) -> bool {
    let mut bytes = name.bytes();
    bytes
        .next()
        .is_some_and(|b| b.is_ascii_alphabetic() || b == b'_')
        && bytes.all(|b| b.is_ascii_alphanumeric() || b == b'_')
}

/// Why `host` cannot be a host name of a site, or `None` when it can. The same rules apply to
/// `FORGEBASE_ALLOWED_HOSTS`: a host name without a port (the port of the `Host` header is
/// ignored), and IPv6 in brackets.
pub(crate) fn host_problem(host: &str) -> Option<&'static str> {
    if host.is_empty() || host.bytes().any(|b| !b.is_ascii_graphic()) {
        return Some("must be a host name (no spaces or other characters outside ASCII)");
    }
    // `[::1]` is a host name with colons; anything after its `]`, or a colon outside brackets,
    // is a port (or an unbracketed IPv6 address).
    let bad = if host.starts_with('[') {
        !host.ends_with(']')
    } else {
        host.contains(':')
    };
    bad.then_some(
        "must be a host name without a port (the port of the Host header is ignored; write IPv6 as [::1])",
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    const GOOD: &str = r#"
[[sites]]
name = "acme"
hosts = ["Acme.Example.com", "acme.localhost"]
database_url_env = "ACME_DATABASE_URL"
pool_size = 4

[[sites]]
name = "beta"
hosts = ["beta.localhost"]
database_url_env = "BETA_DATABASE_URL"
"#;

    fn problems(text: &str) -> String {
        Config::parse(text).expect_err("must be refused")
    }

    #[test]
    fn a_valid_file_is_read() {
        let config = Config::parse(GOOD).expect("valid");
        assert!(config.is_configured());
        assert_eq!(config.names(), "acme, beta");
        let acme = config.select(Some("acme")).expect("acme");
        assert_eq!(acme.hosts, ["acme.example.com", "acme.localhost"]);
        assert_eq!(acme.pool_size, Some(4));
        assert_eq!(config.find("beta").expect("beta").pool_size, None);
    }

    #[test]
    fn site_selection_follows_the_number_of_sites() {
        let two = Config::parse(GOOD).expect("valid");
        let message = two.select(None).expect_err("required").to_string();
        assert!(
            message.contains("--site") && message.contains("acme, beta"),
            "{message}"
        );
        assert!(matches!(two.select(None), Err(CliError::Usage(_))));
        assert!(matches!(two.select(Some("nope")), Err(CliError::Usage(_))));
        assert_eq!(two.serve_set(None).expect("all").len(), 2);
        assert_eq!(two.serve_set(Some("beta")).expect("one").len(), 1);

        let one = Config::parse(
            "[[sites]]\nname = \"a\"\nhosts = [\"a.test\"]\ndatabase_url_env = \"A_URL\"\n",
        )
        .expect("valid");
        assert_eq!(one.select(None).expect("the only site").name, "a");

        let none = Config::implicit();
        assert!(!none.is_configured());
        assert_eq!(none.select(None).expect("default").name, "default");
        assert_eq!(
            none.select(Some("default")).expect("default").name,
            "default"
        );
        let message = none.select(Some("acme")).expect_err("unknown").to_string();
        assert!(message.contains("only site is `default`"), "{message}");
    }

    #[test]
    fn every_problem_is_reported_together() {
        let message = problems(
            r#"
[[sites]]
name = "Bad Name"
hosts = []
database_url_env = "1X"
pool_size = 0

[[sites]]
name = "a"
hosts = ["a.test", "a.test:8000", "[::1"]
database_url_env = "A URL"
pool_size = 9999

[[sites]]
name = "a"
hosts = ["A.TEST"]
database_url_env = "A_URL"

[[sites]]
hosts = ["x.test"]
database_url_env = "X_URL"
"#,
        );
        for expected in [
            "site `Bad Name`: the name must be",
            "site `Bad Name`: `hosts` must list at least one",
            "site `Bad Name`: `database_url_env` must be",
            "site `Bad Name`: `pool_size` is 0",
            "site `a`: host \"a.test:8000\" must be a host name without a port",
            "site `a`: host \"[::1\" must be a host name without a port",
            "site `a`: `database_url_env` must be",
            "site `a`: `pool_size` is 9999",
            "site `a`: the site name is used twice",
            "site `a`: host \"a.test\" is already used by site `a`",
            "site #4: the name must be",
        ] {
            assert!(
                message.contains(expected),
                "missing {expected:?} in:\n{message}"
            );
        }
    }

    #[test]
    fn a_host_listed_by_two_sites_names_both() {
        let message = problems(
            r#"
[[sites]]
name = "a"
hosts = ["x.test"]
database_url_env = "A_URL"
[[sites]]
name = "b"
hosts = ["X.test"]
database_url_env = "B_URL"
"#,
        );
        assert!(
            message.contains("site `b`: host \"x.test\" is already used by site `a`"),
            "{message}"
        );
    }

    #[test]
    fn no_sites_and_unknown_keys_are_refused() {
        assert!(problems("").contains("no sites"));
        assert!(problems("sites = []").contains("no sites"));
        for text in [
            "bogus = 1\n[[sites]]\nname = \"a\"",
            // `apps` arrives with M7.2.
            "[[sites]]\nname = \"a\"\nhosts = [\"a.test\"]\ndatabase_url_env = \"A_URL\"\napps = [\"x\"]\n",
            "[[sites]]\nname = \"a\"\nhosts = [\"a.test\"]\ndatabase_url = \"postgres://u:p@h/d\"\n",
        ] {
            let message = problems(text);
            assert!(message.contains("unknown field"), "{message}");
        }
        assert!(Config::parse("not toml [").is_err());
    }

    #[test]
    fn the_value_of_the_variable_is_never_in_a_message() {
        let site = Site {
            name: "acme".to_owned(),
            hosts: vec!["acme.test".to_owned()],
            database_url_env: "ACME_DATABASE_URL".to_owned(),
            pool_size: None,
        };
        let secret = "postgres://user:hunter2@host/db";
        let set = |name: &str| (name == "ACME_DATABASE_URL").then(|| secret.to_owned());
        assert_eq!(site.database_url_from(set).expect("set"), secret);
        // Unset, empty, or only another variable set: the error names the site and the variable.
        let other = |name: &str| (name == "OTHER").then(|| secret.to_owned());
        let empty = |_: &str| Some(String::new());
        for result in [site.database_url_from(other), site.database_url_from(empty)] {
            let message = result.expect_err("unusable").to_string();
            assert!(
                message.contains("site `acme`") && message.contains("ACME_DATABASE_URL"),
                "{message}"
            );
            assert!(!message.contains("hunter2"), "{message}");
        }
        // A bad config file never echoes the environment either: validation does not read it.
        assert!(!problems("[[sites]]\nname = \"a\"").contains("hunter2"));
    }

    #[test]
    fn host_rules_match_the_allowed_hosts_rules() {
        for good in ["localhost", "127.0.0.1", "[::1]", "erp.example.com"] {
            assert_eq!(host_problem(good), None, "{good}");
        }
        for bad in ["", "a:80", "[::1]:80", "::1", "a b", "[::1", "é.test"] {
            assert!(host_problem(bad).is_some(), "{bad:?}");
        }
    }

    /// Only the implicit-site builder reads `FORGEBASE_DATABASE_URL`: no other code (comments
    /// aside) names it, so no code path can reach a database without going through a [`Site`].
    #[test]
    fn nothing_but_the_implicit_site_reads_the_database_url_variable() {
        let src = Path::new(env!("CARGO_MANIFEST_DIR")).join("src");
        let mut seen = 0;
        for entry in std::fs::read_dir(&src).expect("src") {
            let path = entry.expect("entry").path();
            if path.extension().is_none_or(|e| e != "rs") {
                continue;
            }
            let text = std::fs::read_to_string(&path).expect("read");
            // Test modules (at the end of a file) may name it.
            let code_part = text.split("#[cfg(test)]").next().unwrap_or_default();
            for (number, line) in code_part.lines().enumerate() {
                let code = line.trim_start();
                if code.starts_with("//") {
                    continue;
                }
                if code.contains("FORGEBASE_DATABASE_URL") {
                    assert!(
                        path.ends_with("config.rs") && code.contains("IMPLICIT_URL_VAR: &str"),
                        "{}:{}: {line}",
                        path.display(),
                        number + 1
                    );
                    seen += 1;
                }
                assert!(
                    !code.contains("database_url()") || path.ends_with("config.rs"),
                    "{}:{}: {line}",
                    path.display(),
                    number + 1
                );
            }
        }
        assert_eq!(seen, 1, "the constant of the implicit site");
    }
}
