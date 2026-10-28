//! Executes the commands.

use std::collections::BTreeMap;
use std::io::{Read, Write};
use std::time::Duration;

use forgebase_core::document::Document;
use forgebase_core::meta::{DocType, Registry};
use forgebase_core::value::Value;
use forgebase_db::{DbError, StoredDocument, SyncOptions};

use crate::args::{Command, DocCommand, DocTypesDir};
use crate::doctypes::load_dir;
use crate::error::CliError;
use crate::json::{document_to_json, parse_values};
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
        doctype: DocType,
        values: BTreeMap<String, Value>,
        actor: String,
    },
    Get {
        doctype: DocType,
        name: String,
    },
    Update {
        doctype: DocType,
        name: String,
        expected_version: i64,
        changes: BTreeMap<String, Value>,
        actor: String,
    },
    Delete {
        doctype: DocType,
        name: String,
        expected_version: i64,
    },
}

fn prepare(command: DocCommand) -> Result<Operation, CliError> {
    Ok(match command {
        DocCommand::Insert {
            doctype,
            doctypes,
            data,
            actor,
        } => {
            let doctype = find_doctype(&doctypes, &doctype)?;
            let values = parse_values(&doctype, &read_data(&data.data)?)?;
            Operation::Insert {
                doctype,
                values,
                actor: actor.actor,
            }
        }
        DocCommand::Get {
            doctype,
            name,
            doctypes,
        } => Operation::Get {
            doctype: find_doctype(&doctypes, &doctype)?,
            name,
        },
        DocCommand::Update {
            doctype,
            name,
            version,
            doctypes,
            data,
            actor,
        } => {
            let doctype = find_doctype(&doctypes, &doctype)?;
            let changes = parse_values(&doctype, &read_data(&data.data)?)?;
            Operation::Update {
                doctype,
                name,
                expected_version: version.expected_version,
                changes,
                actor: actor.actor,
            }
        }
        DocCommand::Delete {
            doctype,
            name,
            version,
            doctypes,
        } => Operation::Delete {
            doctype: find_doctype(&doctypes, &doctype)?,
            name,
            expected_version: version.expected_version,
        },
    })
}

async fn execute(url: &str, operation: Operation) -> Result<(), CliError> {
    let pool = forgebase_db::connect(url).await?;
    let client = pool.get().await.map_err(DbError::from)?;
    let client = &**client;
    let stored = match operation {
        Operation::Insert {
            doctype,
            values,
            actor,
        } => {
            let document = Document { name: None, values };
            forgebase_db::insert(client, &doctype, document, &actor).await?
        }
        Operation::Get { doctype, name } => forgebase_db::get(client, &doctype, &name)
            .await?
            .ok_or(DbError::NotFound {
                doctype: doctype.name.clone(),
                name,
            })?,
        Operation::Update {
            doctype,
            name,
            expected_version,
            changes,
            actor,
        } => {
            forgebase_db::update(client, &doctype, &name, expected_version, &changes, &actor)
                .await?
        }
        Operation::Delete {
            doctype,
            name,
            expected_version,
        } => {
            return Ok(forgebase_db::delete(client, &doctype, &name, expected_version).await?);
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

fn find_doctype(dir: &DocTypesDir, name: &str) -> Result<DocType, CliError> {
    let registry = registry(dir)?;
    match registry.get(name) {
        Some(doctype) => Ok(doctype.clone()),
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
