# sales

An example [Forgebase](../../README.md) app in the role of the ERPNext sales module: customers, customer groups and submittable sales orders, with background jobs and a schedule. It is documentation that runs: it uses the public `forgebase` crate only and changes nothing under `crates/`.

It shows:

- **DocTypes** (`doctypes/*.toml`, embedded with `forgebase::doctypes!`): a Link (`customer` to `customer_group`), a child table (`sales_order.items`), a naming series, a submittable DocType and role permissions (`Sales User`, `Sales Manager`).
- **Hooks** (`src/lib.rs`, `OrderRules` on `sales_order`): `validate` computes each item's `amount` and the order's `total`; `before_submit` refuses an order without items or with a total that is not positive.
- **Background jobs** (`src/lib.rs`; [ADR 0013](../../docs/adr/0013-jobs-scheduler-and-transactional-outbox.md)):
  - `before_submit` and `before_cancel` of `OrderRules` enqueue `sales.log_order_event` with `ctx.enqueue`, the transactional outbox: the job is written in the same transaction as the submit or cancel, so a refused or failed operation leaves no job. The job reads the order and writes a `sales_order_log` row (key `<order>-<event>`, for example `SO-2026-00001-submitted`), as the actor `job:sales.log_order_event`.
  - **Idempotent**: delivery is at-least-once, so the job does nothing when the row for its key exists (this keeps the normal path quiet). If two deliveries run at the same moment, the unique key refuses the second insert and the job looks again and finds the row, so it succeeds. An order that no longer exists, or a bad payload, is a permanent `JobError`: the job is `dead` at once, not retried.
- **A schedule** (`App::schedule("daily_summary", Schedule::daily_at(2, 0))`, UTC): `sales.daily_summary` writes a `sales_daily_summary` row named by the day (`YYYY-MM-DD`) for the day **before** the tick (from the payload's `scheduled_for`): the number and total of the orders **submitted** with that `order_date`. It filters on `docstatus` in the list query and stops with a permanent error above 499 orders a day (the list has one page of at most 500 and no offset; the code says what a real app does). The summary is a snapshot: an order submitted later with a past `order_date` is not counted. Also idempotent: a summary that exists is not written again (two runs at the same moment are handled as above). Ticks missed while no worker ran are not caught up.
- **A data migration** (`migrations/0001_default_customer_groups.sql`): creates the customer groups `Retail` and `Wholesale`, once per database.
- **Tests** (`tests/sales.rs`): hooks (and the jobs they enqueue, with `EnqueuedJobs`) without a database, and the HTTP API, `run_jobs` and `run_schedules_at` against a real PostgreSQL: the log after a submit and a cancel, a job delivered twice, no job for a refused submit, the daily summary and its tick.

## Run it

Needs PostgreSQL 16 and an empty database.

```sh
export FORGEBASE_DATABASE_URL=postgres://user:password@localhost:5432/sales
cargo run -p sales -- migrate
echo 'a long password' | cargo run -p sales -- user create ann --role "Sales User"
FORGEBASE_DEV_INSECURE_COOKIES=1 cargo run -p sales -- serve     # http://127.0.0.1:8000/api/v1
```

Jobs need a worker: `migrate` stores the schedule, and `forgebase worker` runs the jobs and enqueues the daily tick.

```sh
cargo run -p sales -- worker                  # until Ctrl-C or SIGTERM
cargo run -p sales -- job list                # id, name, status, attempts, run_at, last error
cargo run -p sales -- doc list sales_order_log
```

The commands (`doc`, `user`, `token`, ...) are those of the stock binary; see [the quick start](../../README.md#quick-start) for what to do with them. `FORGEBASE_DEV_INSECURE_COOKIES=1` is for plain-HTTP development only.

## Test it

```sh
export FORGEBASE_TEST_DATABASE_URL=postgres://user:password@localhost:5432/postgres   # a superuser or CREATEDB URL
cargo test -p sales
```

Every database test creates and drops its own database. They fail, they do not skip, when the variable is not set.

## Notes

- The computed `amount` and `total` overwrite what a client sends. Amounts are rounded to 2 places, half away from zero.
- `sales_order.items` is not required, so a draft can be saved without items; submitting is what needs them.
- A `sales_order_log` row keeps its order: PostgreSQL refuses to delete an order that a log row links to (a cancelled order is still amended, not deleted).
- [`sales_extension`](../sales_extension/README.md) builds on this app.
