//! The `sales` example app: customers and sales orders, in the role of the ERPNext sales module.
//!
//! It shows what an app is made of (DocTypes, hooks, a data migration, background jobs and a
//! schedule) using the public `forgebase` API only. `main.rs` puts it into a binary;
//! `sales_extension` builds on it.

use forgebase::serde_json::json;
use forgebase::{
    App, Column, Date, Decimal, DocTypeHooks, Document, FilterOp, HookContext, HookError,
    JobContext, JobError, JobFuture, JobHandler, ListQuery, Param, Report, Row, Schedule, Value,
};

/// The largest `total` a **Sales User** may approve: 10 000.00. Above it the `"Approve"` transition is
/// vetoed and the order needs `"Approve as Manager"`, a transition only a Sales Manager may take.
///
/// A constant, because hooks have no database access (see also `sales_extension::MIN_ORDER_TOTAL`).
/// It is 1 000 000 with scale 2; `from_parts` is the const constructor.
pub const APPROVAL_LIMIT: Decimal = Decimal::from_parts(1_000_000, 0, 0, false, 2);

/// The app. A plain function that returns data: nothing is registered anywhere else.
pub fn app() -> App {
    App::new("sales")
        .version(env!("CARGO_PKG_VERSION"))
        // Every file in doctypes/ must be listed here (a test checks that).
        .doctypes(forgebase::doctypes![
            "doctypes/customer.toml",
            "doctypes/customer_group.toml",
            "doctypes/sales_daily_summary.toml",
            "doctypes/sales_order.toml",
            "doctypes/sales_order_item.toml",
            "doctypes/sales_order_log.toml",
        ])
        .hooks("sales_order", OrderRules)
        // Background jobs are known as `<app>.<job>`: `sales.log_order_event` is enqueued by the
        // hooks below, `sales.daily_summary` by the schedule. `forgebase worker` runs them.
        .job("log_order_event", LogOrderEvent)
        .job("daily_summary", DailySummary)
        // Every day at 02:00 UTC. Run `migrate` after changing a schedule: it stores them.
        .schedule("daily_summary", Schedule::daily_at(2, 0))
        // A query report (ADR 0016): SQL compiled into the binary, run by the roles named here in a
        // read-only transaction. Parameters and columns are positional, see the SQL file.
        .report(
            "sales_by_customer",
            Report::sql(include_str!("../reports/sales_by_customer.sql"))
                .label("Sales by customer")
                .roles(&["Sales Manager"])
                .param(Param::date("from").required())
                .param(Param::date("to").required())
                .param(Param::link("customer", "customer"))
                .column(Column::link("customer", "customer"))
                .column(Column::int("orders"))
                .column(Column::decimal("total"))
                .column(Column::date("last_order")),
        )
        // Data migrations (SQL, append-only): see the comments in the file.
        .migrations(forgebase::migrations![(
            1,
            "default_customer_groups",
            "migrations/0001_default_customer_groups.sql"
        )])
}

/// Business rules of a sales order: computed amounts and total, and what a submit needs.
pub struct OrderRules;

impl DocTypeHooks for OrderRules {
    /// Computes `amount = qty * rate` of every row and `total = sum(amount)`.
    ///
    /// The computed values **overwrite** whatever the client sent: they are not input. Amounts
    /// are rounded to 2 decimal places, half away from zero (2.345 becomes 2.35, -2.345 becomes
    /// -2.35). A row without a usable `qty` or `rate` (missing, null or of another kind: hooks
    /// run before the framework's own validation, which then reports the problem) gets no
    /// amount and adds nothing to the total.
    fn validate(&self, _ctx: &HookContext<'_>, doc: &mut Document) -> Result<(), HookError> {
        let mut total = Decimal::ZERO;
        for row in doc.tables.entry("items".to_owned()).or_default() {
            match amount_of(row)? {
                Some(amount) => {
                    total = total
                        .checked_add(amount)
                        .ok_or_else(|| HookError::new("the order total is too large"))?;
                    row.insert("amount".to_owned(), Value::Decimal(amount));
                }
                None => {
                    row.insert("amount".to_owned(), Value::Null);
                }
            }
        }
        total.rescale(2);
        doc.values.insert("total".to_owned(), Value::Decimal(total));
        Ok(())
    }

