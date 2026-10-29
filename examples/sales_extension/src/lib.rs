//! The `sales_extension` example app: a custom app on top of `sales`.
//!
//! It adds a DocType of its own (`customer_visit`, with a Link to `sales`' `customer`) and one
//! hook on a DocType of `sales` (`sales_order`), without changing anything in `sales`.

use forgebase::{App, Decimal, DocTypeHooks, Document, HookContext, HookError, Value};

/// The smallest `total` of a sales order that may be saved: 10.00.
///
/// A constant, because hooks have no database access: they cannot read a setting. A real
/// deployment that needs a configurable minimum waits for the hook capabilities of a later
/// milestone, or changes the constant and rebuilds.
pub const MIN_ORDER_TOTAL: Decimal = Decimal::TEN;

/// The app. It depends on `sales`, which must be registered before it in `main.rs`.
pub fn app() -> App {
    App::new("sales_extension")
        .version(env!("CARGO_PKG_VERSION"))
        .depends_on(&["sales"])
        // Every file in doctypes/ must be listed here (a test checks that).
        .doctypes(forgebase::doctypes!["doctypes/customer_visit.toml"])
        // A hook on a DocType of `sales`: allowed because we depend on it.
        .hooks("sales_order", MinimumOrder)
}

/// Refuses a sales order whose total is below [`MIN_ORDER_TOTAL`].
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
