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

use std::sync::Arc;

use forgebase_db::schedule::Spec;
use forgebase_db::{auth, jobs};

use crate::jobs::{JobContext, JobError, JobFuture, JobHandler, JobOptions};
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

/// The built-in jobs with their schedules, in a fixed order.
pub(crate) fn builtin() -> Vec<(JobDef, Spec)> {
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
            spec,
        )
    })
    .collect()
}
