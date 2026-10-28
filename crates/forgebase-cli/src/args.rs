//! Command-line arguments (clap derive).

use std::path::PathBuf;

use clap::{Args, Parser, Subcommand};

/// Forgebase command-line tool.
///
/// Database commands read the connection URL from the `FORGEBASE_DATABASE_URL` environment
/// variable (`postgres://user:password@host:port/database`).
#[derive(Debug, Parser)]
#[command(name = "forgebase", version)]
pub struct Cli {
    #[command(subcommand)]
    pub command: Command,
}

#[derive(Debug, Subcommand)]
pub enum Command {
    /// Apply framework migrations and sync the DocType schema (additive changes only).
    Migrate {
        #[command(flatten)]
        doctypes: DocTypesDir,
        /// Print what would change and keep nothing.
        #[arg(long)]
        dry_run: bool,
        /// Longest wait for a database lock, in milliseconds.
        #[arg(long, value_name = "MS", default_value_t = 5000)]
        lock_timeout_ms: u64,
    },
    /// Insert, read, update and delete single documents.
    #[command(subcommand)]
    Doc(DocCommand),
}

#[derive(Debug, Subcommand)]
pub enum DocCommand {
    /// Insert a document and print it as stored.
    Insert {
        /// DocType name.
        doctype: String,
        #[command(flatten)]
        doctypes: DocTypesDir,
        #[command(flatten)]
        data: DataArg,
        #[command(flatten)]
        actor: ActorArg,
    },
    /// Print a document.
    Get {
        /// DocType name.
        doctype: String,
        /// Document name.
        name: String,
        #[command(flatten)]
        doctypes: DocTypesDir,
    },
    /// Change fields of a document and print it as stored. Fails if the document's version is
    /// not the one given with --version.
    Update {
        /// DocType name.
        doctype: String,
        /// Document name.
        name: String,
        #[command(flatten)]
        version: VersionArg,
        #[command(flatten)]
        doctypes: DocTypesDir,
        #[command(flatten)]
        data: DataArg,
        #[command(flatten)]
        actor: ActorArg,
    },
    /// Delete a document (prints nothing). Fails if the document's version is not the one given
    /// with --version, or if other documents still link to it.
    Delete {
        /// DocType name.
        doctype: String,
        /// Document name.
        name: String,
        #[command(flatten)]
        version: VersionArg,
        #[command(flatten)]
        doctypes: DocTypesDir,
    },
}

#[derive(Debug, Args)]
pub struct VersionArg {
    /// The version of the document you last read (the `version` field of `doc get`). The change
    /// is refused if someone else changed the document since.
    #[arg(long = "version", value_name = "N")]
    pub expected_version: i64,
}

#[derive(Debug, Args)]
pub struct DocTypesDir {
    /// Directory with one DocType `*.toml` file per DocType (not searched recursively).
    #[arg(long, value_name = "DIR")]
    pub doctypes: PathBuf,
}

#[derive(Debug, Args)]
pub struct DataArg {
    /// A JSON object with the field values, or `-` to read it from standard input.
    #[arg(long, value_name = "JSON|-")]
    pub data: String,
}

#[derive(Debug, Args)]
pub struct ActorArg {
    /// Recorded as owner / modified_by. Not authenticated.
    #[arg(long, value_name = "NAME", default_value = "cli")]
    pub actor: String,
}
