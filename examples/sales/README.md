# sales

An example [Forgebase](../../README.md) app in the role of the ERPNext sales module: customers, customer groups and submittable sales orders, with background jobs and a schedule. It is documentation that runs: it uses the public `forgebase` crate only and changes nothing under `crates/`.

It shows:

- **DocTypes** (`doctypes/*.toml`, embedded with `forgebase::doctypes!`): a Link (`customer` to `customer_group`), a child table (`sales_order.items`), a naming series, a submittable DocType and role permissions (`Sales User`, `Sales Manager`).
- **Hooks** (`src/lib.rs`, `OrderRules` on `sales_order`): `validate` computes each item's `amount` and the order's `total`; `before_submit` refuses an order without items or with a total that is not positive; `before_transition` limits the `"Approve"` action (below).
- **An approval workflow** (`[workflow]` in `doctypes/sales_order.toml`; [ADR 0015](../../docs/adr/0015-workflow-states-and-transitions.md), [guide](../../docs/guide/workflow-and-reports.md)): Draft, Pending Approval (docstatus 0; only a Sales Manager may edit an order that waits for approval), Approved (docstatus 1, submitted) and Cancelled (docstatus 2). The transitions are "Submit for Approval" (Sales User, Sales Manager), "Approve" (Sales User, Sales Manager), "Approve as Manager" (Sales Manager), "Reject" back to Draft (Sales Manager) and "Cancel" (Sales Manager). **Plain submit and cancel are refused** on `sales_order`: an order is submitted by approving it and cancelled by the "Cancel" transition, and the `before_submit` and `before_cancel` hooks (and so the log jobs) run inside those transitions.
  - **The approval limit.** Hooks see the actor's name and the transition, never roles (ADR 0009), so a rule "a Sales Manager is needed above 10 000" is written with two actions: `before_transition` refuses `"Approve"` when the stored `total` is above `APPROVAL_LIMIT` (10 000.00, a constant in `src/lib.rs`) and the message says to use `"Approve as Manager"`, which only a Sales Manager may take and which has no limit. `edit_roles` on Pending Approval stops a Sales User from lowering the total after sending the order.
- **A query report** (`reports/sales_by_customer.sql`, [ADR 0016](../../docs/adr/0016-query-reports-read-only-execution.md)): "Sales by customer" for the Sales Manager role. Parameters `from` and `to` (required dates, inclusive) and `customer` (optional Link); columns `customer`, `orders`, `total` and `last_order`; only approved (docstatus 1) orders count, biggest total first. It joins `doc_customer` because a Link is stored as the id of the target row. The desk lists it under Reports.
- **Background jobs** (`src/lib.rs`; [ADR 0013](../../docs/adr/0013-jobs-scheduler-and-transactional-outbox.md)):
  - `before_submit` and `before_cancel` of `OrderRules` enqueue `sales.log_order_event` with `ctx.enqueue`, the transactional outbox: the job is written in the same transaction as the submit or cancel, so a refused or failed operation leaves no job. The job reads the order and writes a `sales_order_log` row (key `<order>-<event>`, for example `SO-2026-00001-submitted`), as the actor `job:sales.log_order_event`.
  - **Idempotent**: delivery is at-least-once, so the job does nothing when the row for its key exists (this keeps the normal path quiet). If two deliveries run at the same moment, the unique key refuses the second insert and the job looks again and finds the row, so it succeeds. An order that no longer exists, or a bad payload, is a permanent `JobError`: the job is `dead` at once, not retried.
