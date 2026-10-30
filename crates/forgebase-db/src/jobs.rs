//! The job queue: storage and SQL only (ADR 0013). This module never runs a handler; the worker
//! in the `forgebase` crate does.
//!
//! A job is a row of `fb_job` (migration 0005). The functions here are the whole life of a row:
//!
//! - [`enqueue`] inserts a job in the transaction it is given. A hook's jobs reach the table
//!   through the same code, in the transaction of the document operation (the outbox).
//! - [`claim`] takes the next due job for a worker: a queued job that is due, or a running job
//!   whose lease expired (its worker died). It sets the lease and counts the attempt.
//! - [`complete`], [`fail`] and [`release`] end an attempt. All three are **guarded**: they change
//!   the row only if it is still `running`, locked by this worker and on this attempt. A worker
//!   that lost its lease (it was too slow, and the job was claimed again) therefore changes
//!   nothing, and the caller learns it from the `false` it gets back.
//! - [`list`], [`get`] and [`retry_dead`] serve the command line.
//!
//! Every time is the database's `now()`, never a clock of the worker, so workers with different
//! clocks agree. Payloads are bound as text and cast to `jsonb` in SQL (no extra feature of the
//! database driver).

use std::time::Duration;

use forgebase_core::hooks::{EnqueuedJob, check_job_payload};
use forgebase_core::meta::Registry;
use time::OffsetDateTime;
use tokio_postgres::{Client, GenericClient};
use uuid::Uuid;

use crate::error::DbError;

/// A job row as stored.
#[derive(Debug, Clone, PartialEq)]
pub struct Job {
    /// The id (UUID v7).
    pub id: Uuid,
    /// The full name, `<app>.<job>`.
    pub name: String,
    /// The payload.
    pub payload: serde_json::Value,
    /// `queued`, `running`, `done` or `dead`.
    pub status: String,
    /// How many times a worker has claimed the job.
    pub attempts: i32,
    /// When the job is due (the next try, for a retry).
    pub run_at: OffsetDateTime,
    /// The end of the lease of a running job.
    pub locked_until: Option<OffsetDateTime>,
    /// The worker that holds or held the job.
    pub locked_by: Option<String>,
    /// The error of the last failed attempt.
    pub last_error: Option<String>,
    /// The actor that caused the job.
    pub enqueued_by: String,
    /// When the job was enqueued.
    pub created_at: OffsetDateTime,
    /// When the job was done or died.
    pub finished_at: Option<OffsetDateTime>,
}

/// A job a worker has claimed.
#[derive(Debug, Clone, PartialEq)]
pub struct Claimed {
    /// The id.
    pub id: Uuid,
    /// The full name.
    pub name: String,
    /// The payload.
    pub payload: serde_json::Value,
    /// This attempt, counted from 1 (the stored `attempts` after the claim).
    pub attempt: i32,
}

/// A job name a worker knows, with the length of the lease it takes on a job of that name.
#[derive(Debug, Clone, Copy)]
pub struct Known<'a> {
    /// The full job name.
    pub name: &'a str,
    /// The lease: the job's timeout plus a margin.
    pub lease: Duration,
}

/// What [`fail`] does with the job.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Failure {
    /// Try again after this long.
    Retry(Duration),
    /// Give up: the job is `dead`.
    Dead,
}

/// What [`retry_dead`] found.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RetryResult {
    /// The dead job is queued again.
    Queued,
    /// No job has this id.
    NotFound,
    /// The job is not dead, so it is not retried; the status says what it is.
    NotDead(String),
}

/// The most characters of an error message that are stored.
pub const MAX_ERROR_CHARS: usize = 2000;

const COLUMNS: &str = "id, name, payload::text, status, attempts, run_at, locked_until, \
    locked_by, last_error, enqueued_by, created_at, finished_at";

/// Inserts a job and returns its id. `name` must be a job of `registry`, and the payload must
/// pass [`check_job_payload`] (64 KiB of JSON); otherwise [`DbError::Job`] and nothing is
/// written. `enqueued_by` must be a valid actor (non-empty, no NUL), else
/// [`DbError::InvalidActor`]. It runs in the transaction of `client`.
pub async fn enqueue(
    client: &impl GenericClient,
    registry: &Registry,
    name: &str,
    payload: &serde_json::Value,
    enqueued_by: &str,
) -> Result<Uuid, DbError> {
    crate::document::check_actor(enqueued_by)?;
    if !registry.has_job(name) {
        return Err(DbError::Job(format!(
            "unknown job `{name}` (jobs are registered by the apps of the binary as `<app>.<job>`)"
        )));
    }
    check_job_payload(payload).map_err(DbError::Job)?;
    let id = Uuid::now_v7();
    let text = payload.to_string();
    client
        .execute(
            "INSERT INTO fb_job (id, name, payload, status, enqueued_by) \
             VALUES ($1, $2, $3::text::jsonb, 'queued', $4)",
            &[&id, &name, &text, &enqueued_by],
        )
        .await?;
    Ok(id)
}