    /// Refuses to submit an order without items or with a total that is not positive, then asks
    /// for the job that logs the submit.
    ///
    /// `validate` runs before this hook (see the pipeline in `DocTypeHooks`), so `total` is
    /// current here.
    fn before_submit(&self, ctx: &HookContext<'_>, doc: &mut Document) -> Result<(), HookError> {
        if doc.tables.get("items").is_none_or(Vec::is_empty) {
            return Err(HookError::new(
                "a sales order needs at least one item to be submitted",
            ));
        }
        match doc.values.get("total") {
            Some(Value::Decimal(total)) if *total > Decimal::ZERO => {}
            _ => {
                return Err(HookError::new(
                    "the total of a sales order must be positive to be submitted",
                ));
            }
        }
        enqueue_log(ctx, doc, "submitted")
    }

    /// The approval limit (see the `[workflow]` of `sales_order.toml`): `"Approve"` is refused for
    /// an order whose `total` is above [`APPROVAL_LIMIT`], and for one that has no `total`.
    ///
    /// **Why two actions and not a role check.** Hooks see the actor's name and the transition,
    /// never the actor's roles (ADR 0009, by design: a rule that depends on roles belongs in the
    /// permission rules). So the role is part of the *action*: `"Approve"` (Sales User and Sales
    /// Manager) is limited here, `"Approve as Manager"` (Sales Manager only, checked by the
    /// framework) has no limit and no hook rule. The veto message names the way out.
    ///
    /// `doc` is the stored order, so `total` is the value `validate` computed when it entered
    /// Pending Approval (that draft-to-draft transition runs `validate`).
    /// `edit_roles` on `Pending Approval` keeps a Sales User from lowering it afterwards.
    fn before_transition(
        &self,
        ctx: &HookContext<'_>,
        doc: &mut Document,
    ) -> Result<(), HookError> {
        let is_approve = ctx.transition.is_some_and(|t| t.action == "Approve");
        if is_approve {
            match doc.values.get("total") {
                Some(Value::Decimal(total)) if *total > APPROVAL_LIMIT => {
                    return Err(HookError::new(format!(
                        "orders above {APPROVAL_LIMIT} need a Sales Manager: use 'Approve as Manager'"
                    )));
                }
                Some(Value::Decimal(_)) => {}
                // Defence in depth: `validate` always computes `total`, so an order without one
                // was changed outside the framework. Without a total the limit cannot be applied,
                // so the limited action must not pass.
                _ => {
                    return Err(HookError::new(
                        "the order has no total, so the approval limit cannot be applied: use 'Approve as Manager'",
                    ));
                }
            }
        }
        Ok(())
    }

    /// Asks for the job that logs the cancel.
    fn before_cancel(&self, ctx: &HookContext<'_>, doc: &Document) -> Result<(), HookError> {
        enqueue_log(ctx, doc, "cancelled")
    }
}

/// Enqueues `sales.log_order_event` for the order `doc`.
///
/// This is the transactional outbox: the framework writes the job in the same transaction as the
/// submit or cancel, after all hooks have run. If the operation is refused (by a later hook, by
/// validation or by a version conflict) nothing is committed and no job exists. A hook cannot
/// write the log itself: hooks have no database access, and the log must not exist for an
/// operation that failed.
fn enqueue_log(ctx: &HookContext<'_>, doc: &Document, event: &str) -> Result<(), HookError> {
    let order = doc
        .name
        .as_deref()
        .ok_or_else(|| HookError::new("the sales order has no name"))?;
    ctx.enqueue(
        "sales.log_order_event",
        json!({"order": order, "event": event}),
    )
}