- **A schedule** (`App::schedule("daily_summary", Schedule::daily_at(2, 0))`, UTC): `sales.daily_summary` writes a `sales_daily_summary` row named by the day (`YYYY-MM-DD`) for the day **before** the tick (from the payload's `scheduled_for`): the number and total of the orders **submitted** with that `order_date`. It filters on `docstatus` in the list query and stops with a permanent error above 499 orders a day (the list has one page of at most 500 and no offset; the code says what a real app does). The summary is a snapshot: an order submitted later with a past `order_date` is not counted. Also idempotent: a summary that exists is not written again (two runs at the same moment are handled as above). Ticks missed while no worker ran are not caught up.
- **A data migration** (`migrations/0001_default_customer_groups.sql`): creates the customer groups `Retail` and `Wholesale`, once per database.
- **A fixture** (`fixtures/customer_group.json`, `App::fixtures`, [ADR 0022](../../docs/adr/0022-fixtures.md)): the groups `Retail`, `Wholesale` and `Online`. `Retail` and `Wholesale` are also made by the migration above (migrations are append-only, so it stays), which shows both outcomes on purpose: the first `migrate` **keeps** the two that exist (adopts them, writes nothing) and **inserts** `Online`; later runs find all three unchanged.
- **An app method** (`App::method("order_summary", ..)`, [ADR 0019](../../docs/adr/0019-app-method-endpoints.md)): `POST /api/v1/method/sales.order_summary` for the roles Sales User and Sales Manager. Optional input `customer`, `from` and `to` (`order_date`, both included); output `{"orders", "total", "by_state"}` with the total as an exact decimal string. It reads through the caller's documents handle, so it counts what the caller may read, and it pages through the list by `name`. See [Integrating with other systems](../../docs/guide/integration.md).
- **Tests** (`tests/sales.rs`): hooks (and the jobs they enqueue, with `EnqueuedJobs`) without a database, and the HTTP API, `run_jobs` and `run_schedules_at` against a real PostgreSQL: the whole workflow (happy path, the approval limit and its veto, the manager's action, reject and edit again, refused plain submit and cancel, a wrong state or action, the log), the report (`check_reports`, the rows on known data, the role gate), the log after a submit and a cancel, a job delivered twice, no job for a refused submit, the daily summary and its tick, the method `sales.order_summary` (401, 404, CSRF, token, bad input, counts, paging over 500 orders) and the fixture (adoption, insert, unchanged).
- **A shared saved view** is runtime data, not part of the app: `examples/sites/ci.sh` shows an Administrator creating one over HTTP (see the guide).

## Run it

Needs PostgreSQL 16 and an empty database.

```sh
export FORGEBASE_DATABASE_URL=postgres://user:password@localhost:5432/sales
cargo run -p sales -- migrate
echo 'a long password' | cargo run -p sales -- user create ann --role "Sales User" --role "Sales Manager"
FORGEBASE_DEV_INSECURE_COOKIES=1 cargo run -p sales -- serve     # http://127.0.0.1:8000/api/v1
```

Jobs need a worker: `migrate` stores the schedule, and `forgebase worker` runs the jobs and enqueues the daily tick.

```sh
cargo run -p sales -- worker                  # until Ctrl-C or SIGTERM
cargo run -p sales -- job list                # id, name, status, attempts, run_at, last error
cargo run -p sales -- doc list sales_order_log
```

The commands (`doc`, `user`, `token`, ...) are those of the stock binary; see [the quick start](../../docs/guide/getting-started.md#quick-start) for what to do with them. `FORGEBASE_DEV_INSECURE_COOKIES=1` is for plain-HTTP development only.

## Test it

```sh
export FORGEBASE_TEST_DATABASE_URL=postgres://user:password@localhost:5432/postgres   # a superuser or CREATEDB URL
cargo test -p sales
```

Every database test creates and drops its own database. They fail, they do not skip, when the variable is not set.

## Notes

- The computed `amount` and `total` overwrite what a client sends. Amounts are rounded to 2 places, half away from zero.
- `sales_order.items` is not required, so a draft can be saved without items; approving (the submit) is what needs them.
- Because `sales_order` has a workflow, `doc submit` and `doc cancel` fail on it. Use `doc transition sales_order NAME "Submit for Approval" --version 1`, then `"Approve"`, and so on (the CLI is the system context and does not check roles).
- A `sales_order_log` row keeps its order: PostgreSQL refuses to delete an order that a log row links to (a cancelled order is still amended, not deleted).
- [`sales_extension`](../sales_extension/README.md) builds on this app.
