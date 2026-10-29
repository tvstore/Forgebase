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

**M0 Foundations, M1 Metadata and storage, M2 Relationships and lifecycle and M3 Auth, permissions and REST API are done; M4 Minimal Desk is in progress** (M4.1: the metadata API, the `desk/` React app with login, home and list view, served by `forgebase serve`; M4.2, the form view, is next) (M1.1: DocType metadata model and document validation in `forgebase-core`; M1.2: PostgreSQL pool, framework migrations and additive schema sync in `forgebase-db`; M1.3: document insert/get/update/delete in `forgebase-db` and the `forgebase` command-line tool). M2 was delivered in four PRs: M2.1 adds Link fields (foreign keys) and optimistic concurrency (`update` and `delete` need the expected version); M2.2 adds child tables (Table fields: rows saved with their parent in one transaction); M2.3 adds the lifecycle pipeline and hooks (`DocTypeHooks`, registered by the app; see ADR 0009); M2.4 adds docstatus (`submittable` DocTypes with `submit`, `cancel` and `amend`) and naming series (counters gapless per prefix). M3 was delivered in three PRs: M3.1 adds users, roles, Argon2id passwords, server-side sessions, API tokens and login lockout (`forgebase_db::auth`, ADR 0010) and the `forgebase user` and `forgebase token` commands; M3.2 adds role permissions (`[[permissions]]` in the DocType file, enforced in every document operation, deny by default; ADR 0011) and the list query (`forgebase_db::list`, `forgebase doc list`); M3.3 adds the HTTP API (`forgebase serve`: `/api/v1/resource`, login and logout, cookie sessions with CSRF protection, API tokens, `Host` validation, no CORS). The workspace has four crates, CI and a toolchain pin. The documents below record research, proposed architecture and open decisions. ADRs 0002-0006 (workspace layout, PostgreSQL only, database access direction, HTTP stack, extension model) are Accepted; most other decisions are **Proposed**, not final. Each document marks its status.

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
| [examples/doctypes/](examples/doctypes/customer.toml) | Example DocTypes used by the quick start (a link, a child table) |
| [docs/adr/](docs/adr/README.md) | Architecture Decision Records |

## Getting started

```sh
cargo build
cargo run -p forgebase-cli -- --version
```

### Quick start

Needs a PostgreSQL 16 server and an empty database. Connections are not encrypted yet (no TLS), so use a local or private network.

```sh
export FORGEBASE_DATABASE_URL=postgres://user:password@localhost:5432/mydb

# Create the framework tables and the DocType tables (add --dry-run to only print the plan; on a fresh database it shows the pending framework migrations only, the DocType plan appears once they are applied)
cargo run -p forgebase-cli -- migrate --doctypes examples/doctypes

# Insert a customer group, then a customer that links to it by name (the name comes from the DocType's naming field)
cargo run -p forgebase-cli -- doc insert customer_group --doctypes examples/doctypes \
  --data '{"title": "retail"}'
cargo run -p forgebase-cli -- doc insert customer --doctypes examples/doctypes \
  --data '{"email": "ann@example.com", "full_name": "Ann", "credit_limit": "1250.50", "customer_group": "retail"}'

# Read it (note its "version"), change it, then delete it (below). update and delete need --version, the version you last read;
# if someone changed the document meanwhile they fail with a conflict that shows the current version.
cargo run -p forgebase-cli -- doc get customer ann@example.com --doctypes examples/doctypes
cargo run -p forgebase-cli -- doc update customer ann@example.com --version 1 --doctypes examples/doctypes \
  --data '{"loyalty_points": 10}'

# A customer group that a customer still links to cannot be deleted: this command fails (exit code 1, the error names `customer`)
cargo run -p forgebase-cli -- doc delete customer_group retail --version 1 --doctypes examples/doctypes
# Delete the customer first, then the group can be deleted
cargo run -p forgebase-cli -- doc delete customer ann@example.com --version 2 --doctypes examples/doctypes
cargo run -p forgebase-cli -- doc delete customer_group retail --version 1 --doctypes examples/doctypes

# A sales order has a child table, `items`: the rows are an array in the same JSON and are saved with the order in one transaction.
# Its name is generated (a UUID); the output shows it, the rows in order and the version. The sed keeps just the name for the next commands.
cargo run -p forgebase-cli -- doc insert customer --doctypes examples/doctypes \
  --data '{"email": "bob@example.com", "full_name": "Bob"}'
ORDER=$(cargo run -q -p forgebase-cli -- doc insert sales_order --doctypes examples/doctypes \
  --data '{"customer": "bob@example.com", "order_date": "2026-10-28", "items": [
    {"description": "Widget", "qty": 2, "rate": "9.50"},
    {"description": "Gadget", "qty": 1}]}' | sed -n 's/^  "name": "\(.*\)",$/\1/p')

# An update that names `items` replaces its rows when they differ from the stored ones (a table with identical rows keeps its row ids); one that leaves it out keeps them.
cargo run -p forgebase-cli -- doc update sales_order "$ORDER" --version 1 --doctypes examples/doctypes \
  --data '{"items": [{"description": "Widget", "qty": 5, "rate": "9.50"}]}'
cargo run -p forgebase-cli -- doc delete sales_order "$ORDER" --version 2 --doctypes examples/doctypes
```

