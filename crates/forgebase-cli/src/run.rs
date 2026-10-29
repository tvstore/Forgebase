//! Executes the commands.

use std::io::{Read, Write};
use std::time::Duration;

use forgebase_core::document::{Changes, Document};
use forgebase_core::meta::{DocType, Registry};
use forgebase_db::{DbError, StoredDocument, SyncOptions};

use crate::args::{ActorArg, Command, DocCommand, DocTypesDir, VersionArg};
use crate::doctypes::load_dir;
use crate::error::CliError;
use crate::json::{Input, document_to_json, parse_input};
use crate::report::migrate_report;

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
            forgebase_db::insert(client, &registry, &doctype, document, &actor).await?
        }
        Operation::Get {
            registry,
            doctype,
            name,
        } => forgebase_db::get(client, &registry, &doctype, &name)
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
                &actor,
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
            let args = (&registry, doctype.as_str(), name.as_str(), expected_version);
            match kind {
                StatusKind::Submit => {
                    forgebase_db::submit(client, args.0, args.1, args.2, args.3, &actor).await?
                }
                StatusKind::Cancel => {
                    forgebase_db::cancel(client, args.0, args.1, args.2, args.3, &actor).await?
                }
                StatusKind::Amend => {
                    forgebase_db::amend(client, args.0, args.1, args.2, args.3, &actor).await?
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
                &actor,
            )
            .await?);
        }
    };
    print_document(&stored)
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
