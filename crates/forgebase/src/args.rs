//! Command-line arguments (clap derive).

use std::path::PathBuf;

use clap::error::ErrorKind;
use clap::{Args, CommandFactory, Parser, Subcommand};

/// Forgebase command-line tool.
///
/// Database commands work on one site, which is one database. Without a config file there is one
/// site, `default`, and its connection URL comes from the `FORGEBASE_DATABASE_URL` environment
/// variable (`postgres://user:password@host:port/database`). With a config file (`--config`)
/// the sites are listed in it, each naming the environment variable that holds its URL.
#[derive(Debug, Parser)]
#[command(name = "forgebase", version)]
pub struct Cli {
    /// The config file (`forgebase.toml`) that lists the sites. Also read from the environment
    /// variable `FORGEBASE_CONFIG`; this option wins. It is never looked for in the current
    /// directory. Without one, the only site is `default`.
    #[arg(long, global = true, value_name = "PATH")]
    pub config: Option<PathBuf>,
    /// The site a database command works on. Required when the config file has several sites,
    /// except for `serve`, which serves every site without it. Optional with one site, and
    /// without a config file (where only `default` exists).
    #[arg(long, global = true, value_name = "NAME")]
    pub site: Option<String>,
    #[command(subcommand)]
    pub command: Command,
}

#[derive(Debug, Subcommand)]
pub enum Command {
    /// Apply framework migrations, sync the DocType schema (additive changes only) and apply the
    /// SQL data migrations of the apps installed on the site (all the binary's apps unless the
    /// site lists `apps`). The apps recorded in the database but not installed are reported as a
    /// warning; their tables and data are kept. The schedules of apps that are not installed are
    /// deleted.
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
    /// Insert, read, list, update, delete, submit, cancel, amend and (with a workflow) transition documents. These commands run
    /// as the trusted system context: no permission rules apply (see --actor).
    #[command(subcommand)]
    Doc(DocCommand),
    /// Export documents to a CSV or JSON file and import them from one (M9.3, ADR 0021). These
    /// commands run as the trusted system context: no permission rules apply (see --actor of
    /// `import`). Both use the document pipeline, so an import runs the hooks, validation and
    /// webhooks of every row.
    #[command(subcommand)]
    Data(DataCommand),
    /// Create and manage users and their roles. Passwords are read from standard input.
    #[command(subcommand)]
    User(UserCommand),
    /// Create and revoke API tokens.
    #[command(subcommand)]
    Token(TokenCommand),
    /// Manage the outgoing webhooks of a site (M9.2, ADR 0020): a webhook posts a signed JSON
    /// snapshot of a document to a URL after it was inserted, updated, deleted, submitted,
    /// cancelled or moved by a workflow transition. A document operation enqueues the delivery in
    /// its own transaction, and `forgebase worker` sends it (so a site that has webhooks needs a
    /// running worker). Secrets are printed once, by `add` and `rotate-secret`, and never again.
    #[command(subcommand)]
    Webhook(WebhookCommand),
    /// Customize the DocTypes of one site (M10.1, ADR 0023). Administrators of the server use the
    /// command line (or the Administrator-only API); there is no admin screen.
    #[command(subcommand)]
    Customize(CustomizeCommand),
    /// List the sites of the config file, create the database of a site, and back it up and
    /// restore it. `--site` is not used here: the commands take the site name as an argument.
    #[command(subcommand)]
    Site(SiteCommand),
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
    /// Run background jobs until Ctrl-C or SIGTERM. It claims only the jobs of the apps installed
    /// on the site (and the built-in ones), runs each in a database transaction with its completion, retries a
    /// failure with a growing delay and marks a job `dead` when the attempts are used up. The
    /// first signal stops claiming and lets running jobs finish for 30 seconds (then they are
    /// given back, not counted as an attempt); a second signal exits at once. It does not run
    /// migrations: run `migrate` first.
    Worker {
        #[command(flatten)]
        doctypes: DocTypesDir,
        /// How many jobs run at the same time, 1 to 64.
        #[arg(long, value_name = "N", default_value_t = 1, value_parser = clap::value_parser!(u32).range(1..=64))]
        concurrency: u32,
    },
    /// Inspect and operate the job queue. There is no HTTP or desk interface for jobs: the
    /// command line is the interface.
    #[command(subcommand)]
    Job(JobCommand),
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
            Command::Migrate { doctypes, .. }
            | Command::Serve { doctypes }
            | Command::Worker { doctypes, .. } => Some(doctypes),
            Command::Data(
                DataCommand::Export { doctypes, .. } | DataCommand::Import { doctypes, .. },
            ) => Some(doctypes),
            Command::Doc(
                DocCommand::Insert { doctypes, .. }
                | DocCommand::Get { doctypes, .. }
                | DocCommand::List { doctypes, .. }
                | DocCommand::Update { doctypes, .. }
                | DocCommand::Submit { doctypes, .. }
                | DocCommand::Cancel { doctypes, .. }
                | DocCommand::Transition { doctypes, .. }
                | DocCommand::Amend { doctypes, .. }
                | DocCommand::Delete { doctypes, .. },
            ) => Some(doctypes),
            // `--empty` uses no apps and no DocTypes, so it needs no `--doctypes` either.
            Command::Site(SiteCommand::Create {
                doctypes,
                empty: false,
                ..
            }) => Some(doctypes),
            // Only `webhook add` needs the DocTypes (to check the DocType of the webhook).
            Command::Webhook(WebhookCommand::Add { doctypes, .. }) => Some(doctypes),
            // Only `customize field add` needs the DocTypes (to check the field against them).
            Command::Customize(CustomizeCommand::Field(FieldCommand::Add { doctypes, .. })) => {
                Some(doctypes)
            }
            Command::Site(
                SiteCommand::Create { empty: true, .. }
                | SiteCommand::Backup { .. }
                | SiteCommand::Restore { .. }
                | SiteCommand::List,
            )
            | Command::Webhook(_)
            | Command::Customize(_)
            | Command::User(_)
            | Command::Token(_)
            | Command::Job(_)
            | Command::NewApp { .. } => None,
        }
    }
}

