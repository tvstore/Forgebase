//! The `sales_extension` example app: a custom app on top of `sales`.
//!
//! It adds a DocType of its own (`customer_visit`, with a Link to `sales`' `customer`), a field
//! on a DocType of `sales` (`loyalty_tier` on `customer`, an overlay), two hooks on another
//! DocType of `sales` (`sales_order`) and a background job that works on `sales` documents,
//! without changing anything in `sales`.

use forgebase::serde_json::json;
use forgebase::{
    App, Date, Decimal, DocTypeHooks, Document, FilterOp, HookContext, HookError, JobContext,
    JobError, JobFuture, JobHandler, ListQuery, Value,
};

/// The smallest `total` of a sales order that may be saved: 10.00.
///
/// A constant, because hooks have no database access: they cannot read a setting. A real
/// deployment that needs a configurable minimum waits for the hook capabilities of a later
/// milestone, or changes the constant and rebuilds.
///
/// It is 1000 with scale 2 (10.00); `from_parts` is the const constructor.
pub const MIN_ORDER_TOTAL: Decimal = Decimal::from_parts(1000, 0, 0, false, 2);

/// The app. It depends on `sales`, which must be registered before it in `main.rs`.
pub fn app() -> App {
    App::new("sales_extension")
        .version(env!("CARGO_PKG_VERSION"))
        .depends_on(&["sales"])
        // Every file in doctypes/ must be listed here (a test checks that).
        .doctypes(forgebase::doctypes!["doctypes/customer_visit.toml"])
        // A field added to `customer`, a DocType of `sales` (ADR 0023): an overlay file holds
        // only `[[fields]]`. `sales` does not change; `migrate` adds the column.
        .extend_doctype("customer", forgebase::overlays!["overlays/customer.toml"])
        // Hooks on a DocType of `sales`: allowed because we depend on it. They run after the
        // hooks of `sales`, in this order.
        .hooks("sales_order", MinimumOrder)
        .hooks("sales_order", FollowUp)
        // A job of this app, known as `sales_extension.plan_follow_up_visit`. The binary's
        // worker runs it next to the jobs of `sales`.
        .job("plan_follow_up_visit", PlanFollowUpVisit)
}

/// Refuses a sales order whose total is below [`MIN_ORDER_TOTAL`].
///
/// In the composed binary, every save of a sales order (draft included) needs a total of at
/// least 10.00. A draft without items (total 0) can therefore no longer be saved, unlike with
/// `sales` alone.
pub struct MinimumOrder;

impl DocTypeHooks for MinimumOrder {
    /// Hooks of one DocType run in app order, so this one runs after the `validate` hook of
    /// `sales`, which computes `total` from the items. A missing or odd total counts as 0.
    fn validate(&self, _ctx: &HookContext<'_>, doc: &mut Document) -> Result<(), HookError> {
        let total = match doc.values.get("total") {
            Some(Value::Decimal(total)) => *total,
            _ => Decimal::ZERO,
        };
        if total < MIN_ORDER_TOTAL {
            return Err(HookError::new(format!(
                "a sales order must total at least {MIN_ORDER_TOTAL}"
            )));
        }
        Ok(())
    }
}

/// Asks for a follow-up visit when a sales order is submitted.
///
/// It runs after the `before_submit` of `sales`, which may refuse the submit; then this hook
/// does not run and no job exists. The job is written in the submit's own transaction (the
/// outbox), so it exists only if the submit committed. A second hook set of the same app for the
/// same DocType is fine: hooks run in registration order.
pub struct FollowUp;

impl DocTypeHooks for FollowUp {
    fn before_submit(&self, ctx: &HookContext<'_>, doc: &mut Document) -> Result<(), HookError> {
        let order = doc
            .name
            .as_deref()
            .ok_or_else(|| HookError::new("the sales order has no name"))?;
        ctx.enqueue(
            "sales_extension.plan_follow_up_visit",
            json!({"order": order}),
        )
    }
}

/// How many days after the order date the follow-up visit is planned.
const FOLLOW_UP_DAYS: i32 = 7;

/// Plans a `customer_visit` for the customer of a submitted order, a week after its
/// `order_date`. The payload is `{"order": <name>}`.
///
/// It reads a DocType of `sales` through the documents handle, which is allowed for the apps of
/// a binary (the handle is the system context: no permission rules).
///
/// - **The date.** A job has no clock rule, so the visit date comes from the order alone: the
///   same order always gives the same date, however late the job runs. An order **without an
///   `order_date`** gets no visit: the job ends `done` having done nothing.
/// - **Idempotent.** Delivery is at-least-once, so the visit remembers its order in
///   `source_order` (unique), and the job looks for it first. If two copies of the job ran at the
///   very same moment, the unique field refuses the second insert; the job then looks again, finds
///   the other's visit and succeeds, so there are never two visits. The look-up before the insert
///   only keeps the normal path quiet.
struct PlanFollowUpVisit;

impl JobHandler for PlanFollowUpVisit {
    fn run<'a>(&'a self, ctx: &'a mut JobContext<'_>) -> JobFuture<'a> {
        Box::pin(async move {
            let Some(order) = ctx.payload()["order"].as_str().map(str::to_owned) else {
                return Err(JobError::permanent(
                    "the payload must be {\"order\": <name>}",
                ));
            };
            let mut documents = ctx.documents();

            let Some(stored) = documents.get("sales_order", &order).await? else {
                return Err(JobError::permanent(format!(
                    "the sales order {order} does not exist"
                )));
            };
            let values = &stored.document().values;
            let (Some(Value::Text(customer)), Some(Value::Date(order_date))) =
                (values.get("customer"), values.get("order_date"))
            else {
                // No order date (or, impossible for a saved order, no customer): nothing to plan.
                return Ok(());
            };
            let visited_on = Date::from_julian_day(order_date.to_julian_day() + FOLLOW_UP_DAYS)
                .map_err(|_| JobError::permanent("the follow-up date is out of range"))?;

            // Idempotency: this order already has its visit.
            let existing = ListQuery::new()
                .filter("source_order", FilterOp::Eq, Value::Text(order.clone()))
                .limit(1);
            if !documents
                .list("customer_visit", &existing)
                .await?
                .is_empty()
            {
                return Ok(());
            }

            let mut visit = Document::default();
            visit
                .values
                .insert("customer".to_owned(), Value::Text(customer.clone()));
            visit
                .values
                .insert("visited_on".to_owned(), Value::Date(visited_on));
            visit.values.insert(
                "notes".to_owned(),
                Value::Text(format!("Follow-up of {order}")),
            );
            visit
                .values
                .insert("source_order".to_owned(), Value::Text(order));
            if let Err(error) = documents.insert("customer_visit", visit).await {
                // Two deliveries at the same moment: the other committed this order's visit
                // first and the unique `source_order` refused this insert (in a savepoint, so
                // the transaction is usable). Look again: if the visit exists, the work is done.
                return match documents.list("customer_visit", &existing).await {
                    Ok(found) if !found.is_empty() => Ok(()),
                    _ => Err(error.into()),
                };
            }
            Ok(())
        })
    }
}