/// `qty * rate` rounded to 2 places, or `None` when either is missing or not a number.
fn amount_of(row: &Row) -> Result<Option<Decimal>, HookError> {
    let Some(Value::Int(qty)) = row.get("qty") else {
        return Ok(None);
    };
    let Some(Value::Decimal(rate)) = row.get("rate") else {
        return Ok(None);
    };
    let too_large = || HookError::new("an item amount is too large");
    let amount = Decimal::from(*qty)
        .checked_mul(*rate)
        .ok_or_else(too_large)?;
    round_half_away(amount).map(Some).ok_or_else(too_large)
}

/// Rounds to 2 decimal places, half away from zero, or `None` on overflow. (`Decimal::round_dp`
/// rounds half to even, and the rounding-strategy type is not part of the public `forgebase`
/// API, so the rule is written out.)
fn round_half_away(value: Decimal) -> Option<Decimal> {
    let hundred = Decimal::ONE_HUNDRED;
    let half = Decimal::new(5, 1);
    // floor(|value| * 100 + 0.5) / 100, with the sign put back.
    let scaled = value.abs().checked_mul(hundred)?.checked_add(half)?;
    let mut rounded = scaled.floor() / hundred;
    rounded.rescale(2);
    Some(if value.is_sign_negative() {
        -rounded
    } else {
        rounded
    })
}

/// Writes one `sales_order_log` row for a lifecycle event of an order (`submitted` or
/// `cancelled`). The payload is `{"order": <name>, "event": <event>}`.
///
/// Delivery is at-least-once, so the handler must be safe to run twice: the log is keyed by
/// `<order>-<event>`, and a row that already exists means an earlier run did the work.
struct LogOrderEvent;

impl JobHandler for LogOrderEvent {
    // There is no async-trait crate: a handler returns a boxed future.
    fn run<'a>(&'a self, ctx: &'a mut JobContext<'_>) -> JobFuture<'a> {
        Box::pin(async move {
            // Copy what is needed out of the payload: `ctx.documents()` borrows `ctx` mutably.
            let (Some(order), Some(event)) = (
                ctx.payload()["order"].as_str().map(str::to_owned),
                ctx.payload()["event"].as_str().map(str::to_owned),
            ) else {
                // A bad payload stays bad: retrying cannot fix it, so the job goes straight to
                // `dead` (an operator sees the message in `forgebase job list`).
                return Err(JobError::permanent(
                    "the payload must be {\"order\": <name>, \"event\": <event>}",
                ));
            };
            let key = format!("{order}-{event}");
            let mut documents = ctx.documents();

            // Idempotency: the work was done by an earlier run of this job. This check keeps the
            // normal path quiet; the unique key and the re-check after a failed insert (below)
            // cover two deliveries at the same moment.
            if documents.get("sales_order_log", &key).await?.is_some() {
                return Ok(());
            }
            let Some(stored) = documents.get("sales_order", &order).await? else {
                // The order is gone (only a draft can be deleted: a submitted or cancelled order that has a log cannot, because of the Link).
                return Err(JobError::permanent(format!(
                    "the sales order {order} does not exist"
                )));
            };

            let mut log = Document::default();
            log.values
                .insert("key".to_owned(), Value::Text(key.clone()));
            log.values
                .insert("sales_order".to_owned(), Value::Text(order));
            log.values.insert("event".to_owned(), Value::Text(event));
            if let Some(total) = stored.document().values.get("total") {
                log.values.insert("total".to_owned(), total.clone());
            }
            // Runs as the system context with the actor `job:sales.log_order_event`: that is the
            // log's `owner` and `modified_by`. A refusal here (`?`) is a permanent `JobError`;
            // a database error is retried.
            if let Err(error) = documents.insert("sales_order_log", log).await {
                // Two deliveries at the same moment: the other one committed the same key first
                // and the unique name refused this insert (a unique violation is a permanent
                // `JobError`, which cannot be told apart from other refusals). The failed insert
                // ran in a savepoint, so the transaction is usable: if the log exists now, the
                // work is done.
                return match documents.get("sales_order_log", &key).await {
                    Ok(Some(_)) => Ok(()),
                    _ => Err(error.into()),
                };
            }
            Ok(())
        })
    }
}

/// The page `Documents::list` returns at most.
const PAGE: u32 = 500;
/// The most orders one daily summary counts (one less than a page, so a full page means "more").
/// An example's limit, see [`DailySummary`].
const MAX_ORDERS_PER_DAY: usize = (PAGE - 1) as usize;

