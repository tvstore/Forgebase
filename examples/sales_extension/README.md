# sales_extension

An example [Forgebase](../../README.md) app in the role of a customer's own **custom app**: it depends on [`sales`](../sales/README.md) and extends it without changing it. It uses the public `forgebase` crate only and changes nothing under `crates/`.

It shows:

- **`depends_on(&["sales"])`**: `sales` must be registered before it in `main.rs`; apps are never sorted.
- **A DocType of its own** (`doctypes/customer_visit.toml`) with a Link to `customer`, a DocType of `sales`.
- **A hook on another app's DocType** (`MinimumOrder` on `sales_order`): `validate` refuses an order whose `total` is below `MIN_ORDER_TOTAL` (10.00). Hooks of one DocType run in app order, so it runs after the `sales` hook that computes `total`. The minimum is a constant because hooks have no database access. In the composed binary, every save of a sales order (draft included) needs a total of at least 10.00, so a draft without items (total 0) can no longer be saved, unlike with `sales` alone.
- **A job of its own on a DocType of `sales`** (`FollowUp` hook and `PlanFollowUpVisit` job): when an order is submitted, a second `before_submit` hook (registered after `MinimumOrder`, so after the hooks of `sales` too) enqueues `sales_extension.plan_follow_up_visit` with `ctx.enqueue` (the transactional outbox: no job if the submit is refused). The job reads the `sales_order` through the documents handle and inserts a `customer_visit` for its customer, seven days after the order's `order_date`, with the notes `Follow-up of <order>`. It is **idempotent**: the visit has an optional unique field `source_order`, and the job looks for it before inserting. An order without an `order_date` gets no visit (a job has no clock rule, so the date comes from the order only). In the composed binary the jobs of both apps run in one worker, so a submit also runs `sales.log_order_event`.
- **The composed binary** (`src/main.rs`): the framework, then `sales`, then this app.
- **Tests** (`tests/extension.rs`): the hooks without a database, and both apps together against a real PostgreSQL (hook order, the cross-app Link, the jobs of both apps, a job delivered twice, no job for a refused submit, the `"app"` of each DocType in `GET /api/v1/meta`, permissions).

## Run it

Needs PostgreSQL 16 and an empty database.

```sh
export FORGEBASE_DATABASE_URL=postgres://user:password@localhost:5432/sales_ext
cargo run -p sales_extension -- migrate          # sales' DocTypes and migration, then customer_visit
echo 'a long password' | cargo run -p sales_extension -- user create ann --role "Sales User"
FORGEBASE_DEV_INSECURE_COOKIES=1 cargo run -p sales_extension -- serve
```

Run the jobs with `cargo run -p sales_extension -- worker` (and see them with `job list`); `doc list customer_visit` shows the planned visits.

Log in with `POST /api/v1/login`; `GET /api/v1/meta` then lists `customer_visit` with `"app": "sales_extension"` and `sales_order` with `"app": "sales"`.

## Test it

```sh
export FORGEBASE_TEST_DATABASE_URL=postgres://user:password@localhost:5432/postgres   # a superuser or CREATEDB URL
cargo test -p sales_extension
```

The tests build the same binary as `main.rs` (`sales::app()` then `sales_extension::app()`).
