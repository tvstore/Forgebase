//! Executes the commands.

use std::io::{BufRead, Read, Write};
use std::time::Duration;

use forgebase_core::document::{Changes, Document};
use forgebase_core::meta::{DocType, Registry};
use forgebase_core::permission::Caller;
use forgebase_db::auth;
use forgebase_db::{DbError, ListQuery, StoredDocument, SyncOptions};

use crate::args::{
    ActorArg, Command, DocCommand, DocTypesDir, TokenCommand, UserCommand, VersionArg,
};
use crate::doctypes::load_dir;
use crate::error::CliError;
use crate::json::{
    Input, document_to_json, documents_to_json, parse_filters, parse_input, parse_sort,
};
use crate::report::migrate_report;

/// The system actor of commands that have no `--actor` (`doc get`, `doc list`): they only read.
const DEFAULT_ACTOR: &str = "cli";

/// Environment variable with the PostgreSQL URL.
const DATABASE_URL_VAR: &str = "FORGEBASE_DATABASE_URL";

/// Runs one command.
pub async fn run(command: Command) -> Result<(), CliError> {
    let url = database_url()?;
    match command {
        Command::Migrate {
            doctypes,
            dry_run,
            lock_timeout_ms,
        } => {
            let registry = registry(&doctypes)?;
            let pool = forgebase_db::connect(&url).await?;
            let options = SyncOptions {
                dry_run,
                lock_timeout: Duration::from_millis(lock_timeout_ms),
            };
            let report = forgebase_db::migrate(&pool, &registry, &options).await?;
            write_stdout(&migrate_report(&report, dry_run))
        }
        Command::Doc(command) => execute(&url, prepare(command)?).await,
        Command::User(command) => user(&url, command).await,
        Command::Token(command) => token(&url, command).await,
    }
}

/// A document operation with its input already read and checked, so that nothing touches the
/// database when the DocType, the JSON or standard input is wrong.
enum Operation {
    Insert {
        registry: Registry,
        doctype: String,
        input: Input,
        actor: String,
    },
    Get {
        registry: Registry,
        doctype: String,
        name: String,
    },
    List {
        registry: Registry,
        doctype: String,
        query: ListQuery,
    },
    Update {
        registry: Registry,
        doctype: String,
        name: String,
        expected_version: i64,
        input: Input,
        actor: String,
    },
    Delete {
        registry: Registry,
        doctype: String,
        name: String,
        expected_version: i64,
        actor: String,
    },
    /// Submit, cancel or amend: a document operation without data.
    Status {
        kind: StatusKind,
        registry: Registry,
        doctype: String,
        name: String,
        expected_version: i64,
        actor: String,
    },
}

#[derive(Clone, Copy)]
enum StatusKind {
    Submit,
    Cancel,
    Amend,
}

fn prepare(command: DocCommand) -> Result<Operation, CliError> {
    Ok(match command {
        DocCommand::Insert {
            doctype,
            doctypes,
            data,
            actor,
        } => {
            let (registry, found) = find_doctype(&doctypes, &doctype)?;
            let input = parse_input(&registry, &found, &read_data(&data.data)?)?;
            Operation::Insert {
                registry,
                doctype: found.name,
                input,
                actor: actor.actor,
            }
        }
        DocCommand::Get {
            doctype,
            name,
            doctypes,
        } => {
            let (registry, found) = find_doctype(&doctypes, &doctype)?;
            Operation::Get {
                registry,
                doctype: found.name,
                name,
            }
        }
        DocCommand::List {
            doctype,
            doctypes,
            filter,
            sort,
            limit,
            offset,
        } => {
            let (registry, found) = find_doctype(&doctypes, &doctype)?;
            let query = ListQuery {
                filters: match filter {
                    Some(text) => parse_filters(&found, &text)?,
                    None => Vec::new(),
                },
                sort: parse_sort(&sort)?,
                limit,
                offset,
            };
            Operation::List {
                registry,
                doctype: found.name,
                query,
            }
        }
        DocCommand::Update {
            doctype,
            name,
            version,
            doctypes,
            data,
            actor,
        } => {
            let (registry, found) = find_doctype(&doctypes, &doctype)?;
            let input = parse_input(&registry, &found, &read_data(&data.data)?)?;
            Operation::Update {
                registry,
                doctype: found.name,
                name,
                expected_version: version.expected_version,
                input,
                actor: actor.actor,
            }
        }
        DocCommand::Submit {
            doctype,
            name,
            version,
            doctypes,
            actor,
        } => status(StatusKind::Submit, doctype, name, version, doctypes, actor)?,
        DocCommand::Cancel {
            doctype,
            name,
            version,
            doctypes,
            actor,
        } => status(StatusKind::Cancel, doctype, name, version, doctypes, actor)?,
        DocCommand::Amend {
            doctype,
            name,
            version,
            doctypes,
            actor,
        } => status(StatusKind::Amend, doctype, name, version, doctypes, actor)?,
        DocCommand::Delete {
            doctype,
            name,
            version,
            doctypes,
            actor,
        } => {
            let (registry, found) = find_doctype(&doctypes, &doctype)?;
            Operation::Delete {
                registry,
                doctype: found.name,
                name,
                expected_version: version.expected_version,
                actor: actor.actor,
            }
        }
    })
}

