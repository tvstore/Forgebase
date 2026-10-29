//! The `sales` example app: customers and sales orders, in the role of the ERPNext sales module.
//!
//! It shows what an app is made of (DocTypes, hooks, a data migration) using the public
//! `forgebase` API only. `main.rs` puts it into a binary; `sales_extension` builds on it.

use forgebase::{App, Decimal, DocTypeHooks, Document, HookContext, HookError, Row, Value};

/// The app. A plain function that returns data: nothing is registered anywhere else.
pub fn app() -> App {
    App::new("sales")
        .version(env!("CARGO_PKG_VERSION"))
        // Every file in doctypes/ must be listed here (a test checks that).
        .doctypes(forgebase::doctypes![
            "doctypes/customer.toml",
            "doctypes/customer_group.toml",
            "doctypes/sales_order.toml",
            "doctypes/sales_order_item.toml",
        ])
        .hooks("sales_order", OrderRules)
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

    /// Refuses to submit an order without items or with a total that is not positive.
    ///
    /// `validate` runs before this hook (see the pipeline in `DocTypeHooks`), so `total` is
    /// current here.
    fn before_submit(&self, _ctx: &HookContext<'_>, doc: &mut Document) -> Result<(), HookError> {
        if doc.tables.get("items").is_none_or(Vec::is_empty) {
            return Err(HookError::new(
                "a sales order needs at least one item to be submitted",
            ));
        }
        match doc.values.get("total") {
            Some(Value::Decimal(total)) if *total > Decimal::ZERO => Ok(()),
            _ => Err(HookError::new(
                "the total of a sales order must be positive to be submitted",
            )),
        }
    }
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
