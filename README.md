<p align="center"><img src="docs/assets/logo-banner.svg" alt="Forgebase" width="480"></p>

<p align="center"><strong>The foundation for business applications.</strong></p>

<p align="center">
  <img src="docs/assets/badge-rust-powered.svg" alt="Rust-powered">
  <img src="docs/assets/badge-metadata-driven.svg" alt="Metadata-driven">
  <img src="docs/assets/badge-full-stack.svg" alt="Full-stack">
</p>

# Forgebase

Forgebase is a Rust framework for building business applications, inspired by the concepts and capabilities of the Frappe Framework. It is an independent implementation: no Frappe code is copied.

> Build for longevity, not complexity. Maintenance is the most expensive part of software ownership.

## Status

**M0 Foundations, M1 Metadata and storage, M2 Relationships and lifecycle, M3 Auth, permissions and REST API and M4 Minimal Desk are done** (M4.1: the metadata API, the `desk/` React app with login, home and list view, served by `forgebase serve`; M4.2: the form view with Submit, Cancel, Amend and Delete, and one Playwright smoke test in CI) (M1.1: DocType metadata model and document validation in `forgebase-core`; M1.2: PostgreSQL pool, framework migrations and additive schema sync in `forgebase-db`; M1.3: document insert/get/update/delete in `forgebase-db` and the `forgebase` command-line tool). M2 was delivered in four PRs: M2.1 adds Link fields (foreign keys) and optimistic concurrency (`update` and `delete` need the expected version); M2.2 adds child tables (Table fields: rows saved with their parent in one transaction); M2.3 adds the lifecycle pipeline and hooks (`DocTypeHooks`, registered by the app; see ADR 0009); M2.4 adds docstatus (`submittable` DocTypes with `submit`, `cancel` and `amend`) and naming series (counters gapless per prefix). M3 was delivered in three PRs: M3.1 adds users, roles, Argon2id passwords, server-side sessions, API tokens and login lockout (`forgebase_db::auth`, ADR 0010) and the `forgebase user` and `forgebase token` commands; M3.2 adds role permissions (`[[permissions]]` in the DocType file, enforced in every document operation, deny by default; ADR 0011) and the list query (`forgebase_db::list`, `forgebase doc list`); M3.3 adds the HTTP API (`forgebase serve`: `/api/v1/resource`, login and logout, cookie sessions with CSRF protection, API tokens, `Host` validation, no CORS). M5 (app model and developer experience) is in progress: **M5.1** adds the app model (the entry crate is now called `forgebase`, a library and the `forgebase` binary; apps are plain Rust crates built from `forgebase::App` and listed in order in the binary's `main`; embedded DocTypes, hooks across apps, SQL data migrations per app, `forgebase::testing`; ADR 0012); **M5.2** adds `forgebase new-app` (generates an app crate that builds and passes its own tests, checked by the CI job `app-template`) and the [first-app guide](docs/guide/first-app.md); example apps follow in M5.3. M6 (jobs and scheduler) is in progress: **M6.1** adds background jobs (a Postgres queue, `forgebase worker`, `forgebase job`, the transactional outbox for hooks; ADR 0013); **M6.2** adds the scheduler (`App::schedule`: every N minutes, daily or weekly, UTC) and the built-in clean-up jobs; the example apps follow in M6.3. **M7 (site management) is done:** M7.1 adds sites, a database per site, the `forgebase.toml` config file and `Host` routing (see [Sites](#sites) and ADR 0014); M7.2 adds the apps installed per site, `forgebase site list` and `forgebase site create`; M7.3 adds `forgebase site backup` and `forgebase site restore`, the [operations guide](docs/guide/operations.md) and the CI job `sites`, which completes M7. The workspace has four crates, CI and a toolchain pin. The documents below record research, proposed architecture and open decisions. ADRs 0002-0006 (workspace layout, PostgreSQL only, database access direction, HTTP stack, extension model) are Accepted; most other decisions are **Proposed**, not final. Each document marks its status.

The repository is currently **private**. It is intended to be open source under the MIT license; it is not open source yet.

## Documentation map

| Document | Contents |
|---|---|
| [AGENTS.md](AGENTS.md) | Permanent project instructions: engineering principles, four-stage workflow, git safety, model selection policy |
| [docs/blueprint/vision.md](docs/blueprint/vision.md) | Purpose, target users, goals, non-goals, relation to Frappe |
| [docs/blueprint/feature-matrix.md](docs/blueprint/feature-matrix.md) | Frappe capabilities vs Forgebase status, with evidence labels |
| [docs/blueprint/architecture.md](docs/blueprint/architecture.md) | Architecture (partly Accepted, mostly Proposed): crates, metadata, lifecycle, API, jobs, multi-site |
| [docs/blueprint/dependency-evaluation.md](docs/blueprint/dependency-evaluation.md) | Rust dependency comparison, licenses, recommendations |
| [docs/blueprint/security-model.md](docs/blueprint/security-model.md) | Authentication, authorization, injection defense, isolation |
| [docs/blueprint/milestones.md](docs/blueprint/milestones.md) | M0 to M10 (roadmap) and later work, with exit criteria |
| [docs/blueprint/maintenance-policy.md](docs/blueprint/maintenance-policy.md) | Toolchain, dependency admission, testing, docs and review rules |
| [CONTRIBUTING.md](CONTRIBUTING.md) | Prerequisites and local checks |
| [docs/guide/operations.md](docs/guide/operations.md) | Running a deployment: configuration, sites, reverse proxy, workers, upgrades, backups and restores |
| [examples/sites/](examples/sites/README.md) | Two sites on one server and a backup restored into a new database (the CI job `sites`) |
| [examples/sales/](examples/sales/README.md) | Example app used by the quick start: DocTypes (a link, a child table), hooks, a data migration, tests |
| [examples/sales_extension/](examples/sales_extension/README.md) | Example app that extends `sales` (a Link across apps, a hook on another app's DocType) |
| [docs/adr/](docs/adr/README.md) | Architecture Decision Records |

## Getting started

```sh
cargo build
cargo run -p forgebase -- --version
```

### Quick start

Needs a PostgreSQL 16 server and an empty database. Connections are not encrypted yet (no TLS), so use a local or private network.

The quick start runs the binary of the example app [`examples/sales`](examples/sales/README.md), so its DocTypes, hooks and data migration are built in and there is no `--doctypes` option. (The stock `forgebase` binary has no apps: it takes the DocTypes of a directory with `--doctypes DIR`, see [Apps](#apps).) Every command here was run against a fresh database.

```sh
export FORGEBASE_DATABASE_URL=postgres://user:password@localhost:5432/mydb

# Create the framework tables and the DocType tables, then apply the app's data migrations (add --dry-run to only print the plan; on a fresh database it shows the pending framework migrations only, the DocType plan appears once they are applied)
cargo run -p sales -- migrate

# The migration `default_customer_groups` of the app created two customer groups, Retail and Wholesale
cargo run -p sales -- doc list customer_group

# Insert another customer group, then a customer that links to it by name (the name comes from the DocType's naming field)
cargo run -p sales -- doc insert customer_group \
  --data '{"title": "Partner"}'
cargo run -p sales -- doc insert customer \
  --data '{"email": "ann@example.com", "full_name": "Ann", "credit_limit": "1250.50", "customer_group": "Partner"}'

# Read it (note its "version"), change it, then delete it (below). update and delete need --version, the version you last read;
# if someone changed the document meanwhile they fail with a conflict that shows the current version.
cargo run -p sales -- doc get customer ann@example.com
cargo run -p sales -- doc update customer ann@example.com --version 1 \
  --data '{"loyalty_points": 10}'

# A customer group that a customer still links to cannot be deleted: this command fails (exit code 1, the error names `customer`)
cargo run -p sales -- doc delete customer_group Partner --version 1
# Delete the customer first, then the group can be deleted
cargo run -p sales -- doc delete customer ann@example.com --version 2
cargo run -p sales -- doc delete customer_group Partner --version 1

# A sales order has a child table, `items`: the rows are an array in the same JSON and are saved with the order in one transaction.
# Its name comes from a naming series (SO-<year>-00001, ...); the output shows it, the rows in order and the version. The sed keeps just the name for the next commands.
# The `validate` hook of the app computes each row's `amount` (qty * rate, 2 places) and the order's `total`. A row without a rate has no amount.
cargo run -p sales -- doc insert customer \
  --data '{"email": "bob@example.com", "full_name": "Bob"}'
ORDER=$(cargo run -q -p sales -- doc insert sales_order \
  --data '{"customer": "bob@example.com", "order_date": "2026-10-28", "items": [
    {"description": "Widget", "qty": 2, "rate": "9.50"},
    {"description": "Gadget", "qty": 1}]}' | sed -n 's/^  "name": "\(.*\)",$/\1/p')

# An update that names `items` replaces its rows when they differ from the stored ones (a table with identical rows keeps its row ids); one that leaves it out keeps them.
# The hook runs again: the amount becomes 47.50 and the total 47.50.
cargo run -p sales -- doc update sales_order "$ORDER" --version 1 \
  --data '{"items": [{"description": "Widget", "qty": 5, "rate": "9.50"}]}'
cargo run -p sales -- doc delete sales_order "$ORDER" --version 2
```

### Submit, cancel and amend

`sales_order` is `submittable` and uses a naming series ([sales_order.toml](examples/sales/doctypes/sales_order.toml)): a new order is a **draft** (`docstatus` 0, named `SO-<year>-00001`, ...; it may have no items yet), `submit` needs at least one item and a positive total (the `before_submit` hook of the app refuses otherwise) and locks it (`docstatus` 1; only fields marked `allow_on_submit`, here `remarks`, can still change), `cancel` makes it `docstatus` 2, and `amend` creates a new draft from a cancelled order, named `<original>-<n>` with `amended_from` set. Every step needs `--version`, the version you last read (each one adds 1).

```sh
# Continues the example above (the `bob@example.com` customer exists). A new order is a draft: "docstatus": 0, named by the series.
NAME=$(cargo run -q -p sales -- doc insert sales_order \
  --data '{"customer": "bob@example.com", "items": [{"description": "Widget", "qty": 2, "rate": "9.50"}]}' | sed -n 's/^  "name": "\(.*\)",$/\1/p')

# An order without items can be saved as a draft but not submitted: this fails (exit code 1) with the hook's message
# "a sales order needs at least one item to be submitted". (Delete the draft again; it is version 1.)
EMPTY=$(cargo run -q -p sales -- doc insert sales_order \
  --data '{"customer": "bob@example.com"}' | sed -n 's/^  "name": "\(.*\)",$/\1/p')
cargo run -p sales -- doc submit sales_order "$EMPTY" --version 1
cargo run -p sales -- doc delete sales_order "$EMPTY" --version 1

# Submit it (version 1 -> 2): "docstatus": 1. A submitted order is locked: this update fails (exit code 1) ...
cargo run -p sales -- doc submit sales_order "$NAME" --version 1
cargo run -p sales -- doc update sales_order "$NAME" --version 2 \
  --data '{"order_date": "2026-11-01"}'
# ... and this one, on the allow_on_submit field, works (version 2 -> 3). A submitted order cannot be deleted either.
cargo run -p sales -- doc update sales_order "$NAME" --version 2 \
  --data '{"remarks": "rush delivery"}'

# Cancel it (3 -> 4: "docstatus": 2), then amend it: a new draft with the same values and rows, named "<NAME>-1", "amended_from": "<NAME>".
cargo run -p sales -- doc cancel sales_order "$NAME" --version 3
cargo run -p sales -- doc amend sales_order "$NAME" --version 4
# The amendment is a draft again: edit it, submit it, and so on. A cancelled order can be amended once (the second try fails), or deleted.
cargo run -p sales -- doc delete sales_order "$NAME" --version 4
```

`doc submit`, `doc cancel` and `doc amend` take `--actor` like the other commands and no data; the output of a submittable DocType includes `docstatus` (0, 1 or 2) and `amended_from`. `docstatus` and `amended_from` cannot be set through `--data`.

A DocType file can also say how the desk should show it (UX.1). A field without `label` gets its humanized name (`order_date` is "Order Date"; write `label = "Quantity"` where that is not good English), `read_only = true` marks a field that only hooks may write (a client that sends a value for it is refused with a 422; it cannot be `required` or a `table`), and `title_field` and `list_fields` name the field that names a document and the columns of the list:

```toml
name = "sales_order"
label = "Sales Order"
title_field = "customer"
list_fields = ["customer", "order_date", "total"]

[[fields]]
name = "total"
type = "decimal"
precision = 12
scale = 2
read_only = true   # computed by a hook; a client must not send it
```

DocTypes are TOML files, one per DocType, in the `doctypes/` directory of an app (embedded in its binary) or in the directory given by `--doctypes` of the stock binary (see [examples/sales/doctypes/customer.toml](examples/sales/doctypes/customer.toml) and the [file format](docs/blueprint/architecture.md#doctype-file-format)). A `link` field names its target DocType (`target = "customer_group"`); in JSON a link is the target document's name, and the database enforces it with a foreign key. A `table` field names a child DocType (`is_child = true`, see [sales_order.toml](examples/sales/doctypes/sales_order.toml) and [sales_order_item.toml](examples/sales/doctypes/sales_order_item.toml)); in JSON it is an array of row objects. A child DocType belongs to exactly one parent and has no commands of its own. A `submittable = true` DocType also has a docstatus and `doc submit`, `doc cancel` and `doc amend` (next section); `naming = { series = "SO-.YYYY.-.#####" }` names documents `SO-2026-00001`, `SO-2026-00002`, ... from a counter that is incremented in the same transaction as the insert, so numbers have no gaps per rendered prefix (date parts are the UTC date of the transaction; DocTypes whose patterns render the same prefix share one counter and see gaps, so give each DocType a distinct prefix). `--data -` reads the JSON from standard input. Decimal values are JSON strings (any form `rust_decimal` parses, such as `"1e3"` or `".5"`), dates are `YYYY-MM-DD`, datetimes are RFC 3339 with microsecond precision (finer digits are truncated). Duplicate JSON keys: the last one wins. Output keys are sorted alphabetically. DocType files: `*.toml` directly in the directory, hidden files skipped, not recursive. The tool prints the stored document as JSON; errors go to standard error as `error: ...` with exit code 1 (2 for a usage error). The `--actor` option (default `cli`) names the trusted **system context** the `doc` commands run as: it is recorded as owner / modified_by on insert, update, submit, cancel and amend, and given to the hooks (on delete it is only given to hooks, not stored); the tool has no authentication, and no permission rule applies to it (see "Permissions" below). The stock `forgebase` binary registers no lifecycle hooks; hooks are Rust code of an app built with the `forgebase` crate, which is why the `sales` binary computes `amount` and `total` and checks submits (see [Apps](#apps) and [ADR 0009](docs/adr/0009-lifecycle-pipeline-and-hooks.md)).

### List documents

`doc list` prints a page of documents as a JSON array (default 20, at most 500; child table rows are not listed). `--filter` is a JSON array of conditions combined with AND, `--sort` can be repeated (`FIELD` or `FIELD:desc`; `name` is always the last key, so paging is stable), `--limit` and `--offset` page. Operators: `=`, `!=`, `<`, `<=`, `>`, `>=`, `in` (the value is an array), `like` (text fields only; a SQL pattern with `%` and `_`), `is null` and `is not null`. Values are written as in `--data`; a link is filtered by the target's name; the standard columns `name`, `owner`, `modified_by`, `created_at`, `modified_at`, `version` and (submittable DocTypes) `docstatus` can be used too.

```sh
cargo run -p sales -- doc list customer \
  --filter '[{"field":"tier","op":"=","value":"gold"},{"field":"loyalty_points","op":">=","value":10}]' \
  --sort loyalty_points:desc --limit 10
```

### Permissions

A DocType file can list `[[permissions]]` (see [sales_order.toml](examples/sales/doctypes/sales_order.toml)): a `role`, the rights it grants (`read`, `write`, `create`, `delete`, `submit`, `cancel`, `amend`) and optionally `if_owner = true` (the rights then apply to documents the user owns only). A user may do what any of their roles grants; the role `Administrator` may do everything; **a DocType without rules is closed to everyone else**. Child rows follow their parent. The rules are enforced by `forgebase-db` in every operation (`insert`, `get`, `update`, `delete`, `submit`, `cancel`, `amend`, `list`), which take a `Caller`: a user with roles (`AuthUser::caller()`), or the trusted system context. A document a user is not allowed to read is treated as if it did not exist (`get`, `list`, updates, links, errors), with the known limits listed in [ADR 0011](docs/adr/0011-permission-model-phase-1.md). The `forgebase doc` commands run as the system context; the rules apply to real users of the [HTTP API](#http-api) (and to Rust code that calls `forgebase_db` with `Caller::user(...)`). See [ADR 0011](docs/adr/0011-permission-model-phase-1.md) and the [security model](docs/blueprint/security-model.md).

### Users and tokens

Passwords are read from the first line of **standard input**, never from an argument or an environment variable. API tokens are printed once as `key:secret`; only a digest is stored, so the secret cannot be shown again. Reading a password from stdin does not hide it: on a terminal it is echoed as you type, so use `read -s` (see below) or a pipe. These commands have no authentication of their own: whoever can run `forgebase` with the database URL can create users and tokens.

```sh
# Create a user with two roles (usernames: 1-140 ASCII letters, digits and _ . @ -; passwords: 8-1024 bytes)
printf '%s\n' 'a long password' | cargo run -p sales -- user create ann --role Sales --role Accounts
# Without echo on a terminal: read -s -r PW && printf '%s\n' "$PW" | cargo run -p sales -- user create ann
cargo run -p sales -- user add-role ann Admin
cargo run -p sales -- user remove-role ann Admin
printf '%s\n' 'another password' | cargo run -p sales -- user set-password ann   # also ends all sessions
cargo run -p sales -- user disable ann    # sessions and tokens stop working; `user enable` undoes it

# An API token for a machine client: prints key:secret once
cargo run -p sales -- token create ann
cargo run -p sales -- token revoke <key>
```

Five consecutive failed logins for a username (each within 15 minutes of the previous one) lock it for 15 minutes; see [ADR 0010](docs/adr/0010-authentication-and-sessions.md) and the [security model](docs/blueprint/security-model.md).

### HTTP API

`serve` (here `cargo run -p sales -- serve`) runs the API on `FORGEBASE_LISTEN` (default `127.0.0.1:8000`). It serves the same DocTypes as `migrate` (the app's, or the `--doctypes` directory in the stock binary) but **does not run migrations**: run `migrate` first. Requests need a `Host` header from `FORGEBASE_ALLOWED_HOSTS` (comma separated host names without ports, default `localhost,127.0.0.1`; an entry with a port is refused at startup). Ctrl-C or SIGTERM stops accepting connections and lets running requests finish for up to 10 seconds; a second signal, or the end of that period, exits at once (exit code 0; open database transactions are rolled back by PostgreSQL when the connection closes). The session cookie is `Secure`, so browsers send it over HTTPS only: put a TLS-terminating reverse proxy in front (the application has no TLS). For local development over plain HTTP, `FORGEBASE_DEV_INSECURE_COOKIES=1` omits `Secure` (and prints a warning); never use it in production. There is no CORS: serve the front-end from the same origin.

```sh
# Prepare: tables, a user with the Administrator role (the example DocTypes name other roles; Administrator may do everything)
cargo run -p sales -- migrate
printf '%s\n' 'a long password' | cargo run -p sales -- user create root --role Administrator

# Terminal 1: serve (development mode)
FORGEBASE_DEV_INSECURE_COOKIES=1 cargo run -p sales -- serve

# Terminal 2: log in. The response has the one-time CSRF token; the session cookie is saved in jar.txt.
CSRF=$(curl -s -c jar.txt -H 'Content-Type: application/json' \
  -d '{"username": "root", "password": "a long password"}' \
  http://localhost:8000/api/v1/login | sed -n 's/.*"csrf_token":"\([0-9a-f]*\)".*/\1/p')

# Create (201). Changing requests with the cookie need the CSRF header; reads do not.
curl -s -b jar.txt -H "X-Forgebase-CSRF: $CSRF" -H 'Content-Type: application/json' \
  -d '{"email": "ann@example.com", "full_name": "Ann", "tier": "gold", "loyalty_points": 12}' \
  http://localhost:8000/api/v1/resource/customer

# List with filters (a JSON array like `doc list --filter`), sort, limit and offset
curl -s -b jar.txt -G http://localhost:8000/api/v1/resource/customer \
  --data-urlencode 'filters=[{"field":"tier","op":"=","value":"gold"}]' \
  --data-urlencode 'sort=loyalty_points:desc' --data-urlencode 'limit=10'

# Read, then change with the version you read (PATCH needs {"version", "data"}; a stale version is 409 `conflict`)
curl -s -b jar.txt http://localhost:8000/api/v1/resource/customer/ann@example.com
curl -s -b jar.txt -X PATCH -H "X-Forgebase-CSRF: $CSRF" -H 'Content-Type: application/json' \
  -d '{"version": 1, "data": {"loyalty_points": 20}}' \
  http://localhost:8000/api/v1/resource/customer/ann@example.com

# Create a sales order (its name comes from the series; use the `name` in the response below), then submit it (submit, cancel and amend take {"version": N}; amend answers 201 with the new draft)
curl -s -b jar.txt -H "X-Forgebase-CSRF: $CSRF" -H 'Content-Type: application/json' \
  -d '{"customer": "ann@example.com", "items": [{"description": "Widget", "qty": 2, "rate": "9.50"}]}' \
  http://localhost:8000/api/v1/resource/sales_order
curl -s -b jar.txt -X POST -H "X-Forgebase-CSRF: $CSRF" -H 'Content-Type: application/json' \
  -d '{"version": 1}' http://localhost:8000/api/v1/resource/sales_order/SO-2026-00001/submit

# Delete (204; the version goes in the query string). Here it is refused with 409 `linked_document_exists`, because the sales order links to the customer
curl -s -b jar.txt -X DELETE -H "X-Forgebase-CSRF: $CSRF" \
  'http://localhost:8000/api/v1/resource/customer/ann@example.com?version=2'

# Log out (204; clears the cookie and ends the session)
curl -s -b jar.txt -X POST -H "X-Forgebase-CSRF: $CSRF" http://localhost:8000/api/v1/logout

# A machine client uses an API token instead: no cookie, no CSRF header. The token header wins over any cookie.
TOKEN=$(cargo run -q -p sales -- token create root)    # prints key:secret once
curl -s -H "Authorization: token $TOKEN" http://localhost:8000/api/v1/resource/customer
```

Responses are JSON: `{"data": ...}` on success and `{"error": {"code", "message", "details"?}}` on failure, with stable codes (for example `unauthenticated` 401, `permission_denied` 403, `csrf_failed` 403, `not_found` 404, `conflict` 409, `validation_failed` 422, `invalid_query` 400). A document you cannot read is a 404, like a missing one. The route table, the request pipeline, the codes and the limits are in the [architecture](docs/blueprint/architecture.md#7-api-rest-resource-api-implemented-in-m33-metadata-and-session-routes-in-m41-the-rest-proposed) and the [security model](docs/blueprint/security-model.md#the-http-layer-implemented-in-m33). `/api/v1/method` (custom endpoints) is not implemented.

### Desk

The desk is a React + TypeScript app in `desk/` (no UI component library; only `react` and `react-dom` at runtime). `forgebase serve` serves the **built** desk on the same origin when `FORGEBASE_DESK_DIR` is set (unset: no desk, `/desk/*` is a 404). It has a login page, a home of cards per app, a sidebar of the DocTypes the user can read grouped by app, breadcrumbs, a **list view** per DocType (the title of each document, the columns the DocType file chooses, a status badge, a quick search, filters that apply as you change them, paging, sorting) and a **form view** generated from the DocType metadata: native controls per field type (a Link field suggests names as you type, a Table field is a grid of rows), Save (only the changed fields are sent), and for a submittable DocType the **Submit**, **Cancel**, **Amend** and **Delete** buttons, shown only when the user has the right and the document's status allows it (the API enforces every right again). Field errors of the server are shown on the fields, a version conflict offers a Reload and keeps your edits, and leaving a form with unsaved changes asks first. Save is the main action (Ctrl/Cmd+S works too), Submit is the main action of a saved draft, and Cancel, Amend and Delete are in a More menu and ask first; a toast confirms what was done. The look is a navy corporate theme (the colour of the logo banner) built with Tailwind CSS v4 at build time (a development tool: the shipped desk is one static CSS file and the build is unchanged, `npm ci && npm run build`; [ADR 0017](docs/adr/0017-desk-styling-tailwind.md)); the layout works from 320 px up.

```sh
# Build the desk (Node.js ^20.19.0 || >=22.12.0; desk/.npmrc ignores dependency install scripts): writes desk/dist
cd desk && npm ci && npm run build && cd ..

# Run it with the API from the HTTP API section above (a user and the tables must exist), then open http://localhost:8000/desk/
# FORGEBASE_DEV_INSECURE_COOKIES=1 is for plain HTTP on localhost only; behind HTTPS leave it out.
FORGEBASE_DEV_INSECURE_COOKIES=1 FORGEBASE_DESK_DIR=desk/dist cargo run -p sales -- serve

# Develop the desk with hot reload (proxies /api to a running `forgebase serve` on port 8000): http://localhost:5173/desk/
cd desk && npm run dev
```

The desk keeps no secret in the browser except the HttpOnly session cookie: the CSRF token lives in memory and is recovered after a reload with `GET /api/v1/session`. What the desk may show is decided by `GET /api/v1/meta` and `GET /api/v1/meta/{doctype}` (only DocTypes the user can read, with the user's rights); the API enforces every right again. A document named `.` or `..` cannot be opened in the desk (a browser resolves those path segments before the request is sent); every other name is URL-encoded. The routes, the form view design, the static-serving rules and the Content-Security-Policy are in the [architecture](docs/blueprint/architecture.md#11-desk-reports-workflow) and the [security model](docs/blueprint/security-model.md#the-http-layer-implemented-in-m33).

**Smoke test.** One Playwright test (Chromium) drives the built desk against a real server: login, home, list, search, create, save, edit and submit a sales order, the More menu, no horizontal scroll at 375 and 320 px, and a user without the submit right (CI job `e2e`). To run it locally you need a PostgreSQL 16 server, an **empty** scratch database, the built desk and Chromium:

```sh
export FORGEBASE_DATABASE_URL=postgres://forgebase:forgebase@localhost:5432/fb_e2e   # an empty scratch database
cargo build -p forgebase && (cd desk && npm ci && npm run build)
desk/e2e/seed.sh                                   # migrates, creates the users e2e_manager and e2e_clerk and a customer (stock binary and `--doctypes`: it adds a role that only the e2e test uses)
FORGEBASE_DEV_INSECURE_COOKIES=1 FORGEBASE_DESK_DIR=desk/dist target/debug/forgebase serve --doctypes target/e2e/doctypes &
cd desk && npx playwright install chromium && npx playwright test   # or set PLAYWRIGHT_CHROMIUM_EXECUTABLE to an installed Chromium
```

**Screenshots.** `desk/e2e/screenshots.spec.ts` adds a few sample documents (draft, submitted and cancelled orders) and takes the login page, home, list and form at 1280, 800 and 375 px (and the list at 320 px, a submitted form at 375 px); it also checks that no page scrolls sideways on a phone. It is skipped unless `SCREENSHOTS=1`, so CI does not run it, and the images are not committed:

```sh
cd desk && SCREENSHOTS=1 SCREENSHOT_DIR=/tmp/desk-shots npx playwright test screenshots   # same server and database as the smoke test
```

See [CONTRIBUTING.md](CONTRIBUTING.md) for the local checks. To get oriented, read the blueprint in this order: [vision](docs/blueprint/vision.md), [architecture](docs/blueprint/architecture.md), [milestones](docs/blueprint/milestones.md). Contributors and agents must follow [AGENTS.md](AGENTS.md).

### Apps

The stock `forgebase` binary has no apps: it loads DocTypes from `--doctypes DIR` (the quick start above uses the binary of an example app instead). Two example apps in this repository show what an app looks like, using the public `forgebase` API only (they change nothing under `crates/`): [`examples/sales`](examples/sales/README.md) (DocTypes, hooks that compute and refuse, a data migration, tests) and [`examples/sales_extension`](examples/sales_extension/README.md) (a custom app that depends on `sales`: a DocType that links to one of `sales`, and a hook on a DocType of `sales`; `cargo run -p sales_extension` is the composed binary). To build an application, make a crate that depends on `forgebase` (the only public API; `forgebase-core`, `forgebase-db` and `forgebase-http` are internal) and write apps and a binary:

```rust
// An app is a plain function that returns data: no registry, no macros that generate code.
pub fn app() -> forgebase::App {
    forgebase::App::new("sales")
        .version(env!("CARGO_PKG_VERSION"))
        .depends_on(&["crm"])
        .doctypes(forgebase::doctypes!["doctypes/sales_order.toml"])
        .hooks("sales_order", OrderHooks)
        .migrations(forgebase::migrations![(1, "backfill", "migrations/0001_backfill.sql")])
}

fn main() -> std::process::ExitCode {
    forgebase::Forgebase::new().app(crm::app()).app(sales::app()).run() // dependencies first
}
```

The binary has the same commands as the stock one (`migrate` also applies the app migrations). The stock binary has no apps, so it reports `"app": "local"` for every DocType in the metadata API (the `--doctypes` directory is the app `local`). Everything about how apps load, depend on each other, own DocTypes, order their hooks and migrate is in [ADR 0012](docs/adr/0012-app-model-and-public-api-boundary.md) and [the architecture](docs/blueprint/architecture.md#2-extension-model-accepted-adr-0006).

To start an app, generate one (a crate with a sample DocType, a hook, tests, a README and an `AGENTS.md` for its maintainers; it builds and passes its tests as created):

```sh
cargo build -p forgebase
target/debug/forgebase new-app crm --forgebase-path . --dir ~/apps    # creates ~/apps/crm
cd ~/apps/crm && FORGEBASE_TEST_DATABASE_URL=postgres://forgebase:forgebase@localhost:5432/postgres cargo test
```

The step-by-step guide (run it, extend another app, upgrade with migrations, common errors) is [docs/guide/first-app.md](docs/guide/first-app.md).

### Background jobs

An app registers a job with `App::job`; a hook, another job or the command line enqueues it, and `forgebase worker` runs it. A job runs in **one database transaction together with its completion**; a failure is retried (5 attempts, 30 s doubling up to 1 h, 5 min timeout) and then the job is `dead`. Delivery is at-least-once, so effects outside the database must be safe to repeat. A hook's `ctx.enqueue(..)` is written in the same transaction as the document change (the transactional outbox), so the job exists if and only if the change is committed. Never put a secret in a payload: it is stored in clear text. Design: [ADR 0013](docs/adr/0013-jobs-scheduler-and-transactional-outbox.md).

```rust
use forgebase::{App, JobContext, JobFuture, JobHandler};

struct Welcome;

impl JobHandler for Welcome {
    fn run<'a>(&'a self, ctx: &'a mut JobContext<'_>) -> JobFuture<'a> {
        Box::pin(async move {
            let name = ctx.payload()["customer"].as_str().unwrap_or_default().to_owned();
            // The system context, as `job:crm.welcome`, inside this job's transaction.
            let _customer = ctx.documents().get("customer", &name).await?;
            Ok(())
        })
    }
}

pub fn app() -> App {
    App::new("crm").job("welcome", Welcome) // the job is `crm.welcome`
}
// in a hook: ctx.enqueue("crm.welcome", forgebase::serde_json::json!({"customer": doc_name}))?;
```

```sh
forgebase migrate                                   # creates the job table (migration 0005)
forgebase worker --concurrency 4                    # runs jobs until Ctrl-C or SIGTERM
forgebase job enqueue crm.welcome --payload '{"customer":"c-1"}'   # or --payload - for stdin
forgebase job list --status dead                    # id, name, status, attempts, run_at, last error
forgebase job show <id>                             # every field as JSON, the payload included
forgebase job retry <id>                            # a dead job goes back to the queue
```

The first signal stops the worker from claiming and lets running jobs finish for 30 seconds (then they are given back, without counting an attempt); a second signal exits at once, and another worker recovers what was running when its lease expires. In an app's tests, `forgebase::testing::run_jobs(&binary, &db)` runs the jobs that are due. Two complete examples: `cargo run -p sales -- worker` runs the log and daily-summary jobs of [`examples/sales`](examples/sales/README.md) (and `cargo run -p sales_extension -- worker` those of both example apps), after `migrate`.

#### Scheduled jobs

An app runs one of its own jobs on a schedule: every N minutes, every day at a time, or every week on a day at a time. **All times are UTC**; there is no cron and there are no time zones.

```rust
use forgebase::{App, Schedule, Weekday};

App::new("crm")
    .job("send_digest", SendDigest)
    .schedule("send_digest", Schedule::daily_at(6, 30))
    .job("weekly_report", WeeklyReport)
    .schedule("weekly_report", Schedule::weekly_on(Weekday::Monday, 7, 0))
```

`Schedule::every_minutes(n)` (1 to 1440) ticks at the times where the minutes since the Unix epoch are divisible by `n`: every 15 gives :00, :15, :30 and :45. Run `forgebase migrate` after you add, change or remove a schedule (it stores the schedules the binary declares and **deletes the others**), and keep `forgebase worker` running: it enqueues each tick once, however many workers run, with the payload `{"scheduled_for": "<the tick, RFC 3339 UTC>"}` (use it for idempotency). Ticks missed while no worker ran are not caught up: one job runs, then the next future time. In tests, `forgebase::testing::run_schedules_at(&binary, &db, at)` enqueues what is due at `at`, and `run_jobs` runs it.

The framework schedules its own clean-up in every binary (app name `forgebase`, reserved): finished jobs, done or dead, 7 days after they finished (daily at 03:00 UTC), stale login-failure rows and expired sessions (every 60 minutes). They need a running worker.

### Sites

One deployment can serve several **sites**, each with its own database (ADR 0014). A single-site setup needs nothing new: without a config file there is one site, `default`, from `FORGEBASE_DATABASE_URL`. For several sites, write a `forgebase.toml` (no secrets: the database URLs come from environment variables) and pass it with `--config forgebase.toml` or `FORGEBASE_CONFIG=forgebase.toml` (it is never searched for in the current directory):

```toml
[[sites]]
name = "acme"
hosts = ["acme.example.com", "acme.localhost"]   # chosen by the Host header, no port
database_url_env = "ACME_DATABASE_URL"
pool_size = 8                                    # optional
apps = ["sales", "sales_extension"]              # optional: the apps installed on this site

[[sites]]
name = "beta"
hosts = ["beta.example.com"]
database_url_env = "BETA_DATABASE_URL"
```

```bash
export ACME_DATABASE_URL=postgres://user:password@localhost:5432/acme
export BETA_DATABASE_URL=postgres://user:password@localhost:5432/beta
forgebase --config forgebase.toml site list                                           # sites, hosts, apps; never the URL
forgebase --config forgebase.toml site create acme                                    # CREATE DATABASE, then migrate
forgebase --config forgebase.toml migrate --site acme --doctypes examples/sales/doctypes
forgebase --config forgebase.toml user create root --role Administrator --site acme   # password on stdin
forgebase --config forgebase.toml serve --doctypes examples/sales/doctypes            # serves all sites by Host
forgebase --config forgebase.toml worker --site acme --doctypes examples/sales/doctypes
```

`--site NAME` is required on every database command except `serve` when the file has several sites, and optional with one site. `serve` without `--site` serves all sites, one pool each; with `--site` only that site. A well-formed `Host` that no site owns is `404`, a malformed one `400`; `X-Forwarded-Host` is ignored, so the reverse proxy must pass the original `Host`. **There is one worker process per site** (`forgebase worker --site NAME`; the scheduler runs in it), and `migrate` migrates one site at a time. Running a deployment (reverse proxy, workers, upgrades, roles) is in the [operations guide](docs/guide/operations.md).

**Apps per site.** `apps` lists the apps of the binary that are installed on a site; absent means all of them. The list must be closed under `depends_on` (a site that lists `sales_extension` must list `sales`: dependencies are never added for you), may be empty (only auth, users and the built-in jobs) and names only apps the binary has; every mistake is reported at once, naming the site. A site's registry has only its apps' DocTypes, hooks, jobs and schedules, so `migrate`, `doc`, `job enqueue`, the meta API, the resources and the worker of a site all see the same subset: a DocType of an app that is not installed is unknown (`404` over HTTP). The `--doctypes DIR` app `local` is in every site and depends on that site's apps. **Removing an app from a site's list keeps its tables and data** (there is no uninstall): `migrate` warns once per such app, its schedules are deleted, and its queued jobs stay queued (the worker does not claim names it does not know) until the app is installed again. **The kept rows of a removed app still hold their links:** a document of an installed app that they link to cannot be deleted (the error says it is linked from another document, without naming the DocType, which the site no longer knows) until the app is installed again and its rows are removed or changed.

**`site list` and `site create`.** `forgebase site list` prints each site's name, hosts, apps and the **name** of its database variable (never the URL) and needs no database. `forgebase site create NAME` runs `CREATE DATABASE` for the database named in the site's URL, connecting to the maintenance database `postgres` of the same server (`--maintenance-db NAME` to use another), and then migrates it like `migrate --site NAME`. The role in the URL needs the `CREATEDB` privilege. It refuses, with exit 1, a database that exists already (run `migrate --site NAME` instead: an existing database is never migrated implicitly). If the migrate fails after the create, the database is kept: fix the cause and run `migrate --site NAME`. With an existing database and a role without `CREATEDB`, `site create` reports "permission denied" (PostgreSQL checks the privilege first). `site create NAME --empty` creates the database and does not migrate it (for a restore); it uses no apps.

**Backup and restore.** They use the PostgreSQL client tools `pg_dump` and `pg_restore` (not bundled; the server's major version, on `PATH` or in `FORGEBASE_PG_BIN`). The connection reaches them through `PG*` environment variables, never on the command line.

```bash
forgebase --config forgebase.toml site backup acme --out acme.dump          # never overwrites; mode 0600
forgebase --config forgebase.toml site create acme2 --empty                 # acme2: another site of the file; a new, empty database
forgebase --config forgebase.toml site restore acme2 --from acme.dump       # only into an empty database; one transaction
forgebase --config forgebase.toml migrate --site acme2 --doctypes examples/sales/doctypes   # if the binary is newer than the backup
```

A backup holds everything in the database, **password hashes, sessions and API token digests included**: protect the file. A restore refuses a database that is not empty (and `site create` without `--empty` migrates, so its database is not empty); it never migrates by itself. Details, scheduling and retention: [operations guide](docs/guide/operations.md#backups).

## License

MIT. See [LICENSE](LICENSE).
