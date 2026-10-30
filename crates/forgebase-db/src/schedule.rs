//! The scheduler: schedule times, the `fb_schedule` table and the tick (M6.2, ADR 0013).
//!
//! A schedule row says when a job is next due. There is no leader: a tick is claimed by moving
//! `next_run_at` forward **in the transaction that enqueues the job**, guarded on the value that
//! was read, so exactly one of any number of concurrent workers wins a due time ([`tick`]).
//! Missed ticks are not caught up: the new `next_run_at` is the first time after the database's
//! `now()`. Every time is the database's `now()`, never a clock of the worker.
//!
//! The definitions live in the binary. [`sync`] makes the rows equal to what the binary
//! declares; it runs inside `migrate`, under the migration lock.

use std::collections::{BTreeMap, BTreeSet};

use forgebase_core::meta::Registry;
use time::{Date, Duration, OffsetDateTime, Time, Weekday};
use tokio_postgres::{Client, GenericClient};

use crate::error::DbError;
use crate::jobs::enqueue;

/// The most minutes of `every N minutes`: one day.
pub const MAX_EVERY_MINUTES: u32 = 1440;

/// The actor recorded on a job that a tick enqueued.
pub const SCHEDULER_ACTOR: &str = "scheduler";

/// When a job is due. Plain values: nothing is checked until [`Spec::problems`] (the apps are
/// validated together at startup).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Spec {
    /// Every `n` minutes, aligned to the Unix epoch: the times at which the minutes since the
    /// epoch are divisible by `n`. 1 to 1440.
    EveryMinutes(u32),
    /// Every day at this UTC time.
    DailyAt {
        /// 0 to 23.
        hour: u8,
        /// 0 to 59.
        minute: u8,
    },
    /// Every week on this day at this UTC time.
    WeeklyOn {
        /// The day.
        weekday: Weekday,
        /// 0 to 23.
        hour: u8,
        /// 0 to 59.
        minute: u8,
    },
}

impl Spec {
    /// What is wrong with the values, one message per problem.
    pub fn problems(&self) -> Vec<String> {
        let mut problems = Vec::new();
        let (hour, minute) = match *self {
            Self::EveryMinutes(n) => {
                if !(1..=MAX_EVERY_MINUTES).contains(&n) {
                    problems.push(format!(
                        "every_minutes({n}): the number of minutes must be from 1 to {MAX_EVERY_MINUTES}"
                    ));
                }
                return problems;
            }
            Self::DailyAt { hour, minute } | Self::WeeklyOn { hour, minute, .. } => (hour, minute),
        };
        if hour > 23 {
            problems.push(format!("the hour {hour} must be from 0 to 23"));
        }
        if minute > 59 {
            problems.push(format!("the minute {minute} must be from 0 to 59"));
        }
        problems
    }

    /// The canonical text stored in `fb_schedule.spec`: `every 15m`, `daily 02:00` or
    /// `weekly mon 02:00`.
    pub fn canonical(&self) -> String {
        match *self {
            Self::EveryMinutes(n) => format!("every {n}m"),
            Self::DailyAt { hour, minute } => format!("daily {hour:02}:{minute:02}"),
            Self::WeeklyOn {
                weekday,
                hour,
                minute,
            } => format!("weekly {} {hour:02}:{minute:02}", day_abbreviation(weekday)),
        }
    }

    /// The first time of this schedule strictly after `t`, in UTC. Only meaningful for values
    /// that have no [`problems`](Spec::problems); out of range values are clamped so the
    /// function never panics.
    pub fn next_after(&self, t: OffsetDateTime) -> OffsetDateTime {
        let t = t.to_offset(time::UtcOffset::UTC);
        match *self {
            Self::EveryMinutes(n) => {
                let n = i64::from(n.clamp(1, MAX_EVERY_MINUTES));
                let minutes = t.unix_timestamp().div_euclid(60);
                let next = (minutes.div_euclid(n) + 1) * n;
                OffsetDateTime::from_unix_timestamp(next * 60).unwrap_or(t)
            }
            Self::DailyAt { hour, minute } => at_or_after(t, t.date(), hour, minute, 1),
            Self::WeeklyOn {
                weekday,
                hour,
                minute,
            } => {
                let today = i64::from(t.weekday().number_days_from_monday());
                let wanted = i64::from(weekday.number_days_from_monday());
                let ahead = (wanted - today).rem_euclid(7);
                let first = t
                    .date()
                    .checked_add(Duration::days(ahead))
                    .unwrap_or(Date::MAX);
                at_or_after(t, first, hour, minute, 7)
            }
        }
    }
}

