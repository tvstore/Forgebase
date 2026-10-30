//! The binary of the desk's Playwright tests (`desk/e2e/seed.sh`, the `e2e` job of CI): the
//! `forgebase` command with one app, `e2e`, that defines a query report (M8.4, ADR 0016).
//!
//! The stock `forgebase` binary has no apps, and a report can only come from an app compiled into
//! the binary. The DocTypes of the tests still come from `--doctypes DIR` (the example app's
//! DocTypes plus roles that only the tests use, see `seed.sh`), so the report reads their tables
//! (`doc_sales_order` and the others) by name; an app cannot link to the DocTypes of that
//! directory, so its customer column is plain text. Build it with
//! `cargo build -p forgebase --example e2e_app`.

use std::process::ExitCode;

use forgebase::{App, Column, Forgebase, Param, Report};

/// Orders by customer in a date range: how many and their total, from the rows of the items.
const SALES_BY_CUSTOMER: &str = "
SELECT c.full_name AS customer,
       count(DISTINCT so.id) AS orders,
       COALESCE(sum(i.qty * i.rate), 0) AS total,
       max(so.order_date) AS last_order
FROM doc_sales_order so
JOIN doc_customer c ON c.id = so.customer
LEFT JOIN doc_sales_order_item i ON i.parent_id = so.id
WHERE so.order_date >= $1 AND so.order_date <= $2
  AND ($3::text IS NULL OR (CASE so.docstatus WHEN 0 THEN 'Draft' WHEN 1 THEN 'Submitted' ELSE 'Cancelled' END) = $3)
  AND (COALESCE($4::bool, false) OR so.docstatus <> 2)
GROUP BY c.full_name
ORDER BY c.full_name;
";

fn e2e() -> App {
    App::new("e2e").report(
        "sales_by_customer",
        Report::sql(SALES_BY_CUSTOMER)
            .label("Sales by customer")
            .roles(&["Sales Manager"])
            .param(Param::date("from").label("From").required())
            .param(Param::date("to").label("To").required())
            .param(Param::select("status", &["Draft", "Submitted", "Cancelled"]).label("Status"))
            .param(
                Param::check("include_cancelled")
                    .label("Include cancelled")
                    .default_value(forgebase::Value::Bool(false)),
            )
            .column(Column::data("customer").label("Customer"))
            .column(Column::int("orders").label("Orders"))
            .column(Column::decimal("total").label("Total"))
            .column(Column::date("last_order").label("Last order")),
    )
}

fn main() -> ExitCode {
    Forgebase::new().app(e2e()).run()
}
