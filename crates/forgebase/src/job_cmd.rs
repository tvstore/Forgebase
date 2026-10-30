//! The `job` commands: list, show, retry and enqueue (ADR 0013). Operators use the command line
//! only; there is no HTTP or desk interface for jobs.

use std::fmt::Write as _;

use forgebase_core::meta::Registry;
use forgebase_db::DbError;
use forgebase_db::jobs::{self, Job, JobFilter, RetryResult};
use time::OffsetDateTime;
use time::format_description::well_known::Rfc3339;

use crate::args::JobCommand;
use crate::config::SiteContext;
use crate::error::CliError;
use crate::run::{read_data, write_stdout};

/// How many characters of the last error `job list` shows.
const ERROR_CHARS: usize = 40;

/// The payload of `job enqueue` (read from standard input for `-`) as JSON; `None` for the other
/// commands. Called before the site's pool is opened, so a bad payload never touches the
/// database.
pub(crate) fn read_payload(command: &JobCommand) -> Result<Option<serde_json::Value>, CliError> {
    match command {
        JobCommand::Enqueue { payload, .. } => Ok(Some(parse_payload(&read_data(payload)?)?)),
        _ => Ok(None),
    }
}

/// Runs a `job` command; `payload` is what [`read_payload`] returned. The site's registry decides
/// which jobs are known: the jobs of the apps installed on the site, and the built-in ones.
pub(crate) async fn job(
    site: &SiteContext,
    command: JobCommand,
    payload: Option<serde_json::Value>,
) -> Result<(), CliError> {
    let registry: &Registry = &site.registry;
    let client = site.pool.get().await.map_err(DbError::from)?;
    let client = &**client;
    match command {
        JobCommand::List {
            status,
            name,
            limit,
        } => {
            let filter = JobFilter {
                status: status.as_deref(),
                name: name.as_deref(),
            };
            let found = jobs::list(client, &filter, i64::from(limit)).await?;
            write_stdout(&table(&found))
        }
        JobCommand::Show { id } => match jobs::get(client, &id).await? {
            Some(job) => {
                let text = serde_json::to_string_pretty(&job_json(&job))
                    .map_err(|e| CliError::Job(e.to_string()))?;
                write_stdout(&format!("{text}\n"))
            }
            None => Err(CliError::Job(format!("no job with id {id}"))),
        },
        JobCommand::Retry { id } => match jobs::retry_dead(client, &id).await? {
            RetryResult::Queued => write_stdout(&format!("job {id} is queued again\n")),
            RetryResult::NotFound => Err(CliError::Job(format!("no job with id {id}"))),
            RetryResult::NotDead(status) => Err(CliError::Job(format!(
                "job {id} is {status}: only a dead job can be retried"
            ))),
        },
        JobCommand::Enqueue { name, actor, .. } => {
            let payload = payload.unwrap_or_default();
            let id = jobs::enqueue(client, registry, &name, &payload, &actor.actor)
                .await
                .map_err(|e| match e {
                    DbError::Job(message) => CliError::Job(message),
                    other => other.into(),
                })?;
            write_stdout(&format!("enqueued job {id}\n"))
        }
    }
}

fn parse_payload(text: &str) -> Result<serde_json::Value, CliError> {
    serde_json::from_str(text.trim())
        .map_err(|e| CliError::Job(format!("the payload is not valid JSON: {e}")))
}

fn time_text(time: OffsetDateTime) -> String {
    time.format(&Rfc3339).unwrap_or_else(|_| time.to_string())
}

fn job_json(job: &Job) -> serde_json::Value {
    serde_json::json!({
        "id": job.id.to_string(),
        "name": job.name,
        "status": job.status,
        "attempts": job.attempts,
        "run_at": time_text(job.run_at),
        "locked_until": job.locked_until.map(time_text),
        "locked_by": job.locked_by,
        "last_error": job.last_error,
        "enqueued_by": job.enqueued_by,
        "created_at": time_text(job.created_at),
        "finished_at": job.finished_at.map(time_text),
        "payload": job.payload,
    })
}

/// The start of an error on one line.
fn short(error: &str) -> String {
    let line: String = error
        .chars()
        .map(|c| if c.is_control() { ' ' } else { c })
        .collect();
    if line.chars().count() > ERROR_CHARS {
        let mut cut: String = line.chars().take(ERROR_CHARS - 1).collect();
        cut.push('…');
        cut
    } else {
        line
    }
}

fn table(jobs: &[Job]) -> String {
    let header = ["ID", "NAME", "STATUS", "ATTEMPTS", "RUN_AT", "LAST_ERROR"];
    let rows: Vec<[String; 6]> = jobs
        .iter()
        .map(|job| {
            [
                job.id.to_string(),
                job.name.clone(),
                job.status.clone(),
                job.attempts.to_string(),
                time_text(job.run_at),
                job.last_error.as_deref().map(short).unwrap_or_default(),
            ]
        })
        .collect();
    let widths: Vec<usize> = (0..header.len())
        .map(|i| {
            rows.iter()
                .map(|row| row[i].chars().count())
                .chain([header[i].len()])
                .max()
                .unwrap_or(0)
        })
        .collect();
    let mut text = String::new();
    let mut line = |cells: &[&str]| {
        let mut out = String::new();
        for (i, cell) in cells.iter().enumerate() {
            let _ = write!(out, "{cell:<width$}  ", width = widths[i]);
        }
        text.push_str(out.trim_end());
        text.push('\n');
    };
    line(&header);
    for row in &rows {
        line(&row.each_ref().map(String::as_str));
    }
    text
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_error_is_shortened_to_one_line() {
        assert_eq!(short("a\nb"), "a b");
        let long = "x".repeat(100);
        let cut = short(&long);
        assert_eq!(cut.chars().count(), ERROR_CHARS);
        assert!(cut.ends_with('…'));
    }

    #[test]
    fn the_payload_must_be_json() {
        assert_eq!(parse_payload(" {\"a\": 1}\n").expect("json")["a"], 1);
        assert!(parse_payload("{").is_err());
    }
}