fn status(
    kind: StatusKind,
    doctype: String,
    name: String,
    version: VersionArg,
    doctypes: DocTypesDir,
    actor: ActorArg,
) -> Result<Operation, CliError> {
    let (registry, found) = find_doctype(&doctypes, &doctype)?;
    Ok(Operation::Status {
        kind,
        registry,
        doctype: found.name,
        name,
        expected_version: version.expected_version,
        actor: actor.actor,
    })
}

async fn execute(url: &str, operation: Operation) -> Result<(), CliError> {
    let pool = forgebase_db::connect(url).await?;
    let mut client = pool.get().await.map_err(DbError::from)?;
    let client = &mut **client;
    let stored = match operation {
        Operation::List {
            registry,
            doctype,
            query,
        } => {
            let caller = Caller::system(DEFAULT_ACTOR);
            let documents =
                forgebase_db::list(&*client, &registry, &doctype, &caller, &query).await?;
            let json = documents_to_json(&documents)?;
            let text =
                serde_json::to_string_pretty(&json).map_err(crate::json::JsonError::Serialize)?;
            return write_stdout(&format!("{text}\n"));
        }
        Operation::Insert {
            registry,
            doctype,
            input,
            actor,
        } => {
            let document = Document {
                name: None,
                values: input.values,
                tables: input.tables,
            };
            forgebase_db::insert(
                client,
                &registry,
                &doctype,
                document,
                &Caller::system(actor),
            )
            .await?
        }
        Operation::Get {
            registry,
            doctype,
            name,
        } => forgebase_db::get(
            client,
            &registry,
            &doctype,
            &name,
            &Caller::system(DEFAULT_ACTOR),
        )
        .await?
        .ok_or(DbError::NotFound { doctype, name })?,
        Operation::Update {
            registry,
            doctype,
            name,
            expected_version,
            input,
            actor,
        } => {
            let changes = Changes {
                values: input.values,
                tables: input.tables,
            };
            forgebase_db::update(
                client,
                &registry,
                &doctype,
                &name,
                expected_version,
                &changes,
                &Caller::system(actor),
            )
            .await?
        }
        Operation::Status {
            kind,
            registry,
            doctype,
            name,
            expected_version,
            actor,
        } => {
            let doctype = doctype.as_str();
            let name = name.as_str();
            let caller = Caller::system(actor);
            match kind {
                StatusKind::Submit => {
                    forgebase_db::submit(
                        client,
                        &registry,
                        doctype,
                        name,
                        expected_version,
                        &caller,
                    )
                    .await?
                }
                StatusKind::Cancel => {
                    forgebase_db::cancel(
                        client,
                        &registry,
                        doctype,
                        name,
                        expected_version,
                        &caller,
                    )
                    .await?
                }
                StatusKind::Amend => {
                    forgebase_db::amend(client, &registry, doctype, name, expected_version, &caller)
                        .await?
                }
            }
        }
        Operation::Delete {
            registry,
            doctype,
            name,
            expected_version,
            actor,
        } => {
            return Ok(forgebase_db::delete(
                client,
                &registry,
                &doctype,
                &name,
                expected_version,
                &Caller::system(actor),
            )
            .await?);
        }
    };
    print_document(&stored)
}

