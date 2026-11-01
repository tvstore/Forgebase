//! The `data` commands (M9.3, ADR 0021): `export` and `import` of documents as CSV or JSON, as
//! the trusted system context. The formats, the checks and the transaction are those of the
//! HTTP API (`forgebase_http::transfer`, `forgebase_db::import`), so a file means the same on
//! both.

use std::io::Write;
use std::path::Path;

use forgebase_core::meta::{DocType, Registry};
use forgebase_core::permission::Caller;
use forgebase_db::{DbError, Export, ExportQuery, ImportMode, ListQuery, begin_export};
use forgebase_http::AppConfig;
use forgebase_http::json::{parse_filters, parse_sort};
use forgebase_http::transfer::{Exporter, FileError, Format, parse_import, run_import};

use crate::args::DataCommand;
use crate::config::SiteContext;
use crate::error::CliError;
use crate::run::write_stdout;

/// The system actor of `data export`, which only reads.
const EXPORT_ACTOR: &str = "cli";

/// A `data` command with its input checked: nothing touches the database when the DocType, the
/// filter, the fields or the file is wrong.
pub(crate) enum Prepared {
    Export {
        doctype: String,
        filters: Vec<forgebase_db::Filter>,
        sort: Vec<forgebase_db::Sort>,
        exporter: Exporter,
        out: Option<std::path::PathBuf>,
        limit: u64,
    },
    Import {
        doctype: String,
        parsed: forgebase_http::transfer::ParsedImport,
        mode: ImportMode,
        dry_run: bool,
        actor: String,
    },
}

fn format(text: &str) -> Result<Format, CliError> {
    // clap's value parser has checked it already.
    Format::parse(text).ok_or_else(|| CliError::Transfer(format!("unknown format `{text}`")))
}

/// Checks a `data` command against the DocType and reads its file.
pub(crate) fn prepare(
    command: DataCommand,
    registry: &Registry,
    found: &DocType,
) -> Result<Prepared, CliError> {
    let defaults = AppConfig::default();
    match command {
        DataCommand::Export {
            format: format_name,
            filter,
            sort,
            fields,
            out,
            limit_override,
            ..
        } => {
            let query = ListQuery {
                filters: match filter {
                    Some(text) => parse_filters(found, &text)?,
                    None => Vec::new(),
                },
                sort: parse_sort(&sort)?,
                limit: None,
                offset: None,
            };
            Ok(Prepared::Export {
                doctype: found.name.clone(),
                filters: query.filters,
                sort: query.sort,
                exporter: Exporter::new(found, format(&format_name)?, fields.as_deref())?,
                out,
                limit: limit_override.unwrap_or(defaults.export_max_rows),
            })
        }
        DataCommand::Import {
            file,
            format: format_name,
            upsert,
            dry_run,
            actor,
            ..
        } => {
            let bytes = std::fs::read(&file)
                .map_err(|e| CliError::Transfer(format!("cannot read {}: {e}", file.display())))?;
            let mode = if upsert {
                ImportMode::Upsert
            } else {
                ImportMode::Insert
            };
            let parsed = parse_import(
                registry,
                found,
                format(&format_name)?,
                mode,
                &bytes,
                defaults.import_max_rows,
            )
            .map_err(|error| match error {
                FileError::Invalid(problems) => CliError::Transfer(format!(
                    "the file cannot be imported:\n{}",
                    crate::error::bullets(&problems)
                )),
                FileError::TooLarge { limit } => {
                    CliError::Transfer(format!("the file has more than {limit} rows; split it"))
                }
            })?;
            Ok(Prepared::Import {
                doctype: found.name.clone(),
                parsed,
                mode,
                dry_run,
                actor: actor.actor,
            })
        }
    }
}

