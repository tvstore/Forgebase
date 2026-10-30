//! [`Schedule`]: when a scheduled job runs (ADR 0013).

use forgebase_db::schedule::Spec;
use time::Weekday;

/// When a job runs, for [`App::schedule`](crate::App::schedule). Three kinds: every N minutes,
/// every day at a time, and every week on a day at a time. **All times are UTC**; there are no
/// time zones and no cron expressions.
///
/// ```ignore
/// use forgebase::{App, Schedule, Weekday};
///
/// App::new("crm")
///     .job("send_digest", SendDigest)
///     .schedule("send_digest", Schedule::daily_at(6, 30))
///     .job("weekly_report", WeeklyReport)
///     .schedule("weekly_report", Schedule::weekly_on(Weekday::Monday, 7, 0))
/// ```
///
/// A schedule only holds the values it was given. Invalid values (see each constructor) are
/// reported with the other mistakes of an app when the binary starts
/// ([`StartupError`](crate::StartupError)), not as a panic.
///
/// **Rules of a tick** (details in ADR 0013):
///
/// - A tick is enqueued once, however many workers run. The job gets the payload
///   `{"scheduled_for": "<RFC 3339 UTC time of the tick>"}`: the time that was due, not the
///   time the worker noticed. Use it for idempotency, because delivery is at-least-once.
/// - Ticks are **not caught up**. If no worker ran during several ticks, one job is enqueued
///   when a worker returns, and the next one is the next future time.
/// - The schedules of a binary are written to the database by `forgebase migrate`, which
///   deletes the schedules the binary does not declare.
#[derive(Debug, Clone, PartialEq)]
#[non_exhaustive]
pub struct Schedule {
    pub(crate) spec: Spec,
}

impl Schedule {
    /// Every `n` minutes, from 1 to 1440 (one day). The ticks are aligned to the **Unix epoch**,
    /// not to the moment the schedule was added or the worker started: they are the times at
    /// which the number of minutes since 1970-01-01 00:00 UTC is divisible by `n`. Every 15
    /// gives :00, :15, :30 and :45 of every hour; every 1440 gives midnight UTC; a number that
    /// does not divide 60 (7, for example) gives times that do not repeat each hour.
    pub fn every_minutes(n: u32) -> Self {
        Self {
            spec: Spec::EveryMinutes(n),
        }
    }

    /// Every day at `hour:minute` UTC. `hour` is 0 to 23 and `minute` 0 to 59.
    pub fn daily_at(hour: u8, minute: u8) -> Self {
        Self {
            spec: Spec::DailyAt { hour, minute },
        }
    }

    /// Every week on `weekday` at `hour:minute` UTC. `hour` is 0 to 23 and `minute` 0 to 59.
    pub fn weekly_on(weekday: Weekday, hour: u8, minute: u8) -> Self {
        Self {
            spec: Spec::WeeklyOn {
                weekday,
                hour,
                minute,
            },
        }
    }

    /// What is wrong with the values, one message per problem.
    pub(crate) fn problems(&self) -> Vec<String> {
        self.spec.problems()
    }
}
