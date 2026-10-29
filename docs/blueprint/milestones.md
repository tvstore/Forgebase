# Milestones

Milestones are **Proposed** until their Stage 1 plan is approved; each milestone's status line says otherwise. There are no dates. **Each milestone needs its own Stage 1 plan and explicit owner approval before implementation starts** (see [AGENTS.md](../../AGENTS.md#3-mandatory-four-stage-workflow)); the scope below is the starting proposal for that plan, not an approval. Design context: [architecture](architecture.md); capability status: [feature matrix](feature-matrix.md).

| Milestone | Theme |
|---|---|
| M0 | Foundations |
| M1 | Metadata and storage |
| M2 | Relationships and lifecycle |
| M3 | Auth, permissions, REST API |
| M4 | Minimal Desk |
| M5 | Jobs and scheduler |
| Later | Items that each need their own plan |

## M0 Foundations

- **Status:** Done (merged 2026-10-28).
- **Scope:** workspace skeleton with 4 empty crates; CI (fmt, `clippy -D warnings`, test, build, cargo-deny); toolchain pin; contribution docs.
- **Exit criteria:** green CI on the empty skeleton.
- **Non-goals:** any feature code; database access; frontend.

## M1 Metadata and storage

- **Status:** Delivered (all three PRs); Done when M1.3 is merged (Stage 1 plan approved by the owner 2026-10-28). Delivered as three PRs: M1.1 metadata model and DB-layer spike, M1.2 `forgebase-db` and schema sync, M1.3 documents CRUD and CLI.
- **Scope:** DocType definition loading and validation; additive schema sync with dry-run; insert/get/update/delete of simple documents via library API and CLI; framework migrations. Included the DB-layer spike (tokio-postgres vs sqlx), the required checkpoint for [ADR 0004](../adr/0004-database-access-layer.md); it confirmed the accepted direction (2026-10-28). The metadata file format is decided (TOML, Accepted 2026-10-28). Also confirms the surrogate-UUID primary key.
- **Exit criteria:** integration tests against a real PostgreSQL (CI service container). Met: the `forgebase-db` and `forgebase-cli` integration tests pass against PostgreSQL 16 (CI runs them with the same service container); a user can run `forgebase migrate` and insert, get, update and delete a document of an example DocType (README quick start).
- **Non-goals:** links, child tables, lifecycle hooks, HTTP API, authentication, permissions, jobs, multi-site.

## M2 Relationships and lifecycle

- **Status:** Done (Stage 1 plan approved by the owner 2026-10-28; owner decisions in [architecture, section 3](architecture.md#m2-owner-decisions-2026-10-28-accepted)). Delivered as four PRs: M2.1 Link fields and optimistic concurrency (merged), M2.2 child tables (merged), M2.3 lifecycle pipeline and hooks (with [ADR 0009](../adr/0009-lifecycle-pipeline-and-hooks.md); merged), M2.4 docstatus (submit, cancel, amend) and naming series (the last PR; CI and independent review are recorded per PR at merge).
- **Scope:** Link foreign keys; child tables; the fixed lifecycle pipeline with hooks; docstatus and amend; optimistic concurrency; naming series. Specifies the exact pipeline order.
- **Exit criteria:** lifecycle-order tests and rollback tests. Met, with these tests (all against a real PostgreSQL 16):
  - lifecycle-order tests for insert, update and delete (`forgebase-db/tests/lifecycle.rs`) and for submit, cancel, amend and delete of a submittable DocType (`tests/docstatus.rs`);
  - rollback tests: a hook failure at each point of every operation leaves nothing changed (`lifecycle.rs`, and `docstatus.rs` for `validate` and `before_submit` on submit, `before_cancel` on cancel, `before_insert` and `validate` on amend); the series counter is not consumed by a failed insert, by a refused document or by a rolled-back caller transaction (`tests/series.rs`);
  - foreign key integrity (Links `ON DELETE RESTRICT`, child rows `ON DELETE CASCADE`; `tests/links.rs`, `tests/child_tables.rs`) and the schema sync of the docstatus columns (`tests/docstatus_schema.rs`);
  - concurrency tests: optimistic concurrency (`tests/concurrency.rs`), racing submits and racing amendments, and 20 parallel series inserts that get exactly the numbers 1 to 20 (`tests/docstatus.rs`, `tests/series.rs`);
  - docs updated (architecture sections 3, 5 and 6, feature matrix, security model, ADR 0009 implementation details, README, CHANGELOG).

  CI green and the independent review (AGENTS.md, Stage 3) are recorded per PR at merge.
- **Non-goals:** Dynamic Link, single DocTypes (still Undecided), workflow, permissions (M3), automatic cancellation of linked documents.

## M3 Auth, permissions, REST API

- **Status:** In progress (the owner approved the M3 plan on 2026-10-29). Delivered as three PRs: **M3.1** users, roles, sessions, API tokens and login lockout (`forgebase_db::auth`, migration 0003, `forgebase user` and `forgebase token`; [ADR 0010](../adr/0010-authentication-and-sessions.md); this PR), **M3.2** role permissions and permission-filtered queries, **M3.3** the HTTP layer (`/api/v1/resource`, cookies, CSRF, login endpoints).
- **Scope:** users and roles; sessions; API tokens; role permissions; permission-filtered queries; `/api/v1/resource`. See [security-model](security-model.md).
- **Exit criteria:** authorization test matrix including negative tests.
- **Non-goals:** OAuth/SSO, 2FA, field-level permissions, user permissions, sharing, GraphQL.

## M4 Minimal Desk

- **Scope:** login, list view and form view for any DocType, generated from metadata. Decides the UI component library (with a dependency evaluation) and frontend test tooling.
- **Exit criteria:** a user can log in and list/create/edit a DocType document through the UI. (Detailed criteria to be set in the M4 plan.)
- **Non-goals:** reports, workflow UI, realtime, custom pages, theming.

## M5 Jobs and scheduler

- **Scope:** `SKIP LOCKED` queue, `forgebase worker`, scheduler with advisory-lock leader election, transactional outbox.
- **Exit criteria:** (to be set in the M5 plan) at minimum tests for at-least-once delivery, retry/backoff, crashed-worker lease recovery and a single active scheduler leader.
- **Non-goals:** Redis, a separate jobs service, realtime.

## Later

Each item needs its own plan and approval: multi-site, custom-field overlay, field-level permissions / user permissions / sharing, reports, workflow, realtime, fixtures and import/export, file attachments.