/// Writes the `sales_daily_summary` of the **day before** the tick: how many orders were
/// submitted with that `order_date`, and their total. The schedule (`daily_at(2, 0)`) gives it
/// the payload `{"scheduled_for": "<the tick, RFC 3339 UTC>"}`.
///
/// Idempotent: the summary is named by its day, so a run that finds it does nothing. That also
/// covers a tick enqueued twice by an operator (`forgebase job enqueue`). Two runs at the same
/// moment: the unique name refuses the second insert, and the handler then finds the summary and
/// succeeds.
///
/// The summary is a snapshot: an order submitted later with a past `order_date` is not counted.
///
/// **Limit of this example.** `Documents::list` returns one page of at most 500 documents and
/// has no offset, so the example counts up to [`MAX_ORDERS_PER_DAY`] orders and fails
/// permanently (it never writes a wrong figure) when there are more. A real app would sum in
/// SQL (a data migration can create a view; the document API has no aggregate), or enqueue one
/// job per page (sort by `name` and filter on the last name seen).
struct DailySummary;

impl JobHandler for DailySummary {
    fn run<'a>(&'a self, ctx: &'a mut JobContext<'_>) -> JobFuture<'a> {
        Box::pin(async move {
            let day = ctx.payload()["scheduled_for"]
                .as_str()
                .and_then(day_before)
                .ok_or_else(|| {
                    JobError::permanent(
                        "the payload must be {\"scheduled_for\": <an RFC 3339 time>}",
                    )
                })?;
            let key = day.to_string();
            let mut documents = ctx.documents();

            if documents.get("sales_daily_summary", &key).await?.is_some() {
                return Ok(());
            }
            // `docstatus` is a standard column of a submittable DocType, so the database does
            // the filtering: only submitted orders (1), not drafts (0) or cancelled ones (2).
            let query = ListQuery::new()
                .filter("order_date", FilterOp::Eq, Value::Date(day))
                .filter("docstatus", FilterOp::Eq, Value::Int(1))
                .limit(PAGE);
            let orders = documents.list("sales_order", &query).await?;
            if orders.len() > MAX_ORDERS_PER_DAY {
                return Err(JobError::permanent(format!(
                    "more than {MAX_ORDERS_PER_DAY} orders on {key}: this example does not page"
                )));
            }

            let mut total = Decimal::ZERO;
            for order in &orders {
                if let Some(Value::Decimal(amount)) = order.document().values.get("total") {
                    total = total
                        .checked_add(*amount)
                        .ok_or_else(|| JobError::permanent("the day's total is too large"))?;
                }
            }
            total.rescale(2);

            let mut summary = Document::default();
            summary
                .values
                .insert("day".to_owned(), Value::Text(key.clone()));
            summary.values.insert(
                "orders".to_owned(),
                Value::Int(i64::try_from(orders.len()).unwrap_or(i64::MAX)),
            );
            summary
                .values
                .insert("total".to_owned(), Value::Decimal(total));
            if let Err(error) = documents.insert("sales_daily_summary", summary).await {
                // Another delivery committed this day first (the unique name refused the
                // insert, in a savepoint): the summary exists, so the work is done.
                return match documents.get("sales_daily_summary", &key).await {
                    Ok(Some(_)) => Ok(()),
                    _ => Err(error.into()),
                };
            }
            Ok(())
        })
    }
}

/// The calendar day before the day of an RFC 3339 time such as `2026-10-31T02:00:00Z`.
///
/// Only the date part is used, and the schedule is UTC, so the text is a UTC day already.
fn day_before(rfc3339: &str) -> Option<Date> {
    let mut parts = rfc3339.get(..10)?.split('-');
    let year: i32 = parts.next()?.parse().ok()?;
    let month: u8 = parts.next()?.parse().ok()?;
    let day: u8 = parts.next()?.parse().ok()?;
    Date::from_calendar_date(year, month.try_into().ok()?, day)
        .ok()?
        .previous_day()
}
