# Milestones

Milestones are **Proposed** until their Stage 1 plan is approved; each milestone's status line says otherwise. There are no dates. **Each milestone needs its own Stage 1 plan and explicit owner approval before implementation starts** (see [AGENTS.md](../../AGENTS.md#3-mandatory-four-stage-workflow)); the scope below is the starting proposal for that plan, not an approval. Design context: [architecture](architecture.md); capability status: [feature matrix](feature-matrix.md).

| Milestone | Theme |
|---|---|
| M0 | Foundations |
| M1 | Metadata and storage |
| M2 | Relationships and lifecycle |
| M3 | Auth, permissions, REST API |
| M4 | Minimal Desk |
| M5 | App model and developer experience |
| M6 | Jobs and scheduler |
| M7 | Site management |
| M8 | Workflow and reports |
| M9 | Integration |
| M10 | Customization |
| Later | Items that each need their own plan |

M4 to M10 are the roadmap proposed after M3 (owner-approved direction 2026-10-29, toward the [Project Goal](vision.md#project-goal)); the order is a proposal and every milestone is still **Proposed**. The jobs, scheduler and outbox milestone was renumbered from M5 to M6 on 2026-10-29; earlier documents (the ADRs) that say "M5" for jobs, the scheduler or the outbox mean M6. Realtime is on demand and not scheduled. Which capability each milestone serves: [feature matrix](feature-matrix.md#roadmap-by-capability).

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

- **Status:** Done (the owner approved the M3 plan on 2026-10-29). Delivered as three PRs: **M3.1** users, roles, sessions, API tokens and login lockout (`forgebase_db::auth`, migration 0003, `forgebase user` and `forgebase token`; [ADR 0010](../adr/0010-authentication-and-sessions.md); PR #11, merged), **M3.2** role permissions, enforced in every document operation, and the list query (`[[permissions]]`, `forgebase_core::permission`, `forgebase_db::list`, `forgebase doc list`; [ADR 0011](../adr/0011-permission-model-phase-1.md); PR #12, merged), **M3.3** the HTTP layer (`forgebase-http`: `/api/v1/resource`, login and logout, cookies, CSRF, `Host` validation, no CORS, the error envelope, `forgebase serve`; this PR, merged once its CI is green and its independent review reports PASS).
- **Scope:** users and roles; sessions; API tokens; role permissions; permission-filtered queries; `/api/v1/resource`. See [security-model](security-model.md).
- **Exit criteria:** authorization test matrix including negative tests. Met, with these tests (all against a real PostgreSQL 16; the full list per required negative test is in the [security model](security-model.md#required-negative-tests)):
  - authorization matrix: every right with and without its role, `if_owner`, deny by default, union of roles, the system context (`forgebase-db/tests/permissions.rs`), and over HTTP with the right status codes (`forgebase-http/tests/resources.rs`: `the_authorization_matrix_over_http`, `the_lifecycle_and_its_error_codes`, `if_owner_hides_the_documents_of_others` with 404 for get, patch and delete);
  - list visibility: a list, filter, sort or page never returns or reveals a row the caller cannot read (`forgebase-db/tests/list.rs`; over HTTP `if_owner_hides_the_documents_of_others`);
  - injection: hostile DocType names, field names, sort keys and values are rejected or inert (`list.rs`; over HTTP `injection_attempts_are_inert`);
  - sessions, tokens, CSRF and lockout: expired, idle, revoked, tampered and unknown credentials are the same 401, wrong, missing and foreign CSRF tokens are 403, token requests need no CSRF, the `Authorization` header wins over the cookie, lockout works over HTTP, the login concurrency bound answers 503 (`forgebase-db/tests/auth.rs`; `forgebase-http/tests/auth.rs`);
  - `Host` validation, no CORS, the error envelope for every failure that reaches the router, including extractor rejections (hyper-level errors such as `431` or malformed HTTP bypass it), body limit, generic internal errors and no secrets in responses (`forgebase-http/tests/auth.rs`, `tests/protocol.rs`);
  - decision 9 (no leaks through Links and delete errors) in the library (`permissions.rs`, including the kept-Link scoping per field and table and the hook-set Link) and over HTTP (`decision_9_no_leaks_through_links`);
  - `forgebase serve` end to end on an ephemeral port: login, cookie, list, Host refusal, dev mode and configuration errors (`forgebase-cli/tests/cli.rs`);
  - docs updated (architecture sections 1, 5 and 7, security model, dependency evaluation, feature matrix, ADR 0005 and 0011, README, CHANGELOG).

  CI green and the independent review (AGENTS.md, Stage 3) are recorded per PR at merge (PR #11, PR #12 and the M3.3 PR).
- **Non-goals:** OAuth/SSO, 2FA, field-level permissions, user permissions, sharing, GraphQL, CORS, `/api/v1/method`, rate limiting beyond the login bound and lockout, TLS in the application, OpenAPI documents, HTTP/2.

## M4 Minimal Desk

- **Scope:** login, list view and form view for any DocType, generated from metadata. Decides the UI component library (with a dependency evaluation) and frontend test tooling.
- **Exit criteria:** a user can log in and list/create/edit a DocType document through the UI. (Detailed criteria to be set in the M4 plan.)
- **Non-goals:** reports, workflow UI, realtime, custom pages, theming.

## M5 App model and developer experience

- **Scope:** a `forgebase new-app` template; one small example business app as a separate crate (DocTypes, hooks, permissions) that validates the framework, not a domain product; app test helpers; a "first app" guide.
- **Exit criteria:** (to be set in the M5 plan) a developer follows the guide and gets a running app with tests, without touching framework crates. Proves success criteria 1 and 5 of the [Project Goal](vision.md#project-goal); capabilities 9 and 10.
- **Non-goals:** runtime plugins ([ADR 0006](../adr/0006-extension-model.md)), domain modules beyond the small example.

## M6 Jobs and scheduler

- **Scope:** `SKIP LOCKED` queue, `forgebase worker`, scheduler with advisory-lock leader election, transactional outbox. Capability 8.
- **Exit criteria:** (to be set in the M6 plan) at minimum tests for at-least-once delivery, retry/backoff, crashed-worker lease recovery and a single active scheduler leader.
- **Non-goals:** Redis, a separate jobs service, realtime.

## M7 Site management

- **Scope:** site configuration file; the multi-site database decision (lean: database per site, see [architecture, section 9](architecture.md)); per-site app install; backups. Capability 1.
- **Exit criteria:** (to be set in the M7 plan) tests for site isolation.
- **Non-goals:** runtime app install ([ADR 0006](../adr/0006-extension-model.md)).

## M8 Workflow and reports

- **Scope:** per-DocType workflow mapped onto docstatus; saved list views and read-only reports ([architecture, section 11](architecture.md#11-desk-reports-workflow)). Capability 8.
- **Exit criteria:** (to be set in the M8 plan).
- **Non-goals:** script reports, Server Scripts.

## M9 Integration

- **Scope:** `/api/v1/method` for registered endpoints, webhooks through the outbox, import/export, fixtures. Capability 6.
- **Exit criteria:** (to be set in the M9 plan) including authorization tests for every method endpoint.
- **Non-goals:** GraphQL, RPC by dotted path.

## M10 Customization

- **Scope:** custom-field overlay, field-level permissions, user permissions. Capabilities 5 and 10.
- **Exit criteria:** (to be set in the M10 plan) extends the authorization test matrix; schema sync stays aware of the overlay.
- **Non-goals:** a Customize Form UI before a real need.

## Later

Each item needs its own plan and approval: realtime (on demand), sharing, file attachments.
