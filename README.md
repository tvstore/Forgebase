# Forgebase

Forgebase is a Rust framework for building business applications, inspired by the concepts and capabilities of the Frappe Framework. It is an independent implementation: no Frappe code is copied.

> Build for longevity, not complexity. Maintenance is the most expensive part of software ownership.

## Status

**M0 Foundations, M1 Metadata and storage and M2 Relationships and lifecycle are done** (M1.1: DocType metadata model and document validation in `forgebase-core`; M1.2: PostgreSQL pool, framework migrations and additive schema sync in `forgebase-db`; M1.3: document insert/get/update/delete in `forgebase-db` and the `forgebase` command-line tool). M2 was delivered in four PRs: M2.1 adds Link fields (foreign keys) and optimistic concurrency (`update` and `delete` need the expected version); M2.2 adds child tables (Table fields: rows saved with their parent in one transaction); M2.3 adds the lifecycle pipeline and hooks (`DocTypeHooks`, registered by the app; see ADR 0009); M2.4 adds docstatus (`submittable` DocTypes with `submit`, `cancel` and `amend`) and naming series (counters gapless per prefix). M3 is in progress: M3.1 adds users, roles, Argon2id passwords, server-side sessions, API tokens and login lockout (`forgebase_db::auth`, ADR 0010) and the `forgebase user` and `forgebase token` commands; there is no HTTP API or permission system yet (M3.2 and M3.3). The workspace has four crates, CI and a toolchain pin. The documents below record research, proposed architecture and open decisions. ADRs 0002-0006 (workspace layout, PostgreSQL only, database access direction, HTTP stack, extension model) are Accepted; most other decisions are **Proposed**, not final. Each document marks its status.

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
| [docs/blueprint/milestones.md](docs/blueprint/milestones.md) | M0 to M5 and later work, with exit criteria |
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

DocTypes are TOML files, one per DocType, in the directory given by `--doctypes` (see [examples/doctypes/customer.toml](examples/doctypes/customer.toml) and the [file format](docs/blueprint/architecture.md#doctype-file-format)). A `link` field names its target DocType (`target = "customer_group"`); in JSON a link is the target document's name, and the database enforces it with a foreign key. A `table` field names a child DocType (`is_child = true`, see [sales_order.toml](examples/doctypes/sales_order.toml) and [sales_order_item.toml](examples/doctypes/sales_order_item.toml)); in JSON it is an array of row objects. A child DocType belongs to exactly one parent and has no commands of its own. A `submittable = true` DocType also has a docstatus and `doc submit`, `doc cancel` and `doc amend` (next section); `naming = { series = "SO-.YYYY.-.#####" }` names documents `SO-2026-00001`, `SO-2026-00002`, ... from a counter that is incremented in the same transaction as the insert, so numbers have no gaps per rendered prefix (date parts are the UTC date of the transaction; DocTypes whose patterns render the same prefix share one counter and see gaps, so give each DocType a distinct prefix). `--data -` reads the JSON from standard input. Decimal values are JSON strings (any form `rust_decimal` parses, such as `"1e3"` or `".5"`), dates are `YYYY-MM-DD`, datetimes are RFC 3339 with microsecond precision (finer digits are truncated). Duplicate JSON keys: the last one wins. Output keys are sorted alphabetically. DocType files: `*.toml` directly in the directory, hidden files skipped, not recursive. The tool prints the stored document as JSON; errors go to standard error as `error: ...` with exit code 1 (2 for a usage error). The `--actor` option (default `cli`) is recorded as owner / modified_by on insert, update, submit, cancel and amend, and given to the hooks (on delete it is only given to hooks, not stored); the tool has no authentication. The tool registers no lifecycle hooks: hooks are Rust code of an app that links `forgebase-db` (see [ADR 0009](docs/adr/0009-lifecycle-pipeline-and-hooks.md)).

### Users and tokens

Passwords are read from the first line of **standard input**, never from an argument or an environment variable. API tokens are printed once as `key:secret`; only a digest is stored, so the secret cannot be shown again. These commands have no authentication of their own: whoever can run `forgebase` with the database URL can create users and tokens.

```sh
# Create a user with two roles (usernames: 1-140 ASCII letters, digits and _ . @ -; passwords: 8-1024 bytes)
printf '%s\n' 'a long password' | cargo run -p forgebase-cli -- user create ann --role Sales --role Accounts
cargo run -p forgebase-cli -- user add-role ann Admin
cargo run -p forgebase-cli -- user remove-role ann Admin
printf '%s\n' 'another password' | cargo run -p forgebase-cli -- user set-password ann   # also ends all sessions
cargo run -p forgebase-cli -- user disable ann    # sessions and tokens stop working; `user enable` undoes it

# An API token for a machine client: prints key:secret once
cargo run -p forgebase-cli -- token create ann
cargo run -p forgebase-cli -- token revoke <key>
```

Five failed logins for a username lock it for 15 minutes; see [ADR 0010](docs/adr/0010-authentication-and-sessions.md) and the [security model](docs/blueprint/security-model.md).

See [CONTRIBUTING.md](CONTRIBUTING.md) for the local checks. To get oriented, read the blueprint in this order: [vision](docs/blueprint/vision.md), [architecture](docs/blueprint/architecture.md), [milestones](docs/blueprint/milestones.md). Contributors and agents must follow [AGENTS.md](AGENTS.md).

## License

MIT. See [LICENSE](LICENSE).
