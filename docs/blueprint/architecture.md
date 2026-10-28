# Architecture

M1 is implemented: the metadata model and document validation (`forgebase-core`), the PostgreSQL pool, migrations, schema sync and document CRUD (`forgebase-db`) and the `forgebase` command-line tool (`forgebase-cli`). Of M2, PR M2.1 (Link fields and optimistic concurrency) is implemented; child tables, the lifecycle pipeline with hooks, docstatus and naming series (M2.2 to M2.4) are accepted designs that are not implemented yet. Everything from M3 on is design only. Sections and statements marked Accepted are owner-accepted ADRs or owner decisions recorded in these docs; the rest is a design proposal. Workflow and approval rules: [AGENTS.md](../../AGENTS.md).

**Status vocabulary**

| Status | Meaning |
|---|---|
| Accepted | Confirmed by the owner, or a process decision recorded as Accepted (an ADR marked "Accepted by owner" is an Accepted decision the owner made or provided). |
| Proposed | Recommendation, awaiting owner decision. |
| Undecided | Options listed; decision deferred. The deciding milestone is named where the approved plan ties it (for example UI library: M4, multi-site: before the multi-site milestone); otherwise: decide when the related milestone is planned. |

Accepted so far: ADR 0001 (record decisions as ADRs), ADRs 0002-0006 (accepted by the owner on 2026-10-28: modular monolith and workspace layout, PostgreSQL only, database access layer direction, HTTP stack, extension model), MIT license (ADR 0007), English docs, the four-stage workflow and the Model Selection Policy (ADR 0008), the Toolchain/MSRV policy (owner decision 2026-10-28, see [maintenance-policy](maintenance-policy.md#toolchain-and-msrv)), and the M1 owner decisions of 2026-10-28 (TOML metadata files, UUID v7 generated in the application, `time`, `rust_decimal`, `clap` (derive) for the CLI, PostgreSQL 16 as the version floor, DocType `name` with a separate `label`; see section 3 and [dependency-evaluation](dependency-evaluation.md)), and the seven M2 owner decisions of 2026-10-28 (section 3, "M2 owner decisions"). **Everything else below is Proposed or Undecided.** The M1 spike confirmed the ADR 0004 direction (no superseding ADR; see [dependency-evaluation](dependency-evaluation.md#m1-spike-result-adr-0004-checkpoint)). Decisions are recorded in [docs/adr](../adr/README.md). Frappe behavior referenced here is labelled in the [feature matrix](feature-matrix.md).

## 1. Shape: modular monolith (Accepted, [ADR 0002](../adr/0002-modular-monolith-and-workspace-layout.md))

The Accepted scope is exactly the ADR 0002 decision: four crates (`forgebase-core`, `forgebase-db`, `forgebase-http`, `forgebase-cli`), the dependency direction cli → http → db → core, jobs and scheduler as a module in `forgebase-db` plus a `worker` command, and a React + TypeScript `desk/` arriving at M4. Other details in this section (Vite, the specific CLI command list, the "Rejected for now" list) remain Proposed.

One Cargo workspace, four crates. Do not split further until a real boundary problem appears.

| Crate | Responsibility |
|---|---|
| `forgebase-core` | Metadata model (DocType, DocField, field types), document value model (validated map), validation, naming, lifecycle pipeline definitions, permission evaluator. **No I/O, no async, no SQL.** Pure and unit-testable; most business rules live here. |
| `forgebase-db` | PostgreSQL access: pool, transactions, metadata-driven schema sync (DDL), framework migrations, document persistence, permission-filtered queries, job queue tables. |
| `forgebase-http` | axum routes, auth/session middleware, REST resource API, JSON error mapping. Thin; delegates to core and db. |
| `forgebase-cli` | The `forgebase` binary (clap): site init, migrate, serve, create user, run worker. |

The frontend `desk/` (React + TypeScript + Vite) arrives only at the Desk milestone (M4).

Dependency direction (core depends on nothing internal):

```mermaid
graph LR
  cli[forgebase-cli] --> http[forgebase-http] --> db[forgebase-db] --> core[forgebase-core]
```

Background jobs and the scheduler are a module in `forgebase-db` plus a `worker` command, not a separate service.

Rejected for now: microservices, dynamic libraries, WASM plugins, a separate jobs service, Redis.

## 2. Extension model (Accepted, [ADR 0006](../adr/0006-extension-model.md))

A Forgebase "app" is a Rust crate compiled into the user's binary. The user's `main` depends on `forgebase-*` and registers app DocTypes and hooks at startup through an explicit registration list (no global magic or inventory macros).

- Pros: type safety, no runtime plugin machinery, one build artifact.
- Cons: changing hooks requires rebuild and redeploy; no install-at-runtime like `bench get-app`.
- This is an intentional difference from Frappe. Server Scripts (runtime user code) are out of scope.

## 3. Metadata (DocType) model (Proposed)

**Owner decisions, 2026-10-28 (Accepted)**, for M1; the remaining items in this section keep their stated status. The last four bullets were confirmed ("setuju semua") after the M1.1 review:

- Metadata file format: **TOML**.
- `id` is a **UUID v7 generated in the application**.
- A DocType `name` is a snake_case identifier with a separate `label`; the label is not derived automatically.
- `required` is enforced by application validation, not by a database `NOT NULL`, in M1.
- M1 field types: Data, Text, Int, Decimal, Check, Date, Datetime, Select. Link arrived in M2.1; Table arrives in M2.2.

- Int maps to PostgreSQL `bigint` (spike lesson: an `int` column decodes as `i32`, while `Value::Int` is `i64`).
- Data fields default to `max_length` 140.
- A required Data, Text, Select or Link field treats an empty or whitespace-only string as missing.
- Decimal values with more decimal places than the field's scale are rejected rather than rounded.

- **Schema-as-code.** DocTypes are defined in files inside the app crate (versioned in git) and loaded at startup; the DB holds a synced copy for introspection and admin. Single direction of truth: files to DB. File format: TOML (Accepted, see above).
- **Field types.** Small closed set first: Data (text with max length), Text, Int, Decimal (Postgres `numeric`; Float only if justified), Check (bool), Date, Datetime (`timestamptz`), Select, Link, Table (child). Layout-only types (section/column breaks) stay out of the data field list; UI layout is a separate optional section. Other types (Attach, JSON, Dynamic Link, Currency, ...) are added one at a time with a reason.
- **Standard columns.** `id` (UUID surrogate PK; v7, generated in the application, Accepted), `name` (unique human-facing identifier), `owner`, `created_at`, `modified_at`, `modified_by`, `version` (int, optimistic concurrency), `docstatus` (smallint, submittable types). A surrogate PK separate from `name` is an intentional difference (Frappe uses `name` as PK) so renames do not cascade. The surrogate key itself is confirmed in M1.
- **Table naming.** Deterministic, validated snake_case identifiers derived from DocType names (for example `doc_sales_order`), always quoted; never interpolated from unvalidated input. PostgreSQL truncates identifiers at 63 bytes, so the limit applies to the full table name including the `doc_` prefix (and to index and constraint names).
- **Naming strategies, initially.** Random/hash; field-based; naming series (Accepted owner decision 2026-10-28, planned for M2.4: gapless, a counter row in `fb_series` incremented in the document's transaction; see "M2 owner decisions" below). Others deferred.
- **Single DocTypes (settings): Undecided** (decide when the related milestone is planned). Simplest option is a regular one-row table rather than a key-value `singles` table.

### M2 owner decisions (2026-10-28, Accepted)

Approved with the M2 plan ("setuju semua"). Implementation is split into four PRs: M2.1 Link fields and optimistic concurrency (implemented), M2.2 child tables, M2.3 lifecycle and hooks, M2.4 docstatus and naming series. Details of each decision are in the section named in brackets.

1. **Link storage** [section 4]: a `uuid` column with a foreign key to the target's `id` (`ON DELETE RESTRICT`) and an automatic index. The API accepts and returns the target `name`. A missing target is a validation error naming the field; deleting a referenced document is refused with a message naming the referencing DocType. The target must be in the registry; self-links are allowed.
2. **Optimistic concurrency** [section 6]: update, delete, submit and cancel require `expected_version`; a mismatch is `DbError::Conflict` with the current version. The CLI needs `--version N` for `doc update` and `doc delete`. This is a breaking API change, accepted in 0.x.
3. **Child tables** [section 4]: `is_child = true`; no own CRUD; no nesting (a child cannot contain a Table field). Columns `id`, `parent_id` (foreign key `ON DELETE CASCADE`, indexed), `idx` and the fields; Link fields inside a child are allowed. Saved with the parent in the same transaction by replacing all rows on each save (delete the old rows, insert the new ones in `idx` order), so row ids change on every save.
4. **Hooks** [section 6]: synchronous and pure, defined in `forgebase-core`; they may modify the document or reject it with an error. No database access in M2; hooks that read the database and after-commit effects wait for the outbox (M5). Hooks: `before_insert`, `validate` (insert, update, submit), `before_submit`, `before_cancel`, `before_delete`. Insert order within one transaction: `before_insert`, the `validate` hook, framework validation (types, required, Links), naming, then write the document and its children. Any hook failure rolls back the whole transaction.
5. **docstatus** [section 6]: `submittable = true` adds `docstatus` and `amended_from` columns. A draft is editable and deletable. A submitted document is locked except fields marked `allow_on_submit`. A cancelled document is deletable or can be amended into a new draft. The amended name is `<original-name>-<n>`; submittable DocTypes may not use field naming.
6. **Naming series**: for example `naming = { series = "INV-.YYYY.-.#####" }`; the counter lives in table `fb_series` and is incremented in the same transaction as the document, so numbers have no gaps (a rollback returns the number) and inserts with the same prefix serialize. Date parts come from the transaction date in UTC.
7. **ADR 0009** records the lifecycle and hooks decisions (written with M2.3).

### DocType file format

One TOML file per DocType; unknown keys are rejected. Loaded with `DocType::from_toml_str`; an example is `crates/forgebase-core/tests/fixtures/customer.toml`.

Top-level keys:

- `name` (required): snake_case identifier, `^[a-z][a-z0-9_]*$`, at most 59 bytes.
- `label` (required): human-readable name, not derived from `name`.
- `naming` (optional): `"random"` (default) or `{ field = "<fieldname>" }`; the named field must be a required, unique Data field.
- `[[fields]]`: the field list, in order.

Field keys (`[[fields]]`):

- `name` (required): snake_case identifier, at most 63 bytes, not a reserved standard column (`id`, `name`, `owner`, `created_at`, `modified_at`, `modified_by`, `version`, `docstatus`, `idx`, `parent_id`) or a PostgreSQL system column (`tableoid`, `xmin`, `xmax`, `cmin`, `cmax`, `ctid`).
- `label` (optional): defaults to the field `name`.
- `type` (required): `data`, `text`, `int`, `decimal`, `check`, `date`, `datetime`, `select` or `link`.
- `required`, `unique` (optional, default `false`). `required` is enforced by application validation; for Text, Data, Select and Link an empty or whitespace-only string counts as missing.
- `max_length` (`data` only): `1..=65535`, default 140.
- `precision` (`1..=28`) and `scale` (`<= precision`), both required for `decimal`. Strict rule: a value with more decimal places than `scale` is rejected, not rounded (trailing zeros are ignored).
- `options` (`select` only, required): non-empty list of unique, non-empty strings.
- `target` (`link` only, required): the name of the DocType the field points to, a valid DocType name. A parameter that does not belong to the type is an error.

Text, Data, Select and Link values and Select options must not contain NUL (`\0`), which PostgreSQL text cannot store.

## 4. Relationships (Link Accepted and implemented in M2.1; child tables Accepted, planned for M2.2)

- **Link field (Accepted owner decision 2026-10-28; implemented in M2.1)** is a real PostgreSQL foreign key to the target's `id` (`ON DELETE RESTRICT`), so the DB enforces integrity (Frappe checks in the application; behavior itself Unverified, see [feature matrix](feature-matrix.md)). The API accepts and returns the target `name`; resolution is internal. Intentional difference.
  - **Definition.** `type = "link"` with `target = "<doctype name>"` (see the file format in section 3). `FieldType::Link { target }`; the value is `Value::Text(name)`, validated like Data without a length limit (text, no NUL, a required Link is non-blank). Existence of the target document is not checked by `forgebase-core` (no I/O): `forgebase-db` reports `ValidationError::LinkNotFound`.
  - **References between DocTypes.** `Registry::add` checks one DocType, so files may name DocTypes that are loaded later. `Registry::check_references` checks that every target is in the registry (a self-link is fine) and returns every `DefinitionError::UnknownLinkTarget`. Callers must run it after loading all DocTypes: `forgebase-db::sync` does (`DbError::InvalidMetadata`, a clear error and not a schema conflict) and so does the CLI loader (the message names the file).
  - **Writing.** For every non-null Link value, `insert` and `update` resolve the name with `SELECT "id" FROM "doc_<target>" WHERE "name" = $1::text FOR KEY SHARE` on the same client. The row lock stops a concurrent delete of the target until the transaction ends; on a pooled client each statement is its own transaction, so use a caller-owned transaction for the lock to cover the write (otherwise a target deleted in between fails the write on the foreign key). An unknown name is a `LinkNotFound` item in `DbError::Validation`, collected with the other problems; nothing is written. The resolved id is bound as `uuid` with an explicit cast.
  - **Reading.** `get` and the `RETURNING` of `insert` and `update` select the target's `name` with a scalar subquery on the id (aliased to the field name), so `StoredDocument` holds `Value::Text(name)`.
  - **Deleting.** A document that other documents link to cannot be deleted: SQLSTATE 23503 becomes `DbError::LinkedDocumentExists { doctype, name, referenced_by }`. `referenced_by` is the DocType derived from the table the server names in the error (`doc_<doctype>`), or `None` if that is not available; no extra query is needed, which matters inside a transaction that the failed statement has aborted.
- **Child tables (Accepted owner decision 2026-10-28, see section 3; planned for M2.2).** Each child DocType belongs to exactly one parent DocType and is marked `is_child = true`; it has no CRUD of its own and cannot contain a Table field (no nesting). The child table has columns `id`, `parent_id` (FK `ON DELETE CASCADE`, indexed), `idx` for ordering and the fields; Link fields inside a child are allowed. There are no polymorphic `parenttype`/`parentfield` columns. Intentional difference (Frappe allows reusing one child DocType across parents). Child rows are saved only as part of the parent document, in the same transaction, by replacing all rows on each save (delete the old rows, insert the new ones in `idx` order), so child row ids change on every save.
- **Dynamic Link:** deferred (no FK possible); not a priority.

## 5. Schema sync and migrations (Proposed; M1.2 and M2.1 implemented in `forgebase-db`)

**Implemented in M1.2 and M2.1** (the rest of this section is still the design direction):

- **Framework migrations**: embedded SQL in `crates/forgebase-db/migrations/`, run by `migrate_framework` in one transaction under a transaction-level advisory lock (`pg_advisory_xact_lock`, released automatically, never a session lock on a pooled connection). Applied migrations are verified first: unknown version (database newer than code) or checksum mismatch is an error. The checksum is SHA-256 computed by PostgreSQL (no hash crate). Tables: `fb_migration`, `fb_doctype` (which DocType tables Forgebase manages).
- **Schema sync** (`sync`): one transaction; transaction-local `lock_timeout` (default 5 s, clamped to 1 ms ..= `i32::MAX` ms; an error `LockTimeout` that is safe to retry; in `migrate` the framework migrations stay applied, only the sync step is not), advisory lock, then introspection of the connection's current schema inside the transaction so concurrent syncs see each other's result. Steps are additive only: create table, add the keys of new tables, add foreign keys, add nullable column, create unique index, create a Link index, register in `fb_doctype`. `dry_run` rolls back and returns the SQL; for framework migrations a dry run still executes the migration DDL before rolling back. Connection errors keep the server message and SQLSTATE (for example 28P01, 3D000). Database state that does not match expectations (such as an `fb_migration` with other column types) gives an error, never a panic.
- **Type mapping**: Data `varchar(n)`, Text and Select `text` (Select options are validated by the application, so option changes need no migration), Int `bigint`, Decimal `numeric(p,s)`, Check `boolean`, Date `date`, Datetime `timestamptz`, Link `uuid` (the target's `id`). Field columns are always nullable; `required` is application-enforced.
- **Conflicts** (whole sync refused, nothing applied): a column whose type, length, precision or scale differs from the metadata (including a Data `max_length` change in either direction), a standard column that is missing, differently typed or nullable (also `id` not being the primary key and `name` not unique), a `timestamptz` column with a precision other than the default 6, a non-table relation or standalone type named `doc_<name>`, and an existing `doc_<name>` table that Forgebase did not create (never adopted); and, for Link fields, the foreign key conflicts described below. **Warnings** (reported, nothing changed): a field column that is `NOT NULL` in the database (fields are nullable; the application enforces `required`), a column no field describes, a unique index whose field is no longer `unique`, and a registered DocType missing from the registry.
- **Names**: Forgebase does not name constraints or indexes. `CREATE TABLE` creates columns only, keys are added afterwards with `ALTER TABLE ... ADD PRIMARY KEY ("id"), ADD UNIQUE ("col")` (unnamed), and unique indexes added later use `CREATE UNIQUE INDEX ON ... ("col")`; PostgreSQL picks free names of at most 63 bytes. A sync works in phases: it first creates every new table with columns only, then adds the primary key and unique constraints of the new tables, then the foreign keys of the new tables, then added columns, then the foreign keys of added Link columns, then late unique and Link indexes, then registers the DocTypes. Every primary key exists before any foreign key is added, so Link fields between new tables (also two DocTypes that link to each other, and self-links) sync in one run. Because all tables of the sync exist before any key or index is named, a generated name cannot clash with a DocType table in the same sync, even when PostgreSQL truncates long names. Unique and index detection reads `pg_index` (valid, single-column, non-partial), not names. If a relation of any kind or a standalone type (view, index, sequence, domain, ...) already has the name `doc_<name>`, the sync reports a conflict; an ordinary table of that name that Forgebase did not create is reported as unmanaged. A later sync can still conflict when a DocType table name equals a name PostgreSQL already gave to an index of an earlier table.
- **Link fields (M2.1)**: a Link column is `uuid`. New tables get `ALTER TABLE t ADD FOREIGN KEY ("col") REFERENCES "doc_<target>" ("id") ON DELETE RESTRICT` (unnamed, one statement per table) in the foreign-key phase. A Link column added to an existing table gets `ADD COLUMN`, then the foreign key, then the index. Every Link column gets a plain `CREATE INDEX ON t ("col")` because PostgreSQL does not index referencing columns itself; a `unique` Link field is covered by its unique constraint and gets no extra index. An index that is missing on an existing Link column is an additive step (it may have been dropped by hand). Foreign keys of managed tables are read from `pg_constraint` (single-column, `contype = 'f'`, with the delete rule, the referenced table and column, and whether it is validated). **Conflicts** (whole sync refused): a Link column that exists without the expected foreign key, with a foreign key to another table or column, with a delete rule other than `RESTRICT` (including the default `NO ACTION`), or with a `NOT VALID` foreign key (existing rows were never checked); and a foreign key on a column whose field is not a Link. Forgebase never repairs a foreign key by itself, because adding one validates all existing rows. A Link whose target DocType is not in the registry is refused before any SQL runs (`DbError::InvalidMetadata`), not reported as a conflict.
- **Limits**: plain `CREATE INDEX` (blocks writes on that table while building; no `CONCURRENTLY` yet); adding a unique index to an existing column fails, and the sync rolls back, if the column already holds duplicates; only the connection's current schema is handled; no TLS to PostgreSQL yet (planned: rustls; run over a local or private network until then).


Two distinct problems:

1. **Framework migrations** (Forgebase's own system tables): small embedded, ordered SQL migrations with checksums and an advisory lock. Tool choice depends on the DB-layer decision ([ADR 0004](../adr/0004-database-access-layer.md)).
2. **DocType schema sync.** Framework code diffs metadata against the live schema (`information_schema`) and produces a plan.
   - **Additive changes** (new table, new nullable column, new index, and the foreign key constraint plus index that a new Link column implies) apply automatically.
   - **Destructive or risky changes** (drop/rename column, type change, NOT NULL on existing data) are never automatic; they require an explicit migration file.
   - `migrate --dry-run` prints the plan. Sync runs in a transaction where PostgreSQL allows it (most DDL is transactional).
   - **Design notes for M1/M2 (not decisions):**
     - A plain `CREATE INDEX` takes a lock that blocks writes on the table while it builds, which matters on large tables.
     - `CREATE INDEX CONCURRENTLY` and `CREATE DATABASE` (needed for database-per-site) cannot run inside a transaction block, so they sit outside the sync transaction and need special handling (for example an explicit, separate step).
     - Adding a Link column also adds a foreign key constraint, which takes locks on the referenced table.
     - PostgreSQL does not automatically index the referencing column of a foreign key; sync should create an index on every foreign key column (Link columns and child `parent_id`), otherwise `ON DELETE RESTRICT` checks and child-row lookups scan the whole table.
     - Most `ALTER TABLE` forms (including ADD COLUMN) take an ACCESS EXCLUSIVE lock (ADD FOREIGN KEY takes SHARE ROW EXCLUSIVE); while such a statement waits behind a long-running transaction it queues and blocks every later read and write on that table. Set `lock_timeout` for sync DDL so it fails fast instead of stalling the application. Hitting `lock_timeout` aborts the sync transaction, so sync must report the failure and allow a retry.
3. **Data patches.** Ordered, run-once, recorded in a table, inside transactions, with "before schema sync" and "after schema sync" phases (concept from Frappe's pre/post model sync).

### Document CRUD (M1.3, implemented in `forgebase-db`)

`insert`, `get`, `update` and `delete` handle one document of one DocType. They take a `tokio_postgres::GenericClient`, so they run on a pooled connection (each call is then its own transaction) or inside a `Transaction` that the caller owns and commits or rolls back.

- **Validation first.** `insert` runs `forgebase_core::document::validate` and `update` runs `validate_update`; all problems are returned together (`DbError::Validation`). Nothing is written when validation fails.
- **Names.** Random naming: `name` is the new `id` as a hyphenated UUID, and a given name is rejected. Field naming: `name` is the value of the naming field; a given name must be absent or equal to it. Changing the naming field in an update is rejected: renames are not supported yet.
- **Standard columns.** `id` is a UUID v7 generated in the application. `created_at` and `modified_at` are the database's `now()` (the transaction start time, so several writes in one transaction share it). `owner` and `modified_by` are the `actor`, which must be a non-empty string without NUL; it is recorded, not authenticated. `version` starts at 1 and every update adds 1. An update keeps `id`, `name`, `owner` and `created_at`.
- **Values.** Always bound parameters, with an explicit cast per field type in the SQL (`$n::varchar`, `::bigint`, `::numeric`, ...) so parameter types come from the metadata, not from inference. Absent fields are stored as NULL; an update changes only the fields it names, and `null` clears one. Identifiers come from validated metadata and are quoted.
- **Errors.** A duplicate in a `unique` field or in `name` is `DbError::UniqueViolation` (SQLSTATE 23505, server message kept); an unknown name in `update` or `delete` is `NotFound` (`get` returns `None`); an update without changes is `NoChanges`.
- **Links (M2.1).** See section 4: names are resolved to ids on write (`FOR KEY SHARE`) and back to names on read; a missing target is a `LinkNotFound` validation item; a delete that would orphan links is `LinkedDocumentExists`.
- **Optimistic concurrency (M2.1, Accepted owner decision 2026-10-28).** `update` and `delete` take `expected_version: i64` (a breaking change to the M1 signatures). The SQL is `... WHERE "name" = $n AND "version" = $m`. When no row matches, one more `SELECT "version" ... WHERE "name" = $1` tells the cases apart: no row is `NotFound`; a row with another version is `DbError::Conflict { doctype, name, expected, current }` and nothing was written. Under READ COMMITTED (the PostgreSQL default, which the conflict detection assumes) a second writer waits for the row lock of the first and then re-checks the version, so of two concurrent updates with the same `expected_version` exactly one succeeds. In a caller transaction at REPEATABLE READ or SERIALIZABLE the losing writer instead gets a serialization failure (SQLSTATE 40001, returned as `DbError::Postgres`, not `Conflict`) and should retry the whole transaction. A conflict is an ordinary result, so it does not abort a caller-owned transaction.
- **Not yet:** permissions (M3), renames, child tables, lifecycle hooks, list and filter queries.

### The `forgebase` command-line tool (M1.3, implemented in `forgebase-cli`)

`forgebase migrate --doctypes DIR [--dry-run] [--lock-timeout-ms N]` and `forgebase doc insert|get|update|delete`. `doc update` and `doc delete` require `--version N`, the `version` of the document as last read; a stale version fails with exit code 1 and a message that includes the current version, and deleting a document that others link to fails with a message naming the linking DocType. The DocType directory is also checked for Link targets that no file defines (reported with the file name). The database URL comes from the `FORGEBASE_DATABASE_URL` environment variable; there is no config file. DocTypes are read from the `*.toml` files directly in `--doctypes DIR` (sorted by file name; not recursive; the extension match is case-sensitive, so `*.TOML` is ignored; names starting with `.` are skipped; a `*.toml` entry that cannot be read, such as a dangling symlink, is reported as an error; every file error is reported with its file name). Output is the stored document as pretty JSON on standard output; errors are `error: ...` on standard error with exit code 1, usage errors exit with 2.

JSON mapping (in the CLI for now, to move to a shared place with the HTTP API in M3): Data, Text and Select are strings; a Link is a string, the `name` of the target document (the database stores its `id`); Int is a JSON integer (floats and values outside `i64` are rejected); Decimal is a JSON **string** (a JSON number is rejected so no precision is lost); Check is a boolean; Date is `YYYY-MM-DD`; Datetime is RFC 3339 and is printed in UTC with `Z`; `null` is no value. Datetime has **microsecond precision** (PostgreSQL `timestamptz`): finer digits are truncated, not rounded or rejected. Duplicate keys in the input are accepted and the last one wins (serde_json behavior). Decimal strings accept every form `rust_decimal` parses (for example `"1e3"`, `"+12.5"`, `".5"`) with exact values. Output keys are sorted alphabetically (the standard columns appear among the fields). Keys that are not fields are passed on so that validation reports them. Limit: values another tool wrote that are outside the supported ranges (`date` and `timestamptz` years beyond ±9999 as `time` supports them, `infinity`, `NaN`) cannot be read; `get` fails with an error that names the DocType and the field.

## 6. Document lifecycle and transactions (Proposed)

- **One fixed, documented, tested pipeline per operation.** Insert, in the order Accepted by M2 owner decision 4 (see section 3 and the hooks bullet below): `before_insert` hook, `validate` hook, framework validation (field types, required, Links; permissions arrive in M3), naming, write, then the proposed `after_insert` hook, after-commit effects and commit. Save, Submit, Cancel and Delete are analogous. The order is covered by tests when M2.3 implements it.
- **Hooks** (Accepted, [ADR 0006](../adr/0006-extension-model.md)) are a small Rust trait per DocType (`DocTypeHooks`, default no-op methods) registered by the app; with several apps, hooks run in registration order. No class override mechanism. The M2 owner decisions of 2026-10-28 (section 3, decisions 4 and 5; implemented in M2.3 and M2.4) narrow the first version: hooks are synchronous and pure, defined in `forgebase-core`, may modify the document or reject it, and have no database access in M2; the hooks are `before_insert`, `validate`, `before_submit`, `before_cancel` and `before_delete`. The insert order is `before_insert`, the `validate` hook, framework validation (types, required, Links), naming, then writing the document and its children, all in one transaction that any hook failure rolls back. The `after_*` hooks, hooks with database reads and the pipeline details below remain Proposed.
- **Transactions.** Exactly one DB transaction per API request or command; the framework owns commit and rollback. Hooks **cannot commit**. Design requirement: hooks receive a restricted, framework-provided handle (a fixed set of operations) rather than a raw database connection or transaction, so app code cannot issue `COMMIT` or unvalidated SQL; the exact mechanism, and whether hook reads run as the user or the system, are decided in M2. Side effects that must happen only after commit (jobs, realtime, emails) go to an outbox/job table written in the same transaction (transactional outbox).
- **docstatus (Accepted owner decision 2026-10-28; planned for M2.4).** Draft (0) / Submitted (1) / Cancelled (2), and amendment (a new draft with `amended_from`, named `<original-name>-<n>`) for DocTypes with `submittable = true`; a submitted document is locked except fields marked `allow_on_submit`; submittable DocTypes may not use field naming.
- **Optimistic concurrency (Accepted owner decision 2026-10-28; implemented in M2.1 for update and delete).** Update, delete, submit and cancel require `expected_version`; a mismatch is `DbError::Conflict` with the current version, which the HTTP API (M3) maps to 409 Conflict. Submit and cancel arrive with M2.4. Details: section 5, "Document CRUD".

## 7. API (Proposed)

- REST: `/api/v1/resource/{doctype}` (list with filters/fields/paging, get, create, update with version, delete) and `/api/v1/method/{name}` for explicitly registered app endpoints.
- JSON only; consistent error envelope; stable versioned prefix from day one; default page limit with a hard maximum.
- No GraphQL, no RPC-by-dotted-path.
- HTTP stack: axum on tokio ([ADR 0005](../adr/0005-http-stack.md)).

Security rules for the API are in [security-model](security-model.md).

## 8. Background jobs, scheduler, realtime

- **Jobs (Proposed).** Postgres table queue using `SELECT ... FOR UPDATE SKIP LOCKED`, run by `forgebase worker` (same binary; Accepted, [ADR 0002](../adr/0002-modular-monolith-and-workspace-layout.md): jobs are a module in `forgebase-db` plus a `worker` command; the queue design in this bullet remains Proposed). At-least-once delivery, so jobs must be idempotent; retries with backoff; a lease/visibility timeout so jobs claimed by a crashed worker become runnable again; a dead state.
- **Scheduler (Proposed).** Cron-like, table driven, leader elected with a Postgres advisory lock. Milestone M5. Design note: a session-level advisory lock belongs to one connection, so it must be held on a dedicated (non-pooled) connection, or a transaction-level lock taken per tick must be used. With the per-tick lock, `next_run_at` must be advanced in the same transaction so a second instance cannot re-run the same tick right after the first commits. The mechanism is decided in M5.
- **Realtime (Undecided, deferred).** Later option: Postgres `LISTEN/NOTIFY` to SSE/WebSocket in the same binary. No Node/socket.io, no Redis.

## 9. Multi-site (Undecided)

Lean: **database-per-site** (strong isolation, simple per-tenant backup and restore, matches the Frappe concept). Alternatives: schema-per-site; row-level tenant column (rejected for weakest isolation: permission bugs leak data). Decide before the multi-site milestone. Note: database-per-site needs `CREATE DATABASE`, which cannot run in a transaction (see section 5).

Design constraint from day one (Proposed): every request or command carries an explicit `Site`/DB-pool context; there is no global DB handle, so multi-site is additive later.

## 10. Customization (Proposed, not a priority)

Custom fields and property setters: when added, a small explicit per-site overlay table applied over app metadata, schema-sync aware. No "Customize Form" equivalent before a real need.

## 11. Desk, reports, workflow

- **Desk (Proposed, M4).** Metadata-driven generic list and form views in React; minimal. UI component library: **Undecided**, decided in M4 together with a dependency evaluation.
- **Reports (Proposed, Later).** Start with saved list views and filters; SQL-based query reports later with a read-only DB role. Script reports: no.
- **Workflow (Proposed, not a priority).** States, transitions and roles mapped onto docstatus; after M5.

## Key challenges and how we approach them

| Challenge | Approach |
|---|---|
| Tables and columns come from runtime metadata, so static ORMs do not fit | Small in-house SQL layer for DocType DDL/CRUD with strict identifier validation, quoting and bound parameters ([dependency-evaluation](dependency-evaluation.md)). Accepted direction ([ADR 0004](../adr/0004-database-access-layer.md)), confirmed by the M1 spike. |
| Schema drift and risky migrations | Additive-only auto-sync, explicit files for destructive changes, `--dry-run`, transactional DDL. |
| Hook order and partial commits. Frappe's docs may list hook order inconsistently (Unverified; search summaries suggest the hook table and the Document API page differ, see [feature matrix](feature-matrix.md)); partial commits from user-code `commit()` are our own analysis (opinion, not documented fact) | One fixed pipeline, one transaction per request, hooks cannot commit, outbox for side effects. |
| Permission complexity | One evaluator in `forgebase-core`; all reads go through a permission-filtered query path; exhaustive negative tests ([security-model](security-model.md)). |
| Integrity of references | Real foreign keys instead of application-level checks. |
| Multi-site retrofit cost | Explicit site/pool context from day one; decision deferred. |
| Scope creep | Closed field-type set, milestone gates, explicit non-goals, [maintenance-policy](maintenance-policy.md). |

## Cost of in-house components

The approach in this document means owning these pieces ourselves: SQL builder, schema sync, migration runner, runtime validator, job queue, scheduler, session store and permission evaluator. This is a deliberate trade against dependency weight and lock-in, not free. Each must stay small, be thoroughly tested (integration tests against real PostgreSQL where it touches the database), and be reviewed against the maintenance cost it adds. If one grows beyond that, revisit the dependency alternatives in [dependency-evaluation](dependency-evaluation.md).

## What we deliberately do not build

- Microservices, dynamic libraries, WASM plugins, runtime plugin install.
- Server Scripts, Script Reports, or any admin-authored code stored in the DB.
- Redis, a Node realtime server, a separate jobs service.
- GraphQL, RPC-by-dotted-path.
- Class-override mechanisms for hooks.
- Row-level tenant column for multi-site.
- A "Customize Form" equivalent (until a real need exists).
- Databases other than PostgreSQL.