/// `date` at `hour:minute` UTC if that is after `t`, otherwise `step` days later.
fn at_or_after(t: OffsetDateTime, date: Date, hour: u8, minute: u8, step: i64) -> OffsetDateTime {
    let time = Time::from_hms(hour.min(23), minute.min(59), 0).unwrap_or(Time::MIDNIGHT);
    let candidate = OffsetDateTime::new_utc(date, time);
    if candidate > t {
        return candidate;
    }
    let later = date.checked_add(Duration::days(step)).unwrap_or(Date::MAX);
    OffsetDateTime::new_utc(later, time)
}

fn day_abbreviation(weekday: Weekday) -> &'static str {
    match weekday {
        Weekday::Monday => "mon",
        Weekday::Tuesday => "tue",
        Weekday::Wednesday => "wed",
        Weekday::Thursday => "thu",
        Weekday::Friday => "fri",
        Weekday::Saturday => "sat",
        Weekday::Sunday => "sun",
    }
}

/// A schedule a binary declares: the full job name it enqueues, and when.
#[derive(Debug, Clone, Copy)]
pub struct Declared<'a> {
    /// The full job name, `<app>.<job>`.
    pub name: &'a str,
    /// When it is due.
    pub spec: &'a Spec,
}

/// Makes `fb_schedule` equal to `declared`, in one transaction, and returns one line per change:
///
/// - a new schedule is inserted, due at its first time after the database's `now()`;
/// - a schedule whose spec text changed gets the new text and a recomputed `next_run_at`;
/// - an unchanged one is left alone (it keeps its `next_run_at`);
/// - a row that is not declared is **deleted**: the binary that migrates is the source of
///   truth.
///
/// With `dry_run` the transaction is rolled back and the lines say what would change.
pub async fn sync(
    client: &mut Client,
    declared: &[Declared<'_>],
    dry_run: bool,
) -> Result<Vec<String>, DbError> {
    let tx = client.transaction().await?;
    let now: OffsetDateTime = tx.query_one("SELECT now()", &[]).await?.try_get(0)?;
    let mut stored: BTreeMap<String, String> = BTreeMap::new();
    for row in tx.query("SELECT name, spec FROM fb_schedule", &[]).await? {
        stored.insert(row.try_get(0)?, row.try_get(1)?);
    }
    let mut lines = Vec::new();
    let mut names: BTreeSet<&str> = BTreeSet::new();
    for schedule in declared {
        names.insert(schedule.name);
        let spec = schedule.spec.canonical();
        let next = schedule.spec.next_after(now);
        match stored.get(schedule.name) {
            None => {
                tx.execute(
                    "INSERT INTO fb_schedule (name, spec, next_run_at) VALUES ($1, $2, $3)",
                    &[&schedule.name, &spec, &next],
                )
                .await?;
                lines.push(format!("add {} ({spec})", schedule.name));
            }
            Some(old) if *old != spec => {
                tx.execute(
                    "UPDATE fb_schedule SET spec = $2, next_run_at = $3, updated_at = now() \
                     WHERE name = $1",
                    &[&schedule.name, &spec, &next],
                )
                .await?;
                lines.push(format!("change {} ({old} -> {spec})", schedule.name));
            }
            Some(_) => {}
        }
    }
    for (name, spec) in &stored {
        if !names.contains(name.as_str()) {
            tx.execute("DELETE FROM fb_schedule WHERE name = $1", &[name])
                .await?;
            lines.push(format!("remove {name} ({spec})"));
        }
    }
    if dry_run {
        tx.rollback().await?;
    } else {
        tx.commit().await?;
    }
    Ok(lines)
}

/// One tick: enqueues the job of every schedule that is due, and returns how many it enqueued.
///
/// `known` are the schedules of this binary by full job name; a due row of another name is
/// left for a binary that has it. The algorithm:
///
/// 1. One statement reads the due rows (`next_run_at <= at`) of the known names together with
///    `at`. `at` is the database's `now()` of that statement, so it never comes from the
///    worker's clock; `at_override` replaces it (tests only).
/// 2. For each due row, in a transaction of its own: `UPDATE fb_schedule SET next_run_at =
///    <next>, last_run_at = at WHERE name = $1 AND next_run_at = <the value read>`. Only one of
///    several concurrent workers changes the row; the others find the guard false and get no
///    row. The winner enqueues the job **in the same transaction** (`enqueued_by =
///    'scheduler'`, payload `{"scheduled_for": "<the due time, RFC 3339 UTC>"}`) and commits.
/// 3. `<next>` is the first time of the schedule after `at`, so ticks that were missed (no
///    worker was running) are not caught up: one run, then the next future time.
pub async fn tick(
    client: &mut impl GenericClient,
    registry: &Registry,
    known: &BTreeMap<String, Spec>,
    at_override: Option<OffsetDateTime>,
) -> Result<usize, DbError> {
    if known.is_empty() {
        return Ok(0);
    }
    let names: Vec<&str> = known.keys().map(String::as_str).collect();
    let due = client
        .query(
            "SELECT name, next_run_at, coalesce($1::timestamptz, now()) FROM fb_schedule \
             WHERE next_run_at <= coalesce($1::timestamptz, now()) AND name = ANY($2::text[]) \
             ORDER BY next_run_at, name",
            &[&at_override, &names],
        )
        .await?;
    let mut enqueued = 0;
    for row in &due {
        let name: String = row.try_get(0)?;
        let seen: OffsetDateTime = row.try_get(1)?;
        let at: OffsetDateTime = row.try_get(2)?;
        let Some(spec) = known.get(&name) else {
            continue;
        };
        let next = spec.next_after(at);
        let tx = client.transaction().await?;
        let claimed = tx
            .query_opt(
                "UPDATE fb_schedule SET next_run_at = $2, last_run_at = $3, updated_at = now() \
                 WHERE name = $1 AND next_run_at = $4 \
                 RETURNING to_char($4::timestamptz AT TIME ZONE 'UTC', \
                                   'YYYY-MM-DD\"T\"HH24:MI:SS\"Z\"')",
                &[&name, &next, &at, &seen],
            )
            .await?;
        let Some(claimed) = claimed else {
            tx.rollback().await?;
            continue;
        };
        let scheduled_for: String = claimed.try_get(0)?;
        enqueue(
            &tx,
            registry,
            &name,
            &serde_json::json!({ "scheduled_for": scheduled_for }),
            SCHEDULER_ACTOR,
        )
        .await?;
        tx.commit().await?;
        enqueued += 1;
    }
    Ok(enqueued)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// `YYYY-MM-DD H:MM[:SS[.fraction]]` as a UTC time (the tests avoid the `time` macros).
    fn utc(text: &str) -> OffsetDateTime {
        let (date, clock) = text.split_once(' ').expect("date and time");
        let mut d = date.split('-').map(|p| p.parse::<i32>().expect("number"));
        let (y, mo, da) = (
            d.next().expect("year"),
            d.next().expect("month"),
            d.next().expect("day"),
        );
        let (hms, fraction) = clock.split_once('.').unwrap_or((clock, ""));
        let mut c = hms.split(':').map(|p| p.parse::<u8>().expect("number"));
        let (h, mi, se) = (
            c.next().expect("hour"),
            c.next().expect("minute"),
            c.next().unwrap_or(0),
        );
        let nanos: u32 = if fraction.is_empty() {
            0
        } else {
            format!("{fraction:0<9}").parse().expect("fraction")
        };
        let month = time::Month::try_from(u8::try_from(mo).expect("month")).expect("month");
        let date =
            Date::from_calendar_date(y, month, u8::try_from(da).expect("day")).expect("date");
        OffsetDateTime::new_utc(date, Time::from_hms_nano(h, mi, se, nanos).expect("time"))
    }

    fn every(n: u32, t: OffsetDateTime) -> OffsetDateTime {
        Spec::EveryMinutes(n).next_after(t)
    }

    fn daily(hour: u8, minute: u8, t: OffsetDateTime) -> OffsetDateTime {
        Spec::DailyAt { hour, minute }.next_after(t)
    }

    fn weekly(weekday: Weekday, hour: u8, minute: u8, t: OffsetDateTime) -> OffsetDateTime {
        Spec::WeeklyOn {
            weekday,
            hour,
            minute,
        }
        .next_after(t)
    }

    #[test]
    fn every_n_minutes_is_aligned_to_the_epoch_and_strictly_after() {
        assert_eq!(
            every(15, utc("2026-10-30 10:07:30")),
            utc("2026-10-30 10:15")
        );
        // On a tick: the next one, not the same.
        assert_eq!(every(15, utc("2026-10-30 10:15")), utc("2026-10-30 10:30"));
        // One nanosecond before a tick, and one second after.
        assert_eq!(
            every(15, utc("2026-10-30 10:14:59.999999999")),
            utc("2026-10-30 10:15")
        );
        assert_eq!(
            every(15, utc("2026-10-30 10:15:01")),
            utc("2026-10-30 10:30")
        );
        // The epoch itself.
        assert_eq!(every(1, utc("1970-01-01 0:00")), utc("1970-01-01 0:01"));
        // Before the epoch.
        assert_eq!(every(60, utc("1969-12-31 23:30")), utc("1970-01-01 0:00"));
        // 7 does not divide the hour: the alignment is to the epoch, not to the hour. The
        // epoch is minute 0, so ticks are at minutes divisible by 7 since then.
        let t = every(7, utc("2026-10-30 10:00"));
        assert_eq!((t.unix_timestamp() / 60) % 7, 0);
        assert!(t > utc("2026-10-30 10:00"));
        assert!(t <= utc("2026-10-30 10:07"));
    }

    #[test]
    fn every_minute_crosses_the_minute_hour_day_month_and_year_boundaries() {
        assert_eq!(
            every(1, utc("2026-10-30 10:07:30")),
            utc("2026-10-30 10:08")
        );
        assert_eq!(every(1, utc("2026-10-30 23:59:59")), utc("2026-10-31 0:00"));
        assert_eq!(every(1, utc("2026-12-31 23:59")), utc("2027-01-01 0:00"));
        assert_eq!(every(1, utc("2028-02-28 23:59")), utc("2028-02-29 0:00"));
    }

    #[test]
    fn every_1440_minutes_is_midnight_utc() {
        assert_eq!(every(1440, utc("2026-10-30 0:00")), utc("2026-10-31 0:00"));
        assert_eq!(every(1440, utc("2026-10-30 23:59")), utc("2026-10-31 0:00"));
        assert_eq!(every(1440, utc("2026-12-31 12:00")), utc("2027-01-01 0:00"));
    }

    #[test]
    fn a_time_with_an_offset_is_read_in_utc() {
        assert_eq!(
            daily(
                2,
                0,
                utc("2026-10-30 0:00")
                    .to_offset(time::UtcOffset::from_hms(2, 0, 0).expect("offset"))
            ),
            utc("2026-10-30 2:00")
        );
    }

    #[test]
    fn daily_is_today_before_the_time_and_tomorrow_at_or_after_it() {
        assert_eq!(
            daily(2, 30, utc("2026-10-30 2:29:59")),
            utc("2026-10-30 2:30")
        );
        assert_eq!(daily(2, 30, utc("2026-10-30 2:30")), utc("2026-10-31 2:30"));
        assert_eq!(
            daily(2, 30, utc("2026-10-30 2:30:00.000000001")),
            utc("2026-10-31 2:30")
        );
        assert_eq!(daily(0, 0, utc("2026-10-30 0:00")), utc("2026-10-31 0:00"));
        assert_eq!(
            daily(23, 59, utc("2026-10-30 23:59")),
            utc("2026-10-31 23:59")
        );
    }

    #[test]
    fn daily_crosses_the_end_of_the_month_the_year_and_a_leap_day() {
        assert_eq!(daily(3, 0, utc("2026-04-30 4:00")), utc("2026-05-01 3:00"));
        assert_eq!(daily(3, 0, utc("2026-12-31 4:00")), utc("2027-01-01 3:00"));
        // 2028 is a leap year, 2027 and 2100 are not.
        assert_eq!(daily(3, 0, utc("2028-02-28 4:00")), utc("2028-02-29 3:00"));
        assert_eq!(daily(3, 0, utc("2028-02-29 4:00")), utc("2028-03-01 3:00"));
        assert_eq!(daily(3, 0, utc("2027-02-28 4:00")), utc("2027-03-01 3:00"));
        assert_eq!(daily(3, 0, utc("2100-02-28 4:00")), utc("2100-03-01 3:00"));
    }

    #[test]
    fn weekly_finds_every_weekday_from_a_known_day() {
        // 2026-10-30 is a Friday.
        let friday = utc("2026-10-30 12:00");
        let expected = [
            (Weekday::Saturday, utc("2026-10-31 2:00")),
            (Weekday::Sunday, utc("2026-11-01 2:00")),
            (Weekday::Monday, utc("2026-11-02 2:00")),
            (Weekday::Tuesday, utc("2026-11-03 2:00")),
            (Weekday::Wednesday, utc("2026-11-04 2:00")),
            (Weekday::Thursday, utc("2026-11-05 2:00")),
            // The same day, but the time has passed: a week later.
            (Weekday::Friday, utc("2026-11-06 2:00")),
        ];
        assert_eq!(friday.weekday(), Weekday::Friday);
        for (weekday, next) in expected {
            assert_eq!(weekly(weekday, 2, 0, friday), next, "{weekday}");
        }
    }

    #[test]
    fn weekly_on_the_same_day_before_at_and_after_the_time() {
        let before = utc("2026-10-30 1:59:59");
        let exactly = utc("2026-10-30 2:00");
        let after = utc("2026-10-30 2:00:01");
        assert_eq!(
            weekly(Weekday::Friday, 2, 0, before),
            utc("2026-10-30 2:00")
        );
        assert_eq!(
            weekly(Weekday::Friday, 2, 0, exactly),
            utc("2026-11-06 2:00")
        );
        assert_eq!(weekly(Weekday::Friday, 2, 0, after), utc("2026-11-06 2:00"));
    }

    #[test]
    fn weekly_crosses_the_end_of_the_month_the_year_and_a_leap_day() {
        // Thursday 2026-12-31 -> Monday 2027-01-04.
        assert_eq!(
            weekly(Weekday::Monday, 0, 0, utc("2026-12-31 5:00")),
            utc("2027-01-04 0:00")
        );
        // Sunday 2028-02-27 -> Thursday 2028-03-02 over the leap day.
        assert_eq!(
            weekly(Weekday::Thursday, 6, 15, utc("2028-02-27 7:00")),
            utc("2028-03-02 6:15")
        );
        // Tuesday 2028-02-29 itself.
        assert_eq!(
            weekly(Weekday::Tuesday, 6, 15, utc("2028-02-28 7:00")),
            utc("2028-02-29 6:15")
        );
    }

    #[test]
    fn the_canonical_text_names_the_schedule() {
        assert_eq!(Spec::EveryMinutes(15).canonical(), "every 15m");
        assert_eq!(
            Spec::DailyAt { hour: 2, minute: 0 }.canonical(),
            "daily 02:00"
        );
        assert_eq!(
            Spec::WeeklyOn {
                weekday: Weekday::Monday,
                hour: 2,
                minute: 5
            }
            .canonical(),
            "weekly mon 02:05"
        );
    }

    #[test]
    fn invalid_values_are_listed_and_the_edges_are_valid() {
        assert!(Spec::EveryMinutes(1).problems().is_empty());
        assert!(Spec::EveryMinutes(1440).problems().is_empty());
        assert_eq!(Spec::EveryMinutes(0).problems().len(), 1);
        assert_eq!(Spec::EveryMinutes(1441).problems().len(), 1);
        assert!(
            Spec::DailyAt {
                hour: 23,
                minute: 59
            }
            .problems()
            .is_empty()
        );
        assert_eq!(
            Spec::DailyAt {
                hour: 24,
                minute: 0
            }
            .problems()
            .len(),
            1
        );
        assert_eq!(
            Spec::DailyAt {
                hour: 24,
                minute: 60
            }
            .problems()
            .len(),
            2
        );
        assert_eq!(
            Spec::WeeklyOn {
                weekday: Weekday::Friday,
                hour: 0,
                minute: 60
            }
            .problems()
            .len(),
            1
        );
    }

    #[test]
    fn invalid_values_do_not_panic_in_next_after() {
        let t = utc("2026-10-30 12:00");
        assert!(Spec::EveryMinutes(0).next_after(t) > t);
        assert!(Spec::EveryMinutes(u32::MAX).next_after(t) > t);
        assert!(
            Spec::DailyAt {
                hour: 99,
                minute: 99
            }
            .next_after(t)
                > t
        );
    }
}
