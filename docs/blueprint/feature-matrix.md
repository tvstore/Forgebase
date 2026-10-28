# Feature matrix

Compares Frappe Framework capabilities with Forgebase's plan. Research date: 2026-10-28. Design details are in [architecture](architecture.md); milestones in [milestones](milestones.md).

## How to read this

**Evidence labels** (for the "Frappe behavior" column):

| Label | Meaning |
|---|---|
| `Source-indexed` | Backed by a web-search result from an official Frappe domain; the URL is given. Search summaries are second-hand paraphrases, so wording and version scope may differ from the live page. |
| `Unverified` | General knowledge only. Must be confirmed before anyone relies on it. |

There is **no `Verified-fetched`** entry yet: `docs.frappe.io` could not be fetched directly from the research environment (the fetch was blocked by the sandbox with ENOTFOUND; only that domain was tried), so no page was read in full. To promote a claim, open the URL and confirm. Open verification questions are listed at the end.

**Forgebase status values:**

| Status | Meaning |
|---|---|
| Planned (Mx) | Intended for milestone Mx. Still **Proposed** until the owner accepts the plan for that milestone. |
| Intentional difference | Forgebase deliberately does it differently from Frappe. |
| Undecided | Options exist; decision deferred. A milestone is noted where the approved plan ties it; otherwise: decide when the related milestone is planned. |
| Not a priority | May be considered later; no plan. |
| Out of scope | Will not be built. |

Frappe URLs below are shortened: `fw/` = `https://docs.frappe.io/framework/user/en/`, `fwo/` = `https://frappeframework.com/docs/user/en/`, `erp/` = `https://docs.frappe.io/erpnext/`. Paths follow these prefixes as listed in the research source; where a URL does not follow a prefix, the full URL is written out.

## Core capabilities

