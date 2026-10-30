//! The error type of the CLI. Every error ends the command with exit code 1.

use std::fmt::Write as _;

use forgebase_db::DbError;
use forgebase_db::auth::AuthError;

use forgebase_http::json::JsonError;

use crate::compose::StartupError;

/// Everything that can make a command fail.
#[derive(Debug, thiserror::Error)]
pub enum CliError {
    /// `FORGEBASE_DATABASE_URL` (the database URL of the implicit site) is missing or empty.
    #[error(
        "{0} is not set; set it to the PostgreSQL URL, for example postgres://user:password@localhost:5432/database"
    )]
    MissingDatabaseUrl(&'static str),
    /// The command line names no usable site (`--site` missing or unknown). Exit code 2, like
    /// every usage error.
    #[error("{0}")]
    Usage(String),
    /// The config file or an environment variable has an unusable value.
    #[error("{0}")]
    Config(String),
    /// The server could not listen or stopped with an I/O error.
    #[error("server error: {0}")]
    Serve(std::io::Error),
    /// `new-app` cannot create the crate. The message says why.
    #[error("{0}")]
    NewApp(String),
    /// The apps or the DocType files are wrong; the message lists every problem.
    #[error("{0}")]
    Startup(#[from] StartupError),
    /// The DocType named on the command line is not in the DocType directory.
    #[error("unknown DocType `{name}`; known DocTypes: {known}")]
    UnknownDocType {
        /// The requested name.
        name: String,
        /// The known names, comma separated (or `none`).
        known: String,
    },
    /// A job command cannot be done; the message says why.
    #[error("{0}")]
    Job(String),
    /// The `--data` JSON cannot be used.
    #[error("{0}")]
    Json(#[from] JsonError),
    /// Reading standard input failed.
    #[error("cannot read standard input: {0}")]
    Stdin(std::io::Error),
    /// Writing the result failed.
    #[error("cannot write output: {0}")]
    Output(std::io::Error),
    /// A user, role or token error. Never contains a password or a secret.
    #[error("{0}")]
    Auth(#[from] AuthError),
    /// A database error while opening a site (connecting, creating its database); names the site.
    /// Never contains the database URL.
    #[error("site `{site}`: {}", describe_db(.source))]
    Site {
        /// The site name.
        site: String,
        /// What went wrong (boxed: a `DbError` is large).
        source: Box<DbError>,
    },
    /// A database or validation error.
    #[error("{}", describe_db(.0))]
    Db(#[from] DbError),
}

/// Multi-item database errors get one line per item.
pub(crate) fn describe_db(error: &DbError) -> String {
    match error {
        DbError::Validation(errors) => {
            let lines: Vec<String> = errors.iter().map(ToString::to_string).collect();
            format!("validation failed:\n{}", bullets(&lines))
        }
        DbError::InvalidMetadata(errors) => {
            let lines: Vec<String> = errors.iter().map(ToString::to_string).collect();
            format!("invalid DocType metadata:\n{}", bullets(&lines))
        }
        DbError::SyncConflicts(conflicts) => {
            let lines: Vec<String> = conflicts.iter().map(ToString::to_string).collect();
            format!(
                "schema sync refused, {} conflict(s); nothing was changed:\n{}",
                lines.len(),
                bullets(&lines)
            )
        }
        other => other.to_string(),
    }
}

pub(crate) fn bullets(lines: &[String]) -> String {
    let mut text = String::new();
    for (i, line) in lines.iter().enumerate() {
        if i > 0 {
            text.push('\n');
        }
        let _ = write!(text, "  - {line}");
    }
    text
}

#[cfg(test)]
mod tests {
    use super::*;
    use forgebase_core::document::ValidationError;

    #[test]
    fn validation_errors_are_listed_one_per_line() {
        let error = CliError::Db(DbError::Validation(vec![
            ValidationError::Required {
                field: "a".to_owned(),
            },
            ValidationError::UnknownField {
                field: "b".to_owned(),
            },
        ]));
        assert_eq!(
            error.to_string(),
            "validation failed:\n  - field `a` is required\n  - unknown field `b`"
        );
    }
}
