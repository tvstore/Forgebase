# Forgebase

Forgebase is a Rust framework for building business applications, inspired by the concepts and capabilities of the Frappe Framework. It is an independent implementation: no Frappe code is copied.

> Build for longevity, not complexity. Maintenance is the most expensive part of software ownership.

## Status

**M0 Foundations and M1 Metadata and storage are done** (M1.1: DocType metadata model and document validation in `forgebase-core`; M1.2: PostgreSQL pool, framework migrations and additive schema sync in `forgebase-db`; M1.3: document insert/get/update/delete in `forgebase-db` and the `forgebase` command-line tool). There is no HTTP API, authentication or permission system yet (M3). The workspace has four crates, CI and a toolchain pin. The documents below record research, proposed architecture and open decisions. ADRs 0002-0006 (workspace layout, PostgreSQL only, database access direction, HTTP stack, extension model) are Accepted; most other decisions are **Proposed**, not final. Each document marks its status.

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
| [examples/doctypes/](examples/doctypes/customer.toml) | Example DocType used by the quick start |
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

# Insert a document; the name comes from the DocType's naming field (email)
cargo run -p forgebase-cli -- doc insert customer --doctypes examples/doctypes \
  --data '{"email": "ann@example.com", "full_name": "Ann", "credit_limit": "1250.50"}'

# Read it, change it, delete it
cargo run -p forgebase-cli -- doc get customer ann@example.com --doctypes examples/doctypes
cargo run -p forgebase-cli -- doc update customer ann@example.com --doctypes examples/doctypes \
  --data '{"loyalty_points": 10}'
cargo run -p forgebase-cli -- doc delete customer ann@example.com --doctypes examples/doctypes
```

DocTypes are TOML files, one per DocType, in the directory given by `--doctypes` (see [examples/doctypes/customer.toml](examples/doctypes/customer.toml) and the [file format](docs/blueprint/architecture.md#doctype-file-format-m1)). `--data -` reads the JSON from standard input. Decimal values are JSON strings (any form `rust_decimal` parses, such as `"1e3"` or `".5"`), dates are `YYYY-MM-DD`, datetimes are RFC 3339 with microsecond precision (finer digits are truncated). Duplicate JSON keys: the last one wins. Output keys are sorted alphabetically. DocType files: `*.toml` directly in the directory, hidden files skipped, not recursive. The tool prints the stored document as JSON; errors go to standard error as `error: ...` with exit code 1 (2 for a usage error). The `--actor` option (default `cli`) is only recorded as owner / modified_by; the tool has no authentication.

See [CONTRIBUTING.md](CONTRIBUTING.md) for the local checks. To get oriented, read the blueprint in this order: [vision](docs/blueprint/vision.md), [architecture](docs/blueprint/architecture.md), [milestones](docs/blueprint/milestones.md). Contributors and agents must follow [AGENTS.md](AGENTS.md).

## License

MIT. See [LICENSE](LICENSE).