### Submit, cancel and amend

`sales_order` is `submittable` and uses a naming series ([sales_order.toml](examples/doctypes/sales_order.toml)): a new order is a **draft** (`docstatus` 0, named `SO-<year>-00001`, ...), `submit` locks it (`docstatus` 1; only fields marked `allow_on_submit`, here `remarks`, can still change), `cancel` makes it `docstatus` 2, and `amend` creates a new draft from a cancelled order, named `<original>-<n>` with `amended_from` set. Every step needs `--version`, the version you last read (each one adds 1).

```sh
# Continues the example above (the `bob@example.com` customer exists). A new order is a draft: "docstatus": 0, named by the series.
NAME=$(cargo run -q -p forgebase-cli -- doc insert sales_order --doctypes examples/doctypes \
  --data '{"customer": "bob@example.com", "items": [{"description": "Widget", "qty": 2}]}' | sed -n 's/^  "name": "\(.*\)",$/\1/p')

# Submit it (version 1 -> 2): "docstatus": 1. A submitted order is locked: this update fails (exit code 1) ...
cargo run -p forgebase-cli -- doc submit sales_order "$NAME" --version 1 --doctypes examples/doctypes
cargo run -p forgebase-cli -- doc update sales_order "$NAME" --version 2 --doctypes examples/doctypes \
  --data '{"order_date": "2026-11-01"}'
# ... and this one, on the allow_on_submit field, works (version 2 -> 3). A submitted order cannot be deleted either.
cargo run -p forgebase-cli -- doc update sales_order "$NAME" --version 2 --doctypes examples/doctypes \
  --data '{"remarks": "rush delivery"}'

# Cancel it (3 -> 4: "docstatus": 2), then amend it: a new draft with the same values and rows, named "<NAME>-1", "amended_from": "<NAME>".
cargo run -p forgebase-cli -- doc cancel sales_order "$NAME" --version 3 --doctypes examples/doctypes
cargo run -p forgebase-cli -- doc amend sales_order "$NAME" --version 4 --doctypes examples/doctypes
# The amendment is a draft again: edit it, submit it, and so on. A cancelled order can be amended once (the second try fails), or deleted.
cargo run -p forgebase-cli -- doc delete sales_order "$NAME" --version 4 --doctypes examples/doctypes
```

`doc submit`, `doc cancel` and `doc amend` take `--actor` like the other commands and no data; the output of a submittable DocType includes `docstatus` (0, 1 or 2) and `amended_from`. `docstatus` and `amended_from` cannot be set through `--data`.

