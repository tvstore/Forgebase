//! Command-line arguments (clap derive).

use std::path::PathBuf;

use clap::error::ErrorKind;
use clap::{Args, CommandFactory, Parser, Subcommand};

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
    /// Apply framework migrations, sync the DocType schema (additive changes only) and apply the
    /// SQL data migrations of the apps compiled into the binary.
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
    /// Insert, read, list, update, delete, submit, cancel and amend documents. These commands run
    /// as the trusted system context: no permission rules apply (see --actor).
    #[command(subcommand)]
    Doc(DocCommand),
    /// Create and manage users and their roles. Passwords are read from standard input.
    #[command(subcommand)]
    User(UserCommand),
    /// Create and revoke API tokens.
    #[command(subcommand)]
    Token(TokenCommand),
    /// Create a new app: a Rust crate (library and binary) with a sample DocType, a hook, tests,
    /// a README and an AGENTS.md, in `<DIR>/<NAME>`. It builds and passes its tests as created. Needs
    /// no database. Never overwrites: an existing target directory is an error.
    NewApp {
        /// The app and crate name: lower-case ASCII letters, digits and underscores, starting
        /// with a letter (the rules of a DocType name), at most 54 bytes.
        name: String,
        /// The Forgebase checkout the app depends on (it contains `crates/forgebase`). Written
        /// into the app's `Cargo.toml` as an absolute path.
        #[arg(long, value_name = "DIR")]
        forgebase_path: PathBuf,
        /// The directory in which `<NAME>` is created.
        #[arg(long, value_name = "DIR", default_value = ".")]
        dir: PathBuf,
    },
    /// Serve the HTTP API (`/api/v1`) until Ctrl-C or SIGTERM (running requests get 10 seconds; a second signal exits at once). It does not run migrations: run `migrate`
    /// first. Environment: `FORGEBASE_LISTEN` (default `127.0.0.1:8000`),
    /// `FORGEBASE_ALLOWED_HOSTS` (comma separated host names without ports, default `localhost,127.0.0.1`) and
    /// `FORGEBASE_DEV_INSECURE_COOKIES=1` (development only: no `Secure` on the session cookie)
    /// and `FORGEBASE_DESK_DIR` (the built desk, typically `desk/dist`, served under `/desk/`; unset: no desk).
    Serve {
        #[command(flatten)]
        doctypes: DocTypesDir,
    },
}

impl Command {
    /// The `--doctypes` option of the command, for commands that have it.
    pub fn doctypes(&self) -> Option<&DocTypesDir> {
        match self {
            Command::Migrate { doctypes, .. } | Command::Serve { doctypes } => Some(doctypes),
            Command::Doc(
                DocCommand::Insert { doctypes, .. }
                | DocCommand::Get { doctypes, .. }
                | DocCommand::List { doctypes, .. }
                | DocCommand::Update { doctypes, .. }
                | DocCommand::Submit { doctypes, .. }
                | DocCommand::Cancel { doctypes, .. }
                | DocCommand::Amend { doctypes, .. }
                | DocCommand::Delete { doctypes, .. },
            ) => Some(doctypes),
            Command::User(_) | Command::Token(_) | Command::NewApp { .. } => None,
        }
    }
}

impl Command {
    /// The names of the (sub)commands that select this command, for example `["doc", "get"]`.
    fn path(&self) -> [&'static str; 2] {
        match self {
            Command::Migrate { .. } => ["migrate", ""],
            Command::Serve { .. } => ["serve", ""],
            Command::NewApp { .. } => ["new-app", ""],
            Command::User(_) => ["user", ""],
            Command::Token(_) => ["token", ""],
            Command::Doc(doc) => [
                "doc",
                match doc {
                    DocCommand::Insert { .. } => "insert",
                    DocCommand::Get { .. } => "get",
                    DocCommand::List { .. } => "list",
                    DocCommand::Update { .. } => "update",
                    DocCommand::Submit { .. } => "submit",
                    DocCommand::Cancel { .. } => "cancel",
                    DocCommand::Amend { .. } => "amend",
                    DocCommand::Delete { .. } => "delete",
                },
            ],
        }
    }

    /// The usage error for a command that needs `--doctypes` and did not get it (a binary without
    /// apps has no other source of DocTypes). It is the error clap itself would give for a
    /// required option, so it shows the usage of the subcommand.
    pub fn missing_doctypes_error(&self) -> clap::Error {
        let mut root = Cli::command();
        root.build();
        let mut command = &root;
        for name in self.path().into_iter().filter(|name| !name.is_empty()) {
            command = command.find_subcommand(name).unwrap_or(command);
        }
        command
            .clone()
            .mut_arg("doctypes", |arg| arg.required(true))
            .error(
                ErrorKind::MissingRequiredArgument,
                "the following required arguments were not provided:\n  --doctypes <DIR>",
            )
    }
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
    /// Print a page of documents as a JSON array, filtered and sorted. Child table rows are not
    /// listed. The default page is 20 documents, at most 500.
    ///
    /// --filter is a JSON array of conditions, combined with AND, each `{"field": F, "op": OP,
    /// "value": V}`. OP is one of `=`, `!=`, `<`, `<=`, `>`, `>=`, `in` (V is an array), `like`
    /// (text fields only; V is a pattern with % and _) or `is null` / `is not null` (no `value`).
    /// V is written as in `--data`. Example:
    /// --filter '[{"field":"status","op":"=","value":"open"},{"field":"total","op":">","value":"100.00"}]'
    List {
        /// DocType name.
        doctype: String,
        #[command(flatten)]
        doctypes: DocTypesDir,
        /// A JSON array of filters, see above.
        #[arg(long, value_name = "JSON")]
        filter: Option<String>,
        /// Sort key `FIELD` or `FIELD:desc` (also `FIELD:asc`); repeat for several. `name` is
        /// always the last key.
        #[arg(long, value_name = "FIELD[:desc]")]
        sort: Vec<String>,
        /// Page size, 1 to 500 (default 20).
        #[arg(long, value_name = "N")]
        limit: Option<u32>,
        /// Documents to skip (default 0).
        #[arg(long, value_name = "N")]
        offset: Option<u64>,
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
    /// Directory with one DocType `*.toml` file per DocType (not searched recursively). It
    /// becomes the app `local`, which depends on every app compiled into the binary. Required
    /// when the binary has no apps (the stock `forgebase` binary); a binary with apps does not
    /// need it.
    #[arg(long, value_name = "DIR")]
    pub doctypes: Option<PathBuf>,
}

#[derive(Debug, Args)]
pub struct DataArg {
    /// A JSON object with the field values, or `-` to read it from standard input.
    #[arg(long, value_name = "JSON|-")]
    pub data: String,
}

#[derive(Debug, Args)]
pub struct ActorArg {
    /// The name of the trusted system context these commands run as: recorded as owner /
    /// modified_by on insert, update, submit, cancel and amend; given to hooks (on delete it is
    /// only given to hooks, not stored). Not authenticated, and no permission rules apply to it.
    #[arg(long, value_name = "NAME", default_value = "cli")]
    pub actor: String,
}