/// Writes the jobs that the hooks of one document operation collected, in the operation's
/// transaction (the outbox). The first problem ([`DbError::Job`]) fails the operation.
pub(crate) async fn enqueue_collected(
    client: &impl GenericClient,
    registry: &Registry,
    jobs: Vec<EnqueuedJob>,
    enqueued_by: &str,
) -> Result<(), DbError> {
    for job in jobs {
        enqueue(client, registry, job.name(), job.payload(), enqueued_by).await?;
    }
    Ok(())
}

/// Claims the next job a worker can run, or `None`. One statement, so it needs no transaction.
///
/// A job is eligible if its name is in `known` and it is either queued and due (`run_at <=
/// now()`), or running with an expired lease (the worker died). The job with the oldest
/// `run_at` goes first, and `FOR UPDATE SKIP LOCKED` lets concurrent workers pass over a row that
/// another worker is claiming. The claim sets `status = 'running'`, counts the attempt, records
/// `worker` and sets the lease to `now()` plus the job's `lease` from `known`.
///
/// Expired leases are looked for first, in a statement of its own: running jobs are few and have
/// an index, while one statement with `OR` could not use the ordered index of the queue and
/// would sort every due job on every claim.
pub async fn claim(
    client: &impl GenericClient,
    known: &[Known<'_>],
    worker: &str,
) -> Result<Option<Claimed>, DbError> {
    if known.is_empty() {
        return Ok(None);
    }
    let names: Vec<&str> = known.iter().map(|k| k.name).collect();
    let leases: Vec<f64> = known.iter().map(|k| k.lease.as_secs_f64()).collect();
    for due in [
        "status = 'running' AND locked_until < now()",
        "status = 'queued' AND run_at <= now()",
    ] {
        let sql = format!(
            "UPDATE fb_job SET status = 'running', attempts = attempts + 1, locked_by = $3, \
               locked_until = now() + make_interval(secs => ( \
                 SELECT t.lease FROM unnest($1::text[], $2::float8[]) AS t(name, lease) \
                 WHERE t.name = fb_job.name)) \
             WHERE id = (SELECT id FROM fb_job WHERE name = ANY($1::text[]) AND {due} \
                         ORDER BY run_at, id LIMIT 1 FOR UPDATE SKIP LOCKED) \
             RETURNING id, name, payload::text, attempts"
        );
        if let Some(row) = client.query_opt(&sql, &[&names, &leases, &worker]).await? {
            let text: String = row.try_get(2)?;
            return Ok(Some(Claimed {
                id: row.try_get(0)?,
                name: row.try_get(1)?,
                payload: parse_payload(&text)?,
                attempt: row.try_get(3)?,
            }));
        }
    }
    Ok(None)
}

/// Marks the job done, if this worker still holds it on this attempt. Call it in the transaction
/// of the handler: when it returns `false` the lease was lost and the caller must roll the
/// transaction back, so the handler's effects commit together with the completion or not at all.
pub async fn complete(
    client: &impl GenericClient,
    id: Uuid,
    worker: &str,
    attempt: i32,
) -> Result<bool, DbError> {
    let changed = client
        .execute(
            "UPDATE fb_job SET status = 'done', finished_at = now(), locked_until = NULL \
             WHERE id = $1 AND status = 'running' AND locked_by = $2 AND attempts = $3",
            &[&id, &worker, &attempt],
        )
        .await?;
    Ok(changed == 1)
}

/// Records a failed attempt, if this worker still holds the job on this attempt. With
/// [`Failure::Retry`] the job is queued again, due after the delay; with [`Failure::Dead`] it
/// is dead. `error` is stored, cut to [`MAX_ERROR_CHARS`] characters. Returns `false` when the
/// lease was lost (nothing changed).
pub async fn fail(
    client: &impl GenericClient,
    id: Uuid,
    worker: &str,
    attempt: i32,
    error: &str,
    failure: Failure,
) -> Result<bool, DbError> {
    let error: String = error.chars().take(MAX_ERROR_CHARS).collect();
    let guard = "WHERE id = $1 AND status = 'running' AND locked_by = $2 AND attempts = $3";
    let changed = match failure {
        Failure::Retry(after) => {
            let secs = after.as_secs_f64();
            client
                .execute(
                    &format!(
                        "UPDATE fb_job SET status = 'queued', run_at = now() + make_interval(secs => $5), \
                         last_error = $4, locked_by = NULL, locked_until = NULL {guard}"
                    ),
                    &[&id, &worker, &attempt, &error, &secs],
                )
                .await?
        }
        Failure::Dead => {
            client
                .execute(
                    &format!(
                        "UPDATE fb_job SET status = 'dead', finished_at = now(), last_error = $4, \
                         locked_until = NULL {guard}"
                    ),
                    &[&id, &worker, &attempt, &error],
                )
                .await?
        }
    };
    Ok(changed == 1)
}

/// Gives a claimed job back, for example when a worker stops: the job is queued again, due now,
/// and the attempt is not counted. Guarded like [`complete`]; returns `false` when the lease
/// was lost.
pub async fn release(
    client: &impl GenericClient,
    id: Uuid,
    worker: &str,
    attempt: i32,
) -> Result<bool, DbError> {
    let changed = client
        .execute(
            "UPDATE fb_job SET status = 'queued', attempts = attempts - 1, run_at = now(), \
             locked_by = NULL, locked_until = NULL \
             WHERE id = $1 AND status = 'running' AND locked_by = $2 AND attempts = $3",
            &[&id, &worker, &attempt],
        )
        .await?;
    Ok(changed == 1)
}

/// Which jobs [`list`] returns.
#[derive(Debug, Clone, Copy, Default)]
pub struct JobFilter<'a> {
    /// Only this status: `queued`, `running`, `done` or `dead`.
    pub status: Option<&'a str>,
    /// Only this full name.
    pub name: Option<&'a str>,
}