DocTypes are TOML files, one per DocType, in the directory given by `--doctypes` (see [examples/doctypes/customer.toml](examples/doctypes/customer.toml) and the [file format](docs/blueprint/architecture.md#doctype-file-format)). A `link` field names its target DocType (`target = "customer_group"`); in JSON a link is the target document's name, and the database enforces it with a foreign key. A `table` field names a child DocType (`is_child = true`, see [sales_order.toml](examples/doctypes/sales_order.toml) and [sales_order_item.toml](examples/doctypes/sales_order_item.toml)); in JSON it is an array of row objects. A child DocType belongs to exactly one parent and has no commands of its own. A `submittable = true` DocType also has a docstatus and `doc submit`, `doc cancel` and `doc amend` (next section); `naming = { series = "SO-.YYYY.-.#####" }` names documents `SO-2026-00001`, `SO-2026-00002`, ... from a counter that is incremented in the same transaction as the insert, so numbers have no gaps per rendered prefix (date parts are the UTC date of the transaction; DocTypes whose patterns render the same prefix share one counter and see gaps, so give each DocType a distinct prefix). `--data -` reads the JSON from standard input. Decimal values are JSON strings (any form `rust_decimal` parses, such as `"1e3"` or `".5"`), dates are `YYYY-MM-DD`, datetimes are RFC 3339 with microsecond precision (finer digits are truncated). Duplicate JSON keys: the last one wins. Output keys are sorted alphabetically. DocType files: `*.toml` directly in the directory, hidden files skipped, not recursive. The tool prints the stored document as JSON; errors go to standard error as `error: ...` with exit code 1 (2 for a usage error). The `--actor` option (default `cli`) names the trusted **system context** the `doc` commands run as: it is recorded as owner / modified_by on insert, update, submit, cancel and amend, and given to the hooks (on delete it is only given to hooks, not stored); the tool has no authentication, and no permission rule applies to it (see "Permissions" below). The tool registers no lifecycle hooks: hooks are Rust code of an app that links `forgebase-db` (see [ADR 0009](docs/adr/0009-lifecycle-pipeline-and-hooks.md)).

### List documents

`doc list` prints a page of documents as a JSON array (default 20, at most 500; child table rows are not listed). `--filter` is a JSON array of conditions combined with AND, `--sort` can be repeated (`FIELD` or `FIELD:desc`; `name` is always the last key, so paging is stable), `--limit` and `--offset` page. Operators: `=`, `!=`, `<`, `<=`, `>`, `>=`, `in` (the value is an array), `like` (text fields only; a SQL pattern with `%` and `_`), `is null` and `is not null`. Values are written as in `--data`; a link is filtered by the target's name; the standard columns `name`, `owner`, `modified_by`, `created_at`, `modified_at`, `version` and (submittable DocTypes) `docstatus` can be used too.

```sh
cargo run -p forgebase-cli -- doc list customer --doctypes examples/doctypes \
  --filter '[{"field":"tier","op":"=","value":"gold"},{"field":"loyalty_points","op":">=","value":10}]' \
  --sort loyalty_points:desc --limit 10
```

### Permissions

A DocType file can list `[[permissions]]` (see [sales_order.toml](examples/doctypes/sales_order.toml)): a `role`, the rights it grants (`read`, `write`, `create`, `delete`, `submit`, `cancel`, `amend`) and optionally `if_owner = true` (the rights then apply to documents the user owns only). A user may do what any of their roles grants; the role `Administrator` may do everything; **a DocType without rules is closed to everyone else**. Child rows follow their parent. The rules are enforced by `forgebase-db` in every operation (`insert`, `get`, `update`, `delete`, `submit`, `cancel`, `amend`, `list`), which take a `Caller`: a user with roles (`AuthUser::caller()`), or the trusted system context. A document a user is not allowed to read is treated as if it did not exist (`get`, `list`, updates, links, errors), with the known limits listed in [ADR 0011](docs/adr/0011-permission-model-phase-1.md). The `forgebase doc` commands run as the system context; the rules apply to real users of the [HTTP API](#http-api) (and to Rust code that calls `forgebase_db` with `Caller::user(...)`). See [ADR 0011](docs/adr/0011-permission-model-phase-1.md) and the [security model](docs/blueprint/security-model.md).

### Users and tokens

Passwords are read from the first line of **standard input**, never from an argument or an environment variable. API tokens are printed once as `key:secret`; only a digest is stored, so the secret cannot be shown again. Reading a password from stdin does not hide it: on a terminal it is echoed as you type, so use `read -s` (see below) or a pipe. These commands have no authentication of their own: whoever can run `forgebase` with the database URL can create users and tokens.

```sh
# Create a user with two roles (usernames: 1-140 ASCII letters, digits and _ . @ -; passwords: 8-1024 bytes)
printf '%s\n' 'a long password' | cargo run -p forgebase-cli -- user create ann --role Sales --role Accounts
# Without echo on a terminal: read -s -r PW && printf '%s\n' "$PW" | cargo run -p forgebase-cli -- user create ann
cargo run -p forgebase-cli -- user add-role ann Admin
cargo run -p forgebase-cli -- user remove-role ann Admin
printf '%s\n' 'another password' | cargo run -p forgebase-cli -- user set-password ann   # also ends all sessions
cargo run -p forgebase-cli -- user disable ann    # sessions and tokens stop working; `user enable` undoes it

# An API token for a machine client: prints key:secret once
cargo run -p forgebase-cli -- token create ann
cargo run -p forgebase-cli -- token revoke <key>
```

Five consecutive failed logins for a username (each within 15 minutes of the previous one) lock it for 15 minutes; see [ADR 0010](docs/adr/0010-authentication-and-sessions.md) and the [security model](docs/blueprint/security-model.md).

### HTTP API

`forgebase serve` runs the API on `FORGEBASE_LISTEN` (default `127.0.0.1:8000`). It reads the DocType files like `migrate` but **does not run migrations**: run `migrate` first. Requests need a `Host` header from `FORGEBASE_ALLOWED_HOSTS` (comma separated host names without ports, default `localhost,127.0.0.1`; an entry with a port is refused at startup). Ctrl-C or SIGTERM stops accepting connections and lets running requests finish for up to 10 seconds; a second signal, or the end of that period, exits at once (exit code 0; open database transactions are rolled back by PostgreSQL when the connection closes). The session cookie is `Secure`, so browsers send it over HTTPS only: put a TLS-terminating reverse proxy in front (the application has no TLS). For local development over plain HTTP, `FORGEBASE_DEV_INSECURE_COOKIES=1` omits `Secure` (and prints a warning); never use it in production. There is no CORS: serve the front-end from the same origin.

```sh
# Prepare: tables, a user with the Administrator role (the example DocTypes name other roles; Administrator may do everything)
cargo run -p forgebase-cli -- migrate --doctypes examples/doctypes
printf '%s\n' 'a long password' | cargo run -p forgebase-cli -- user create root --role Administrator

# Terminal 1: serve (development mode)
FORGEBASE_DEV_INSECURE_COOKIES=1 cargo run -p forgebase-cli -- serve --doctypes examples/doctypes

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
  -d '{"customer": "ann@example.com", "items": [{"description": "Widget", "qty": 2}]}' \
  http://localhost:8000/api/v1/resource/sales_order
curl -s -b jar.txt -X POST -H "X-Forgebase-CSRF: $CSRF" -H 'Content-Type: application/json' \
  -d '{"version": 1}' http://localhost:8000/api/v1/resource/sales_order/SO-2026-00001/submit

# Delete (204; the version goes in the query string). Here it is refused with 409 `linked_document_exists`, because the sales order links to the customer
curl -s -b jar.txt -X DELETE -H "X-Forgebase-CSRF: $CSRF" \
  'http://localhost:8000/api/v1/resource/customer/ann@example.com?version=2'

# Log out (204; clears the cookie and ends the session)
curl -s -b jar.txt -X POST -H "X-Forgebase-CSRF: $CSRF" http://localhost:8000/api/v1/logout

# A machine client uses an API token instead: no cookie, no CSRF header. The token header wins over any cookie.
TOKEN=$(cargo run -q -p forgebase-cli -- token create root)    # prints key:secret once
curl -s -H "Authorization: token $TOKEN" http://localhost:8000/api/v1/resource/customer
```

Responses are JSON: `{"data": ...}` on success and `{"error": {"code", "message", "details"?}}` on failure, with stable codes (for example `unauthenticated` 401, `permission_denied` 403, `csrf_failed` 403, `not_found` 404, `conflict` 409, `validation_failed` 422, `invalid_query` 400). A document you cannot read is a 404, like a missing one. The route table, the request pipeline, the codes and the limits are in the [architecture](docs/blueprint/architecture.md#7-api-rest-resource-api-implemented-in-m33-the-rest-proposed) and the [security model](docs/blueprint/security-model.md#the-http-layer-implemented-in-m33). `/api/v1/method` (custom endpoints) is not implemented.

### Desk

The desk is a React + TypeScript app in `desk/` (no UI component library; only `react` and `react-dom` at runtime). `forgebase serve` serves the **built** desk on the same origin when `FORGEBASE_DESK_DIR` is set (unset: no desk, `/desk/*` is a 404). M4.1 has the login page, the list of DocTypes the user can read and a list view per DocType (paging, sorting, filters). The form view arrives in M4.2.

```sh
# Build the desk (Node.js ^20.19.0 || >=22.12.0; desk/.npmrc ignores dependency install scripts): writes desk/dist
cd desk && npm ci && npm run build && cd ..

# Run it with the API from the HTTP API section above (a user and the tables must exist), then open http://localhost:8000/desk/
# FORGEBASE_DEV_INSECURE_COOKIES=1 is for plain HTTP on localhost only; behind HTTPS leave it out.
FORGEBASE_DEV_INSECURE_COOKIES=1 FORGEBASE_DESK_DIR=desk/dist cargo run -p forgebase-cli -- serve --doctypes examples/doctypes

# Develop the desk with hot reload (proxies /api to a running `forgebase serve` on port 8000): http://localhost:5173/desk/
cd desk && npm run dev
```

The desk keeps no secret in the browser except the HttpOnly session cookie: the CSRF token lives in memory and is recovered after a reload with `GET /api/v1/session`. What the desk may show is decided by `GET /api/v1/meta` and `GET /api/v1/meta/{doctype}` (only DocTypes the user can read, with the user's rights); the API enforces every right again. The routes, the static-serving rules and the Content-Security-Policy are in the [architecture](docs/blueprint/architecture.md#7-api-rest-resource-api-implemented-in-m33-metadata-and-session-routes-in-m41-the-rest-proposed) and the [security model](docs/blueprint/security-model.md#the-http-layer-implemented-in-m33).

See [CONTRIBUTING.md](CONTRIBUTING.md) for the local checks. To get oriented, read the blueprint in this order: [vision](docs/blueprint/vision.md), [architecture](docs/blueprint/architecture.md), [milestones](docs/blueprint/milestones.md). Contributors and agents must follow [AGENTS.md](AGENTS.md).

## License

MIT. See [LICENSE](LICENSE).
