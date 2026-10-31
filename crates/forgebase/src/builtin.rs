//! The clean-up jobs the framework owns (ADR 0013, owner decision 8). They are registered in
//! every binary under the reserved app name `forgebase`, with the default job options, and
//! scheduled like an app's jobs. The SQL is in `forgebase-db`; this module only runs it, in
//! batches, in the transaction of the job.
//!
//! | Job | Deletes | Schedule |
//! |---|---|---|
//! | `forgebase.purge_jobs` | `done` and `dead` jobs finished more than 7 days ago | daily 03:00 UTC |
//! | `forgebase.purge_login_failures` | login failure rows that no longer count or lock | every 60 minutes |
//! | `forgebase.purge_sessions` | sessions that are past their expiry or idle limit | every 60 minutes |
//!
//! `forgebase.deliver_webhook` (M9.2, ADR 0020) is the framework's one job that is not a clean-up
//! and has no schedule: a document operation enqueues it, in its own transaction, for every
//! webhook that listens to the event, and the worker posts the signed payload.

use std::sync::Arc;

use std::time::Duration;

use forgebase_db::schedule::Spec;
use forgebase_db::{auth, jobs, webhooks};
use serde_json::{Value as Json, json};

use crate::jobs::{JobContext, JobError, JobFuture, JobHandler, JobOptions};
use crate::webhook_client::{Failure, Outgoing, parse_url, send};
use crate::worker::{JobDef, log};

/// The app name the framework keeps for itself.
pub(crate) const APP: &str = "forgebase";

/// Rows deleted per statement.
const BATCH: i64 = 1000;
/// Statements per run. A run that hits the limit leaves the rest for the next scheduled run,
/// so one run stays short whatever the backlog.
const MAX_BATCHES: u32 = 100;

#[derive(Clone, Copy)]
enum Kind {
    Jobs,
    LoginFailures,
    Sessions,
}

struct Purge(Kind);

impl JobHandler for Purge {
    fn run<'a>(&'a self, ctx: &'a mut JobContext<'_>) -> JobFuture<'a> {
        Box::pin(async move {
            let name = ctx.name().to_owned();
            let mut total = 0;
            for _ in 0..MAX_BATCHES {
                let tx = ctx.transaction();
                let deleted = match self.0 {
                    Kind::Jobs => jobs::purge_finished(tx, BATCH).await,
                    Kind::LoginFailures => auth::purge_login_failures(tx, BATCH).await,
                    Kind::Sessions => auth::purge_sessions(tx, BATCH).await,
                }
                .map_err(|e| JobError::from_db(&e))?;
                total += deleted;
                if deleted < u64::try_from(BATCH).unwrap_or(u64::MAX) {
                    break;
                }
            }
            if total > 0 {
                log(&format!("{name}: deleted {total} row(s)"));
            }
            Ok(())
        })
    }
}

/// The webhook delivery. One attempt is one request; the retries are the job's (ADR 0013).
struct DeliverWebhook;

/// The job timeout of a delivery: the client's own limit is 10 seconds, this is the margin.
const DELIVER_TIMEOUT: Duration = Duration::from_secs(30);

/// What a delivery found out, as the outcome of the job.
fn outcome(status: u16, label: &str) -> Result<(), JobError> {
    match status {
        200..=299 => Ok(()),
        300..=399 => Err(JobError::permanent(format!(
            "{label} answered HTTP {status}: redirects are not followed"
        ))),
        _ => Err(JobError::new(format!("{label} answered HTTP {status}"))),
    }
}

impl JobHandler for DeliverWebhook {
    fn run<'a>(&'a self, ctx: &'a mut JobContext<'_>) -> JobFuture<'a> {
        Box::pin(async move {
            let payload = ctx.payload().clone();
            let field = |key: &str| payload.get(key).and_then(Json::as_str).map(str::to_owned);
            let (Some(name), Some(event)) = (field("webhook"), field("event")) else {
                return Err(JobError::permanent(
                    "the job payload is not a webhook delivery",
                ));
            };
            let webhook = webhooks::get(ctx.transaction(), &name)
                .await
                .map_err(|e| JobError::from_db(&e))?;
            let Some(webhook) = webhook.filter(|w| w.enabled) else {
                // Removed or disabled since the document operation: nothing to send, and
                // retrying would not change it.
                log(&format!(
                    "webhook `{name}` is removed or disabled; the delivery of `{event}` is dropped"
                ));
                return Ok(());
            };
            let mut body = json!({
                "event": event,
                "doctype": payload.get("doctype").cloned().unwrap_or(Json::Null),
                "name": payload.get("name").cloned().unwrap_or(Json::Null),
                "data": payload.get("data").cloned().unwrap_or(Json::Null),
                "occurred_at": payload.get("occurred_at").cloned().unwrap_or(Json::Null),
                "delivery": ctx.id(),
            });
            if payload.get("truncated") == Some(&Json::Bool(true)) {
                body["truncated"] = Json::Bool(true);
            }
            let body = serde_json::to_vec(&body)
                .map_err(|_| JobError::permanent("the delivery body cannot be serialized"))?;
            let label = parse_url(&webhook.url, webhook.insecure_http)
                .map(|t| t.label())
                .map_err(JobError::permanent)?;
            let sent = send(Outgoing {
                url: &webhook.url,
                secret: &webhook.secret,
                allow_private: webhook.allow_private,
                insecure_http: webhook.insecure_http,
                event: &event,
                // The job id is the same on every attempt: receivers de-duplicate on it.
                delivery: ctx.id(),
                body,
            })
            .await;
            match sent {
                Ok(status) => outcome(status, &label),
                Err(Failure::Refused(message)) => Err(JobError::permanent(message)),
                Err(Failure::Transport(message)) => Err(JobError::new(message)),
            }
        })
    }
}

/// The built-in jobs with their schedules (`None`: not scheduled), in a fixed order.
pub(crate) fn builtin() -> Vec<(JobDef, Option<Spec>)> {
    let mut all = clean_ups();
    all.push((
        JobDef {
            name: format!("{APP}.deliver_webhook"),
            handler: Arc::new(DeliverWebhook),
            options: JobOptions::new().timeout(DELIVER_TIMEOUT),
        },
        None,
    ));
    all
}

fn clean_ups() -> Vec<(JobDef, Option<Spec>)> {
    [
        (
            "purge_jobs",
            Kind::Jobs,
            Spec::DailyAt { hour: 3, minute: 0 },
        ),
        (
            "purge_login_failures",
            Kind::LoginFailures,
            Spec::EveryMinutes(60),
        ),
        ("purge_sessions", Kind::Sessions, Spec::EveryMinutes(60)),
    ]
    .into_iter()
    .map(|(name, kind, spec)| {
        (
            JobDef {
                name: format!("{APP}.{name}"),
                handler: Arc::new(Purge(kind)),
                options: JobOptions::new(),
            },
            Some(spec),
        )
    })
    .collect()
}