/// Runs a prepared `data` command.
pub(crate) async fn execute(site: &SiteContext, prepared: Prepared) -> Result<(), CliError> {
    match prepared {
        Prepared::Export {
            doctype,
            filters,
            sort,
            exporter,
            out,
            limit,
        } => {
            export(
                site,
                &doctype,
                &filters,
                &sort,
                exporter,
                out.as_deref(),
                limit,
            )
            .await
        }
        Prepared::Import {
            doctype,
            parsed,
            mode,
            dry_run,
            actor,
        } => import(site, &doctype, parsed, mode, dry_run, &actor).await,
    }
}

/// Where an export is written: standard output or a new file (removed again if the export fails).
enum Sink {
    Stdout,
    File(std::io::BufWriter<std::fs::File>),
}

impl Sink {
    fn write(&mut self, text: &str) -> Result<(), CliError> {
        match self {
            Sink::Stdout => write_stdout(text),
            Sink::File(file) => file.write_all(text.as_bytes()).map_err(CliError::Output),
        }
    }
}

async fn export(
    site: &SiteContext,
    doctype: &str,
    filters: &[forgebase_db::Filter],
    sort: &[forgebase_db::Sort],
    mut exporter: Exporter,
    out: Option<&Path>,
    limit: u64,
) -> Result<(), CliError> {
    let registry = &*site.registry;
    let mut client = site.pool.get().await.map_err(DbError::from)?;
    let tx = begin_export(&mut client).await?;
    let query = ExportQuery {
        doctype,
        filters,
        sort,
        limit,
        with_tables: exporter.with_tables(),
    };
    let mut export = Export::start(&tx, registry, &Caller::system(EXPORT_ACTOR), &query).await?;
    let mut sink = match out {
        None => Sink::Stdout,
        Some(path) => Sink::File(std::io::BufWriter::new(
            std::fs::OpenOptions::new()
                .write(true)
                .create_new(true)
                .open(path)
                .map_err(|e| {
                    CliError::Transfer(format!("cannot create {}: {e}", path.display()))
                })?,
        )),
    };
    let result = write_pages(&mut sink, &mut export, &mut exporter, &tx, registry).await;
    if let Sink::File(file) = &mut sink
        && result.is_ok()
    {
        file.flush().map_err(CliError::Output)?;
    }
    drop(sink);
    match (result, out) {
        (Ok(()), Some(path)) => write_stdout(&format!(
            "wrote {} row(s) to {}\n",
            export.count(),
            path.display()
        )),
        (Ok(()), None) => Ok(()),
        (Err(error), path) => {
            if let Some(path) = path {
                let _ = std::fs::remove_file(path);
            }
            Err(error)
        }
    }
}

async fn write_pages(
    sink: &mut Sink,
    export: &mut Export,
    exporter: &mut Exporter,
    tx: &forgebase_db::Transaction<'_>,
    registry: &Registry,
) -> Result<(), CliError> {
    sink.write(&exporter.start())?;
    while let Some(page) = export.next_page(tx, registry).await? {
        sink.write(&exporter.rows(&page)?)?;
    }
    sink.write(&exporter.finish())
}

async fn import(
    site: &SiteContext,
    doctype: &str,
    parsed: forgebase_http::transfer::ParsedImport,
    mode: ImportMode,
    dry_run: bool,
    actor: &str,
) -> Result<(), CliError> {
    let mut client = site.pool.get().await.map_err(DbError::from)?;
    let summary = run_import(
        &mut **client,
        &site.registry,
        doctype,
        &Caller::system(actor),
        parsed,
        mode,
        dry_run,
    )
    .await?;
    let text = serde_json::to_string_pretty(&summary.to_json())
        .map_err(forgebase_http::json::JsonError::Serialize)?;
    write_stdout(&format!("{text}\n"))?;
    if summary.error_count > 0 {
        return Err(CliError::Transfer(format!(
            "{} row(s) have errors{}",
            summary.error_count,
            if dry_run {
                " (dry run: nothing was imported)"
            } else {
                "; nothing was imported"
            }
        )));
    }
    Ok(())
}
