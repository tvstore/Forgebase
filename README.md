# Forgebase

Forgebase is a Rust framework for building business applications, inspired by the concepts and capabilities of the Frappe Framework. It is an independent implementation: no Frappe code is copied.

> Build for longevity, not complexity. Maintenance is the most expensive part of software ownership.

## Status

**M0 Foundations and M1 Metadata and storage are done** (M1.1: DocType metadata model and document validation in `forgebase-core`; M1.2: PostgreSQL pool, framework migrations and additive schema sync in `forgebase-db`; M1.3: document insert/get/update/delete in `forgebase-db` and the `forgebase` command-line tool). **M2 Relationships and lifecycle is in progress**: M2.1 adds Link fields (foreign keys) and optimistic concurrency (`update` and `delete` need the expected version); M2.2 adds child tables (Table fields: rows saved with their parent in one transaction); M2.3 adds the lifecycle pipeline and hooks (`DocTypeHooks`, registered by the app; see ADR 0009); docstatus and naming series follow in M2.4. There is no HTTP API, authentication or permission system yet (M3). The workspace has four crates, CI and a toolchain pin. The documents below record research, proposed architecture and open decisions. ADRs 0002-0006 (workspace layout, PostgreSQL only, database access direction, HTTP stack, extension model) are Accepted; most other decisions are **Proposed**, not final. Each document marks its status.

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

DocTypes are TOML files, one per DocType, in the directory given by `--doctypes` (see [examples/doctypes/customer.toml](examples/doctypes/customer.toml) and the [file format](docs/blueprint/architecture.md#doctype-file-format)). A `link` field names its target DocType (`target = "customer_group"`); in JSON a link is the target document's name, and the database enforces it with a foreign key. A `table` field names a child DocType (`is_child = true`, see [sales_order.toml](examples/doctypes/sales_order.toml) and [sales_order_item.toml](examples/doctypes/sales_order_item.toml)); in JSON it is an array of row objects. A child DocType belongs to exactly one parent and has no commands of its own. `--data -` reads the JSON from standard input. Decimal values are JSON strings (any form `rust_decimal` parses, such as `"1e3"` or `".5"`), dates are `YYYY-MM-DD`, datetimes are RFC 3339 with microsecond precision (finer digits are truncated). Duplicate JSON keys: the last one wins. Output keys are sorted alphabetically. DocType files: `*.toml` directly in the directory, hidden files skipped, not recursive. The tool prints the stored document as JSON; errors go to standard error as `error: ...` with exit code 1 (2 for a usage error). The `--actor` option (default `cli`) is recorded as owner / modified_by on insert and update, and given to the hooks (on delete it is only given to hooks, not stored); the tool has no authentication. The tool registers no lifecycle hooks: hooks are Rust code of an app that links `forgebase-db` (see [ADR 0009](docs/adr/0009-lifecycle-pipeline-and-hooks.md)).

See [CONTRIBUTING.md](CONTRIBUTING.md) for the local checks. To get oriented, read the blueprint in this order: [vision](docs/blueprint/vision.md), [architecture](docs/blueprint/architecture.md), [milestones](docs/blueprint/milestones.md). Contributors and agents must follow [AGENTS.md](AGENTS.md).

## License

MIT. See [LICENSE](LICENSE).