/// Runs a `user` command. The password (if any) is read from standard input before the database
/// is touched, and never printed.
async fn user(url: &str, command: UserCommand) -> Result<(), CliError> {
    let password = match &command {
        UserCommand::Create { .. } | UserCommand::SetPassword { .. } => Some(read_password()?),
        _ => None,
    };
    let pool = forgebase_db::connect(url).await?;
    let mut client = pool.get().await.map_err(DbError::from)?;
    let client = &mut **client;
    let password = password.as_deref().unwrap_or_default();
    let text = match command {
        UserCommand::Create { username, roles } => {
            let roles: Vec<&str> = roles.iter().map(String::as_str).collect();
            auth::create_user(client, &username, password, &roles).await?;
            format!("created user {username}\n")
        }
        UserCommand::SetPassword { username } => {
            auth::set_password(client, &username, password).await?;
            format!("changed the password of user {username}; its sessions were ended\n")
        }
        UserCommand::AddRole { username, role } => {
            if auth::add_role(client, &username, &role).await? {
                format!("added role {role} to user {username}\n")
            } else {
                format!("user {username} has role {role} already\n")
            }
        }
        UserCommand::RemoveRole { username, role } => {
            if auth::remove_role(client, &username, &role).await? {
                format!("removed role {role} from user {username}\n")
            } else {
                format!("user {username} does not have role {role}\n")
            }
        }
        UserCommand::Disable { username } => {
            auth::set_enabled(client, &username, false).await?;
            format!("disabled user {username}; its sessions were ended\n")
        }
        UserCommand::Enable { username } => {
            auth::set_enabled(client, &username, true).await?;
            format!("enabled user {username}\n")
        }
    };
    write_stdout(&text)
}

/// Runs a `token` command. `create` prints `key:secret` and nothing else.
async fn token(url: &str, command: TokenCommand) -> Result<(), CliError> {
    let pool = forgebase_db::connect(url).await?;
    let client = pool.get().await.map_err(DbError::from)?;
    let client = &**client;
    match command {
        TokenCommand::Create { username } => {
            let token = auth::create_api_token(client, &username).await?;
            write_stdout(&format!("{}:{}\n", token.key, token.secret))
        }
        TokenCommand::Revoke { key } => {
            auth::revoke_api_token(client, &key).await?;
            write_stdout(&format!("revoked API token {key}\n"))
        }
    }
}

/// The first line of standard input without its line ending. At most 4096 bytes are read, far
/// more than the longest valid password, so a runaway input cannot fill memory.
fn read_password() -> Result<String, CliError> {
    let mut line = String::new();
    std::io::stdin()
        .lock()
        .take(4096)
        .read_line(&mut line)
        .map_err(CliError::Stdin)?;
    while line.ends_with(['\n', '\r']) {
        line.pop();
    }
    Ok(line)
}

fn database_url() -> Result<String, CliError> {
    match std::env::var(DATABASE_URL_VAR) {
        Ok(url) if !url.is_empty() => Ok(url),
        _ => Err(CliError::MissingDatabaseUrl(DATABASE_URL_VAR)),
    }
}

fn registry(dir: &DocTypesDir) -> Result<Registry, CliError> {
    load_dir(&dir.doctypes).map_err(CliError::DocTypes)
}

/// The registry of the directory and the DocType called `name` in it.
fn find_doctype(dir: &DocTypesDir, name: &str) -> Result<(Registry, DocType), CliError> {
    let registry = registry(dir)?;
    match registry.get(name) {
        Some(doctype) => {
            let doctype = doctype.clone();
            Ok((registry, doctype))
        }
        None => Err(CliError::UnknownDocType {
            name: name.to_owned(),
            known: if registry.is_empty() {
                "none".to_owned()
            } else {
                registry
                    .iter()
                    .map(|d| d.name.as_str())
                    .collect::<Vec<_>>()
                    .join(", ")
            },
        }),
    }
}

/// The JSON text of `--data`: the argument itself, or standard input for `-`.
fn read_data(arg: &str) -> Result<String, CliError> {
    if arg != "-" {
        return Ok(arg.to_owned());
    }
    let mut text = String::new();
    std::io::stdin()
        .read_to_string(&mut text)
        .map_err(CliError::Stdin)?;
    Ok(text)
}

fn print_document(stored: &StoredDocument) -> Result<(), CliError> {
    let json = document_to_json(stored)?;
    // Serializing a `serde_json::Value` cannot fail (string keys only).
    let text = serde_json::to_string_pretty(&json).map_err(crate::json::JsonError::Serialize)?;
    write_stdout(&format!("{text}\n"))
}

/// Writes to standard output without panicking on a closed pipe.
fn write_stdout(text: &str) -> Result<(), CliError> {
    let mut out = std::io::stdout().lock();
    out.write_all(text.as_bytes())
        .and_then(|()| out.flush())
        .map_err(CliError::Output)
}
