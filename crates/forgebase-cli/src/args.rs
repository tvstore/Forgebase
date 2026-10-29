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
    /// Insert, read, update, delete, submit, cancel and amend single documents.
    #[command(subcommand)]
    Doc(DocCommand),
    /// Create and manage users and their roles. Passwords are read from standard input.
    #[command(subcommand)]
    User(UserCommand),
    /// Create and revoke API tokens.
    #[command(subcommand)]
    Token(TokenCommand),
}

#[derive(Debug, Subcommand)]
pub enum UserCommand {
    /// Create an enabled user. The password is read from the first line of standard input.
    Create {
        /// Username: 1 to 140 ASCII letters, digits and `_ . @ -` (case-sensitive).
        username: String,
        /// A role to give the user; repeat for several.
        #[arg(long = "role", value_name = "ROLE")]
        roles: Vec<String>,
    },
    /// Change a user's password (read from the first line of standard input) and end all of the
    /// user's sessions.
    SetPassword {
        /// Username.
        username: String,
    },
    /// Give a user a role.
    AddRole {
        /// Username.
        username: String,
        /// Role name.
        role: String,
    },
    /// Take a role from a user.
    RemoveRole {
        /// Username.
        username: String,
        /// Role name.
        role: String,
    },
    /// Disable a user: no login, and its sessions and API tokens stop working.
    Disable {
        /// Username.
        username: String,
    },
    /// Enable a disabled user.
    Enable {
        /// Username.
        username: String,
    },
}

#[derive(Debug, Subcommand)]
pub enum TokenCommand {
    /// Create an API token for a user and print `key:secret` once. The secret cannot be shown
    /// again; store it now.
    Create {
        /// Username.
        username: String,
    },
    /// Revoke an API token by its key.
    Revoke {
        /// The key (the part before the colon).
        key: String,
    },
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
    /// Submit a draft of a submittable DocType (it becomes locked) and print it as stored. Fails
    /// if the document's version is not the one given with --version.
    Submit {
        /// DocType name.
        doctype: String,
        /// Document name.
        name: String,
        #[command(flatten)]
        version: VersionArg,
        #[command(flatten)]
        doctypes: DocTypesDir,
        #[command(flatten)]
        actor: ActorArg,
    },
    /// Cancel a submitted document and print it as stored. Fails if the document's version is
    /// not the one given with --version.
    Cancel {
        /// DocType name.
        doctype: String,
        /// Document name.
        name: String,
        #[command(flatten)]
        version: VersionArg,
        #[command(flatten)]
        doctypes: DocTypesDir,
        #[command(flatten)]
        actor: ActorArg,
    },
    /// Amend a cancelled document: create a new draft named `<name>-<n>` with a copy of its
    /// values and rows, and print it. Takes no data; --version is the cancelled document's.
    Amend {
        /// DocType name.
        doctype: String,
        /// Document name (of the cancelled document).
        name: String,
        #[command(flatten)]
        version: VersionArg,
        #[command(flatten)]
        doctypes: DocTypesDir,
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
        #[command(flatten)]
        actor: ActorArg,
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
    /// Recorded as owner / modified_by on insert, update, submit, cancel and amend; given to
    /// hooks (on delete it is only given to hooks, not stored). Not authenticated.
    #[arg(long, value_name = "NAME", default_value = "cli")]
    pub actor: String,
}