impl Command {
    /// The names of the (sub)commands that select this command, for example `["doc", "get"]`.
    fn path(&self) -> [&'static str; 2] {
        match self {
            Command::Migrate { .. } => ["migrate", ""],
            Command::Serve { .. } => ["serve", ""],
            Command::Worker { .. } => ["worker", ""],
            Command::Job(_) => ["job", ""],
            Command::NewApp { .. } => ["new-app", ""],
            Command::User(_) => ["user", ""],
            Command::Token(_) => ["token", ""],
            Command::Webhook(webhook) => [
                "webhook",
                match webhook {
                    WebhookCommand::Add { .. } => "add",
                    WebhookCommand::List => "list",
                    WebhookCommand::Disable { .. } => "disable",
                    WebhookCommand::Enable { .. } => "enable",
                    WebhookCommand::Remove { .. } => "remove",
                    WebhookCommand::RotateSecret { .. } => "rotate-secret",
                    WebhookCommand::Test { .. } => "test",
                },
            ],
            Command::Customize(CustomizeCommand::Field(field)) => [
                "customize",
                match field {
                    FieldCommand::Add { .. } => "field add",
                    FieldCommand::List { .. } => "field list",
                    FieldCommand::Disable { .. } => "field disable",
                    FieldCommand::Enable { .. } => "field enable",
                },
            ],
            Command::Site(SiteCommand::List) => ["site", "list"],
            Command::Site(SiteCommand::Create { .. }) => ["site", "create"],
            Command::Site(SiteCommand::Backup { .. }) => ["site", "backup"],
            Command::Site(SiteCommand::Restore { .. }) => ["site", "restore"],
            Command::Data(data) => [
                "data",
                match data {
                    DataCommand::Export { .. } => "export",
                    DataCommand::Import { .. } => "import",
                },
            ],
            Command::Doc(doc) => [
                "doc",
                match doc {
                    DocCommand::Insert { .. } => "insert",
                    DocCommand::Get { .. } => "get",
                    DocCommand::List { .. } => "list",
                    DocCommand::Update { .. } => "update",
                    DocCommand::Submit { .. } => "submit",
                    DocCommand::Cancel { .. } => "cancel",
                    DocCommand::Transition { .. } => "transition",
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
pub enum SiteCommand {
    /// Print each site as a table: its name, hosts, apps (`all` when the config file lists none)
    /// and the name of the environment variable that holds its database URL (never the URL).
    /// Needs no database. Without a config file it shows the implicit site `default`.
    List,
    /// Create the database of a site, then migrate it as `migrate --site NAME` does. The site
    /// must be in the config file (or be `default` without one). Refuses, with exit code 1, when
    /// the database exists already: it never migrates an existing database implicitly (use
    /// `migrate`). The database name is the one in the site's URL; the role of the URL needs the
    /// `CREATEDB` privilege. If the migrate fails the database is kept: fix the problem and run
    /// `migrate --site NAME`. With `--empty` it only creates the empty database (for `restore`).
    Create {
        /// The site name.
        name: String,
        /// The database to connect to for `CREATE DATABASE` (on the same server, with the same
        /// user and password as the site's URL).
        #[arg(long, value_name = "NAME", default_value = forgebase_db::DEFAULT_MAINTENANCE_DB)]
        maintenance_db: String,
        /// Create the database and do not migrate it: it has no tables, which is what `site
        /// restore` needs. No apps and no `--doctypes` are used.
        #[arg(long)]
        empty: bool,
        #[command(flatten)]
        doctypes: DocTypesDir,
    },
    /// Write a backup of the site's database to a new file, with `pg_dump` (PostgreSQL client
    /// tools of the server's major version, on `PATH` or in `FORGEBASE_PG_BIN`; they are not part
    /// of Forgebase). The file is a custom-format dump of everything in the database, **password
    /// hashes, session and API token digests included**: it is created with mode 0600; keep it
    /// safe. The dump is written to `FILE.partial` and published as FILE (a hard link) only when it is complete:
    /// a failed backup leaves nothing, and an interrupted one (SIGINT, SIGTERM) can leave only
    /// `FILE.partial`, which can be removed. An existing file is never overwritten (exit 1; there
    /// is no `--force`). The database URL and password are passed to `pg_dump` in its
    /// environment, never on its command line.
    Backup {
        /// The site name.
        name: String,
        /// The file to create. It must not exist.
        #[arg(long, value_name = "FILE")]
        out: PathBuf,
    },
    /// Restore a backup of `site backup` into the site's database with `pg_restore`, in one
    /// transaction. `pg_restore` must have the server's major version, and the backup must come
    /// from a `pg_dump` of the same or an older major version. The database must exist and be
    /// **empty** (no tables, schemas other than `public`, extensions other than `plpgsql`,
    /// functions or types: the kinds of objects checked): create it with `site create NAME
    /// --empty`. Nothing is migrated: run `migrate --site NAME` afterwards if this binary is
    /// newer than the backup.
    Restore {
        /// The site name.
        name: String,
        /// The backup file.
        #[arg(long, value_name = "FILE")]
        from: PathBuf,
    },
}

#[derive(Debug, Subcommand)]
pub enum JobCommand {
    /// Print the newest jobs as a table: id, name, status, attempts, run_at and the start of the
    /// last error.
    List {
        /// Only jobs with this status.
        #[arg(long, value_name = "STATUS", value_parser = ["queued", "running", "done", "dead"])]
        status: Option<String>,
        /// Only jobs with this full name (`<app>.<job>`).
        #[arg(long, value_name = "NAME")]
        name: Option<String>,
        /// How many jobs, 1 to 1000.
        #[arg(long, value_name = "N", default_value_t = 50, value_parser = clap::value_parser!(u32).range(1..=1000))]
        limit: u32,
    },
    /// Print every field of a job as JSON, the payload included.
    Show {
        /// The job id.
        id: String,
    },
    /// Queue a dead job again: attempts back to 0, due now, error cleared. Any other status is
    /// an error.
    Retry {
        /// The job id.
        id: String,
    },
    /// Enqueue a job by its full name (`<app>.<job>`), which the binary must know. The job is
    /// recorded as enqueued by `--actor`. The payload is stored in clear text: never put a
    /// secret in it.
    Enqueue {
        /// The full job name.
        name: String,
        /// The payload, any JSON (at most 64 KiB), or `-` to read it from standard input.
        #[arg(long, value_name = "JSON|-", default_value = "{}")]
        payload: String,
        #[command(flatten)]
        actor: ActorArg,
    },
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
pub enum WebhookCommand {
    /// Add a webhook and print its secret once. The secret signs every delivery
    /// (`X-Forgebase-Signature`, HMAC-SHA256); store it now, it cannot be shown again. It is kept
    /// in the site's database in plain text (the signature needs it), so anyone who can read
    /// that database can read it. The URL must be `https://` and must not contain user info; the
    /// DocType must exist and must not be a child DocType. At delivery time the host is
    /// resolved and must not be a loopback, private, link-local or reserved address (see
    /// --allow-private).
    Add {
        /// The webhook name: lower-case letters, digits and underscores, starting with a letter.
        name: String,
        /// The DocType whose documents trigger the webhook.
        #[arg(long, value_name = "DOCTYPE")]
        doctype: String,
        /// The events, comma separated: insert, update, delete, submit, cancel, transition. A
        /// workflow transition that submits or cancels the document also fires `submit` or
        /// `cancel`; an amendment fires `insert`.
        #[arg(long, value_name = "EVENTS", value_delimiter = ',')]
        events: Vec<String>,
        /// The URL deliveries are posted to.
        #[arg(long, value_name = "URL")]
        url: String,
        /// Allow the URL to resolve to a loopback, private, link-local or reserved address
        /// (including the cloud metadata address). Off by default: it is the SSRF guard. Use it
        /// only for a receiver inside your own network.
        #[arg(long)]
        allow_private: bool,
        /// Allow `http://` instead of `https://`. For development only: the payload and its
        /// signature travel unencrypted.
        #[arg(long)]
        insecure_http: bool,
        /// Read the secret from the first line of standard input instead of generating 32 random
        /// bytes. It is not printed back.
        #[arg(long)]
        secret_stdin: bool,
        #[command(flatten)]
        doctypes: DocTypesDir,
    },
    /// Print the webhooks as a table: name, DocType, events, the scheme, host and port of the
    /// URL (never its path or query, which may hold a token), whether it is enabled and its two
    /// flags. Never a secret.
    List,
    /// Stop a webhook: no new delivery is enqueued, and queued ones are dropped when they run.
    Disable {
        /// The webhook name.
        name: String,
    },
    /// Enable a disabled webhook.
    Enable {
        /// The webhook name.
        name: String,
    },
    /// Delete a webhook. Queued deliveries are dropped when they run.
    Remove {
        /// The webhook name.
        name: String,
    },
    /// Give a webhook a new secret and print it once (or read it from standard input with
    /// --secret-stdin). Deliveries signed with the old one are no longer valid for the receiver
    /// once it switches: update the receiver first or accept both for a while.
    RotateSecret {
        /// The webhook name.
        name: String,
        /// Read the new secret from the first line of standard input.
        #[arg(long)]
        secret_stdin: bool,
    },
    /// Send one test delivery (`event` is `test`) now, signed and checked like a real one, without
    /// the queue. Prints the HTTP status of the answer, or why the delivery was refused. Exit
    /// code 0 for a 2xx answer, 1 otherwise. It works even if the webhook is disabled.
    Test {
        /// The webhook name.
        name: String,
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
    /// Take a workflow transition of a document and print it as stored (M8.1, ADR 0015): ACTION
    /// is the action of the transition from the document's current state, for example
    /// `Approve`. For a DocType with a workflow only; on one, plain `submit` and `cancel` are
    /// refused. Fails if the document's version is not the one given with --version. Like
    /// every `doc` command it runs as the trusted system context: the roles of the transition
    /// and the caller's rights are **not** checked (see --actor), but the `before_transition`
    /// hooks and the validation run.
    Transition {
        /// DocType name.
        doctype: String,
        /// Document name.
        name: String,
        /// The action, as written in the DocType's workflow (case-sensitive).
        action: String,
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

#[derive(Debug, Subcommand)]
pub enum DataCommand {
    /// Write the documents of a DocType to standard output (or --out) as CSV or JSON, in one
    /// consistent snapshot, in pages. Child tables are written to JSON only. Text that a
    /// spreadsheet could take for a formula gets a leading `'` in CSV (numbers never do); use
    /// JSON for exact text. An export of more rows than the limit (100000, see
    /// --limit-override) is refused before anything is written.
    ///
    /// --filter and --sort are those of `doc list`; there is no paging.
    Export {
        /// DocType name.
        doctype: String,
        #[command(flatten)]
        doctypes: DocTypesDir,
        /// The file format.
        #[arg(long, value_name = "FORMAT", value_parser = ["csv", "json"], default_value = "csv")]
        format: String,
        /// A JSON array of filters, as for `doc list`.
        #[arg(long, value_name = "JSON")]
        filter: Option<String>,
        /// Sort key `FIELD` or `FIELD:desc`; repeat for several. `name` is always the last key.
        #[arg(long, value_name = "FIELD[:desc]")]
        sort: Vec<String>,
        /// The keys to write, comma separated: field names and the standard columns `name`,
        /// `owner`, `modified_by`, `created_at`, `modified_at`, `version` (and `docstatus`,
        /// `workflow_state` where the DocType has them). Default: `name` and every field.
        #[arg(long, value_name = "FIELDS", value_delimiter = ',')]
        fields: Option<Vec<String>>,
        /// Write to this new file instead of standard output. An existing file is never
        /// overwritten, and a failed export leaves no partial file.
        #[arg(long, value_name = "FILE")]
        out: Option<PathBuf>,
        /// Allow this many rows instead of the default limit (100000).
        #[arg(long, value_name = "N")]
        limit_override: Option<u64>,
    },
    /// Import documents from a CSV or JSON file in one transaction: every row goes through the
    /// document pipeline (hooks, validation, naming, webhooks), each in a savepoint, and if any
    /// row has an error **nothing** is imported. Prints the summary as JSON (as the API's
    /// `data`), with the first 100 row errors; exit code 1 when any row failed. With --dry-run
    /// nothing is ever kept. At most 10000 rows.
    ///
    /// CSV: the first row is the header of field keys (a `name` column is allowed; child tables
    /// are not, use JSON); an empty cell is not given (insert) or set to null (upsert of an
    /// existing document). JSON: an array of objects as for `doc insert`, child rows as arrays.
    Import {
        /// DocType name.
        doctype: String,
        /// The file to read.
        file: PathBuf,
        #[command(flatten)]
        doctypes: DocTypesDir,
        /// The file format.
        #[arg(long, value_name = "FORMAT", value_parser = ["csv", "json"])]
        format: String,
        /// Update the documents that exist (by `name`) and insert the others, instead of
        /// inserting every row.
        #[arg(long)]
        upsert: bool,
        /// Check everything and report, but keep nothing.
        #[arg(long)]
        dry_run: bool,
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

#[derive(Debug, Subcommand)]
pub enum CustomizeCommand {
    /// Site custom fields: fields this site adds to a DocType of its own database. They are
    /// stored in the site (`fb_custom_field`), get a column at once, and are seen by every
    /// server and worker of the site within 10 seconds. Only adding is possible: a field cannot
    /// be changed or removed, only disabled (which hides it everywhere and keeps its column and
    /// data) and enabled again.
    #[command(subcommand)]
    Field(FieldCommand),
}

#[derive(Debug, Subcommand)]
pub enum FieldCommand {
    /// Add a custom field to a DocType. The field is one TOML table body, written like a
    /// `[[fields]]` entry of a DocType file (for example `name = "custom_vip"` and
    /// `type = "check"` on two lines). The name must start with `custom_`; the type is any
    /// except `table`; `required` and `read_only` are refused (existing documents have no value,
    /// and no hook can write the field); `unique` and `allow_on_submit` (on a submittable
    /// DocType) are allowed. The DocType must exist, must not be a child DocType and must have
    /// been migrated.
    Add {
        /// The DocType to add the field to.
        #[arg(long, value_name = "DOCTYPE")]
        doctype: String,
        /// The file with the field definition, or `-` to read it from standard input.
        #[arg(long, value_name = "FILE")]
        field: String,
        #[command(flatten)]
        doctypes: DocTypesDir,
    },
    /// Print the custom fields as a table: DocType, name, type, label, whether enabled and when
    /// and by whom they were last changed.
    List {
        /// Only the fields of this DocType.
        #[arg(long, value_name = "DOCTYPE")]
        doctype: Option<String>,
    },
    /// Hide a custom field everywhere (the API, the desk, filters, export, import). Its column
    /// and its data are kept.
    Disable {
        /// The DocType of the field.
        #[arg(long, value_name = "DOCTYPE")]
        doctype: String,
        /// The field name.
        name: String,
    },
    /// Show a disabled custom field again, with its data.
    Enable {
        /// The DocType of the field.
        #[arg(long, value_name = "DOCTYPE")]
        doctype: String,
        /// The field name.
        name: String,
    },
}

#[derive(Debug, Args)]
pub struct DocTypesDir {
    /// Directory with one DocType `*.toml` file per DocType (not searched recursively). It
    /// becomes the app `local`, which depends on the apps installed on the site (every app of
    /// the binary unless the site lists `apps`) and is part of every site's registry. Required
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
    /// modified_by on insert, update, submit, cancel, amend and transition (and in the workflow log); given to hooks (on delete it is
    /// only given to hooks, not stored). Not authenticated, and no permission rules apply to it.
    #[arg(long, value_name = "NAME", default_value = "cli")]
    pub actor: String,
}