| Capability | Frappe behavior (short) | Evidence + source | Forgebase status | Notes |
|---|---|---|---|---|
| CLI and sites | `bench` CLI and directory layout hold apps and sites; `bench --site <s> set-config`, `show-config`, `--site all`. | Source-indexed: `fw/bench/reference/show-config`, `fwo/bench/reference/set-config`, `https://docs.frappe.io/framework-copy/bench/overview` | Planned (M1) | Single `forgebase` binary; commands listed in [architecture, section 1](architecture.md). |
| DocType metadata | A DocType describes a model (fields, permissions, naming, behavior); Desk builds list/form/report views from it. One table per DocType (`tab` prefix). | Source-indexed: `fw/basics/doctypes`, `fw/desk` | Planned (M1) | Schema-as-code, files to DB; file format TOML (Accepted 2026-10-28). See [architecture, section 3](architecture.md#3-metadata-doctype-model-proposed). |
| Metadata storage | DocType JSON lives in the app source; `bench migrate` syncs it to DB records and tables (stored twice). | Source-indexed: `fw/basics/apps`, `fwo/guides/deployment/migrations` | Planned (M1) | Same two-copy idea, but one direction only: files to DB. |
| Dynamic fields / field types | More than 30 built-in types (Data, Link, Dynamic Link, Select, Table, Currency, ...). Layout types (section/column/tab break) share the field list (Unverified). | Source-indexed: `fw/basics/doctypes/fieldtypes`, `erp/field-types`; layout types Unverified | Planned (M1) | Small closed set first; layout types kept out of the data field list. Type list: [architecture, section 3](architecture.md#3-metadata-doctype-model-proposed). |
| Standard columns | `name` (PK), `creation`, `modified`, `modified_by`, `owner`. | Source-indexed: `fw/tutorial/create-a-doctype` | Intentional difference | Surrogate `id` PK separate from human-facing `name`. Column list and M1 confirmation: [architecture, section 3](architecture.md#3-metadata-doctype-model-proposed). |
| Naming | Hash (default), autoincrement, naming series, expression, field, prompt, by script. | Source-indexed: `fw/basics/doctypes/naming` | Planned (M2) | Initially random/hash, naming series (gap-tolerant), field-based. Others deferred. |
| Single DocTypes | One record, no list view; values stored as rows in `tabSingles(doctype, field, value)`. | Source-indexed: `fw/basics/doctypes/single-doctype` | Undecided | Simplest option is a regular one-row table, not a key-value table. Decide when the related milestone is planned. |
| Virtual DocTypes | No DB table; controller supplies data; still uses resource API and permissions. | Source-indexed: `fwo/basics/doctypes/virtual-doctype`, `fw/basics/virtual_docfield` | Not a priority | Assumption: no requirement yet. Not in the owner's intended feature scope. |
| Document lifecycle | Controller hooks `before_insert`, `validate`, `before_save`, `on_update`, `on_submit`, `on_cancel`, `on_trash`, etc. Exact `after_insert` vs `on_update` order unclear in docs. | Source-indexed: `fw/basics/doctypes/controllers`; order itself Unverified | Planned (M2) | One fixed, documented, tested pipeline per operation; exact order specified and tested in M2. |
| Transactions | Auto-commit at end of successful POST/PUT; rollback on uncaught exception; `frappe.db.commit()` callable from user code; commit/rollback hooks. | Source-indexed: `fw/api/database` | Intentional difference | One DB transaction per request/command, owned by the framework; hooks cannot commit; after-commit effects via transactional outbox. |
| docstatus and amend | 0 Draft / 1 Submitted / 2 Cancelled; submitted docs locked except "Allow on Submit" fields; amend creates a new draft linked via `amended_from`. `-1` name suffix is Unverified. | Source-indexed: `fwo/basics/doctypes/docstatus`, `fwo/tutorial/types-of-doctype` | Planned (M2) | Same concept as an explicit state machine for submittable DocTypes. |
| Optimistic concurrency | Stale save raises "document has been modified" (compares `modified`). | Unverified | Planned (M2) | Accepted 2026-10-28; implemented for update and delete in M2.1 (submit and cancel follow in M2.4). Both take `expected_version`; a mismatch is `DbError::Conflict` with the current version (409 once the HTTP API exists). CLI: `--version N`. |
| Link fields | Stores target `name`; existence checked on save and delete blocked while referenced; no DB-level FK. | Option semantics Source-indexed: `fw/basics/doctypes/docfield`, `erp/creating-custom-link-field`; validation and no-FK behavior Unverified | Intentional difference | Accepted 2026-10-28; implemented in M2.1. Real PostgreSQL foreign key to target `id`, `ON DELETE RESTRICT`, with an automatic index. API still uses `name`; a missing target is a validation error and deleting a referenced document is refused. See [architecture, section 4 (Relationships)](architecture.md). |
| Dynamic Link | Target DocType read from a sibling field. | Source-indexed: `erp/dynamic-link-fields` | Not a priority | No FK possible; deferred. |
| Child tables | Rows in child DocType's table with `parent`, `parenttype`, `parentfield`, `idx`; one child DocType reusable by many parents. | Source-indexed: `fw/basics/doctypes/child-doctype`; atomic save with parent Unverified | Intentional difference | Accepted 2026-10-28; implemented in M2.2. Each child DocType belongs to exactly one parent; `parent_id` FK `ON DELETE CASCADE` (indexed) + `idx`; saved only with the parent in one transaction by replacing all rows (row ids change on every save); no nesting. See [architecture, section 4 (Relationships)](architecture.md). |
| Hooks | `hooks.py`: `doc_events`, `scheduler_events`, `override_doctype_class`, `extend_doctype_class` (v16), `fixtures`, etc. | Source-indexed: `fw/python-api/hooks` | Intentional difference | Rust trait `DocTypeHooks` with default no-op methods, registered explicitly at startup in registration order. No class override. |
| App/extension model | Python apps installed per bench/site (`bench get-app`). | Source-indexed: `fw/basics/apps`; per-site install Unverified | Intentional difference | Apps are Rust crates compiled into the user's binary; no runtime install ([ADR 0006](../adr/0006-extension-model.md)). |
| Custom fields / property setters | Site-level Custom Field and Property Setter DocTypes, created via Customize Form; do not change the base DocType. | Source-indexed: `fw/basics/doctypes/customize`, `erp/user/manual/en/customize-form` | Not a priority | When added: small explicit per-site overlay table, schema-sync aware. No Customize Form before a real need. |
| Fixtures | DB records exported as JSON, declared via `fixtures` hook, synced on install/migrate. | Source-indexed: `fw/python-api/hooks`, `fw/tutorial/create-a-doctype` | Undecided | Later; with import/export. Decide when the related milestone is planned. |
| Migrations and patches | `bench migrate`: before_migrate, patches, schema sync, fixtures, ..., after_migrate. Patches in `patches.txt` run once; `[pre_model_sync]`/`[post_model_sync]` since v14. Sync is additive; removed fields are not dropped. | Source-indexed: `fw/database-migrations`, `fwo/bench/reference/migrate` | Planned (M1) | Additive changes auto-apply; destructive changes need an explicit migration file; `migrate --dry-run`; pre/post-sync data patches. |
| Authentication | Session cookie via `/api/method/login`; API key:secret token header; OAuth 2 provider. | Source-indexed: `fw/guides/integration/rest_api/token_based_authentication`, `.../oauth-2` | Planned (M3) | Opaque hashed session tokens + API tokens + Argon2id. OAuth/SSO and 2FA deferred (Not a priority). |
| Authorization (roles) | Role permission rows per DocType: Read, Write, Create, Delete, Submit, Cancel, Amend, plus Report, Export, Import, Share, Print, Email; "If Owner". Union across roles (Unverified). | Source-indexed: `fw/basics/users-and-permissions` | Planned (M3) | Phase 1: role permissions + owner-only condition; deny by default. |
| Permission levels, User Permissions, sharing | permlevel field groups; record filters by linked value; per-document sharing (DocShare, Unverified). Matching rules unverified. | Source-indexed: `fw/basics/users-and-permissions`, `https://docs.frappe.io/framework/get_query`; rules Unverified | Undecided | Phase 2, separate decision (Later); decide when the related milestone is planned. |
| REST API | `/api/resource/{doctype}` CRUD with filters, paging (default 20); `/api/v1/` from v15. | Source-indexed: `fw/api/rest` | Planned (M3) | `/api/v1/resource/{doctype}`; versioned prefix from day one; JSON only; default page limit with hard maximum. |
| Whitelisted methods | `/api/method/{dotted.path}` for `@frappe.whitelist()` functions; no automatic DocType permission check. | Source-indexed: `https://docs.frappe.io/framework-git/security/security-best-practices` | Intentional difference | `/api/v1/method/{name}` for explicitly registered endpoints that must declare required permission. No RPC-by-dotted-path. |
| Background jobs | Python RQ + Redis; `frappe.enqueue(...)`; queues short/default/long; `enqueue_after_commit`. | Source-indexed: `fw/api/background_jobs` | Planned (M5) | Postgres `SKIP LOCKED` queue; see [architecture, section 8](architecture.md#8-background-jobs-scheduler-realtime). No Redis. |
| Scheduler | `scheduler_events` (hourly, daily, cron, ...) synced to Scheduled Job Type; long-running process enqueues jobs. | Source-indexed: `fw/api/background_jobs`, `fw/python-api/hooks` | Planned (M5) | Cron-like table, leader via Postgres advisory lock. |
| Realtime | Node.js socket.io server fed via Redis pub/sub; `frappe.publish_realtime`. | Source-indexed: `fw/api/realtime` | Undecided | Deferred; decide when the related milestone is planned. Option: LISTEN/NOTIFY to SSE/WebSocket in the same binary. No Node, no Redis. |
| Desk | Metadata-driven admin SPA: list, form, report views; timeline. | Source-indexed: `fw/desk`, `fw/api/list` | Planned (M4) | Minimal generic list + form in React + TS. UI library Undecided (decide in M4). |
| Reports | Report Builder (no-code), Query Report (SQL), Script Report (Python). | Source-indexed: `fw/desk/reports/report-builder`, `.../query-report`, `.../script-report` | Not a priority | Proposed: saved list views/filters first; SQL query reports later with a read-only DB role; script reports: Out of scope ([architecture, section 11](architecture.md#11-desk-reports-workflow)). |
| Workflows | Per-DocType states and transitions with roles and conditions, mapped onto docstatus. | Source-indexed: `erp/user/manual/en/workflows`, `erp/workflow` | Not a priority | After M5. |
| Version history | "Track Changes" writes `Version` records; Audit Trail across amendments. | Source-indexed: `erp/user/manual/en/document-versioning`, `fw/audit-trail` | Not a priority | `owner/modified_by/modified_at` from day one; full history deferred. |
| Multi-site | One bench serves many sites; each site has its own database and `site_config.json`; `dns_multitenant` picks site by hostname. | Source-indexed: `fw/basics/site_config`, `https://docs.frappe.io/framework-git/administration/site-config`, `fw/bench/frappe-commands` | Undecided | Lean: database-per-site; decide before the multi-site milestone. See [architecture, section 9](architecture.md#9-multi-site-undecided). |
| Server Scripts | Admin-editable Python/DocType Event scripts stored as DocTypes. | Source-indexed: `fwo/desk/scripting/server-script` | Out of scope | Runtime user code is a security and versioning burden. |
| Database support | MariaDB primary; PostgreSQL status in the framework not confirmed by official docs. | Mostly Unverified (forum-sourced); `https://docs.frappe.io/books/installation` covers Frappe Books only | Intentional difference | PostgreSQL only ([ADR 0003](../adr/0003-postgresql-only.md)). |

## Open verification questions

Taken from the research; each needs a direct read of the official page.

1. Exact controller hook order (`after_insert` vs `on_update`; `before_submit`, `before_cancel`, `on_trash`, `after_delete`).
2. Link validation on save, the delete-time link check, and whether any DB-level FK exists.
3. User Permission matching rules and interaction with If Owner and sharing.
4. DocShare model and interaction with role permissions.
5. Whether `/api/v2` exists and its shape.
6. Official PostgreSQL support status.
7. Amendment naming (`-1` suffix) and the `amended_from` chain.
8. Optimistic concurrency on save.
9. Atomic save of child rows and `idx` maintenance.
10. Full field-type list and DB column types.
11. Fixture sync conflict rules with site edits.
12. Per-site app installation and migrations across sites.