/// The newest jobs first (by id, which is time ordered), at most `limit`.
pub async fn list(
    client: &impl GenericClient,
    filter: &JobFilter<'_>,
    limit: i64,
) -> Result<Vec<Job>, DbError> {
    let rows = client
        .query(
            &format!(
                "SELECT {COLUMNS} FROM fb_job \
                 WHERE ($1::text IS NULL OR status = $1) AND ($2::text IS NULL OR name = $2) \
                 ORDER BY id DESC LIMIT $3"
            ),
            &[&filter.status, &filter.name, &limit],
        )
        .await?;
    rows.iter().map(job_from_row).collect()
}

/// The job with this id, or `None`. An id that is not a UUID is [`DbError::Job`].
pub async fn get(client: &impl GenericClient, id: &str) -> Result<Option<Job>, DbError> {
    let id = parse_id(id)?;
    let row = client
        .query_opt(
            &format!("SELECT {COLUMNS} FROM fb_job WHERE id = $1"),
            &[&id],
        )
        .await?;
    row.as_ref().map(job_from_row).transpose()
}

/// Queues a dead job again: `attempts` back to 0, due now, error and finish time cleared. Any
/// other status is refused ([`RetryResult::NotDead`]).
pub async fn retry_dead(client: &impl GenericClient, id: &str) -> Result<RetryResult, DbError> {
    let id = parse_id(id)?;
    let changed = client
        .execute(
            "UPDATE fb_job SET status = 'queued', attempts = 0, run_at = now(), last_error = NULL, \
             finished_at = NULL, locked_by = NULL, locked_until = NULL \
             WHERE id = $1 AND status = 'dead'",
            &[&id],
        )
        .await?;
    if changed == 1 {
        return Ok(RetryResult::Queued);
    }
    let row = client
        .query_opt("SELECT status FROM fb_job WHERE id = $1", &[&id])
        .await?;
    Ok(match row {
        Some(row) => RetryResult::NotDead(row.try_get(0)?),
        None => RetryResult::NotFound,
    })
}

fn parse_id(id: &str) -> Result<Uuid, DbError> {
    Uuid::parse_str(id).map_err(|_| DbError::Job(format!("`{id}` is not a job id (a UUID)")))
}

fn parse_payload(text: &str) -> Result<serde_json::Value, DbError> {
    serde_json::from_str(text)
        .map_err(|e| DbError::Job(format!("a stored payload is not JSON: {e}")))
}

fn job_from_row(row: &tokio_postgres::Row) -> Result<Job, DbError> {
    let payload: String = row.try_get(2)?;
    Ok(Job {
        id: row.try_get(0)?,
        name: row.try_get(1)?,
        payload: parse_payload(&payload)?,
        status: row.try_get(3)?,
        attempts: row.try_get(4)?,
        run_at: row.try_get(5)?,
        locked_until: row.try_get(6)?,
        locked_by: row.try_get(7)?,
        last_error: row.try_get(8)?,
        enqueued_by: row.try_get(9)?,
        created_at: row.try_get(10)?,
        finished_at: row.try_get(11)?,
    })
}

/// A connection for the transaction of a handler. If it is dropped before [`JobConnection::finish`]
/// (a panic, a timeout, an aborted task), the connection is closed instead of going back to the
/// pool: a query may still be running on it, and a pool must never hand that out again.
pub struct JobConnection {
    object: Option<deadpool_postgres::Object>,
    finished: bool,
}

impl JobConnection {
    /// Takes a connection from the pool.
    pub async fn get(pool: &crate::Pool) -> Result<Self, DbError> {
        Ok(Self {
            object: Some(pool.get().await?),
            finished: false,
        })
    }

    /// The client, to start the handler's transaction on.
    pub fn client(&mut self) -> &mut Client {
        // `object` is `Some` until `drop`.
        self.object.as_mut().expect("the connection is present")
    }

    /// The transaction on this connection ended (committed or rolled back) normally: the
    /// connection may go back to the pool.
    pub fn finish(mut self) {
        self.finished = true;
    }
}

impl Drop for JobConnection {
    fn drop(&mut self) {
        if !self.finished
            && let Some(object) = self.object.take()
        {
            // Taken out of the pool: dropping it closes the connection.
            drop(deadpool_postgres::Object::take(object));
        }
    }
}
