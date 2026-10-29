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
- **Exit criteria:** integration tests against a real PostgreSQL (CI service container). Met: the `forgebase-db` and CLI (`forgebase-cli`, now `forgebase`) integration tests pass against PostgreSQL 16 (CI runs them with the same service container); a user can run `forgebase migrate` and insert, get, update and delete a document of an example DocType (README quick start).
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
  - `forgebase serve` end to end on an ephemeral port: login, cookie, list, Host refusal, dev mode and configuration errors (`forgebase/tests/cli.rs`, formerly `forgebase-cli/tests/cli.rs`);
  - docs updated (architecture sections 1, 5 and 7, security model, dependency evaluation, feature matrix, ADR 0005 and 0011, README, CHANGELOG).

  CI green and the independent review (AGENTS.md, Stage 3) are recorded per PR at merge (PR #11, PR #12 and the M3.3 PR).
- **Non-goals:** OAuth/SSO, 2FA, field-level permissions, user permissions, sharing, GraphQL, CORS, `/api/v1/method`, rate limiting beyond the login bound and lockout, TLS in the application, OpenAPI documents, HTTP/2.

## M4 Minimal Desk

- **Status:** Done once the M4.2 pull request is merged (the owner approved the M4 plan on 2026-10-29). Delivered as two PRs: **M4.1** the metadata API (`GET /api/v1/meta`, `/meta/{doctype}`), the CSRF-recovering `GET /api/v1/session`, static serving of the built desk (`FORGEBASE_DESK_DIR`, CSP), the `desk/` scaffold (Vite, TypeScript, Vitest, ESLint), the login page, the home page and the list view (paging, sorting, filters), the frontend CI job and license check (PR #15, merged); **M4.2** the form view (create, edit, delete, submit, cancel, amend), the M4.1 review follow-ups and one Playwright smoke test in CI (job `e2e`).
- **Scope:** login, list view and form view for any DocType, generated from metadata. The UI component library question is decided: none (owner decision 2026-10-29, [architecture, section 11](architecture.md#11-desk-reports-workflow)); frontend test tooling is Vitest, plus one Playwright smoke test.
- **Exit criteria:** a user can log in and list/create/edit a DocType document through the UI; an action without permission is hidden in the UI **and** refused by the API (tests); no secrets in browser storage beyond the HttpOnly session cookie; accessibility basics (labels, keyboard use of native controls); docs updated. Met, with this evidence:
  - **the smoke flow** (login, list, create, edit, submit a sales order with a child row and a Link, then a user without the submit right): `desk/e2e/smoke.spec.ts`, test `login, list, create, edit and submit a sales order; a clerk cannot submit`, run by the CI job `e2e` (green CI on the pull request head is the evidence at merge; a local run against a real server passed, see the PR);
  - **hidden in the UI and refused by the API:** the desk side is the pure functions `fieldReadOnly` and `lifecycleActions` (`desk/src/access.test.ts`: docstatus x rights x submittable x `allow_on_submit`, `own` rights) and the smoke test (the clerk sees neither Submit, Cancel, Amend nor Delete); the API side is the M3 authorization tests (`forgebase-db/tests/permissions.rs`, `forgebase-http/tests/resources.rs`: `the_authorization_matrix_over_http`, `the_lifecycle_and_its_error_codes`), and the smoke test sends one refused `submit` request as the clerk (403);
  - **no secrets in browser storage:** ESLint forbids `localStorage`, `sessionStorage`, `indexedDB`, `document.cookie`, `dangerouslySetInnerHTML` and inline `style` in `desk/src`; `desk/src/api.test.ts` shows the client never touches storage; the M4.1 Playwright check of the in-memory CSRF token after a reload (recorded in the M4.1 PR); and the smoke test asserts that `localStorage` and `sessionStorage` are empty, `document.cookie` is empty and the session cookie is `HttpOnly`, after login and after the form flow;
  - **accessibility basics:** a label for every control (cells of a table by column and row), `aria-required`, errors linked with `aria-describedby`, focus on the first invalid field or the alert, on the heading after navigation, in-page questions that focus their safe answer, keyboard operation of the grid and the Link picker (exercised in Chromium with Playwright while developing; the smoke test checks the focus after Add row, after a failed save and after create);
  - **docs updated:** architecture section 11, security model, dependency evaluation (Playwright), README, CONTRIBUTING, CHANGELOG, feature matrix;
  - **independent review (AGENTS.md, Stage 3):** PR #15 (M4.1) passed review and merged; the review of the M4.2 pull request is recorded in that pull request at merge (this text was written before it).
- **Non-goals:** reports, workflow UI, realtime, custom pages, theming, i18n, embedding the assets in the binary (M7), CORS, drag and drop in tables, rich text, file attachments, per-field permissions.
- **Known limits at the end of M4:** documents named `.` or `..` cannot be opened (a REST path cannot address them); `datetime` fields show the browser's local time and no fractions of a second; a blocked browser Back leaves a duplicate history entry; the smoke test runs one browser and one flow.

## M5 App model and developer experience

- **Status:** In progress (the owner approved M5 plan v2 on 2026-10-29; all eight owner decisions are Accepted, see [ADR 0012](../adr/0012-app-model-and-public-api-boundary.md)). Delivered as three PRs: **M5.1** the framework part (below, merged), **M5.2** `forgebase new-app` and the "first app" guide (delivered by its PR; M5 stays In progress until M5.3), **M5.3** the example apps (which touch no file under `crates/`: they prove the public API is enough).
- **Scope:** the app model and public API boundary (the `forgebase-cli` crate renamed `forgebase`, lib + bin; `App`, `Forgebase`, embedded DocTypes and migrations, app data migrations with `fb_app`, hooks across apps, startup errors, `forgebase::testing`) in M5.1; a `forgebase new-app` template and a "first app" guide in M5.2; small example business apps as separate crates (DocTypes, hooks, permissions) that validate the framework, not domain products, in M5.3.
- **M5.1 evidence:** tests against a real PostgreSQL 16 and without one: every startup error (`forgebase/tests/startup.rs`, `compose.rs` unit tests); embedded files resolved relative to the calling crate, the DocType-list guard, hook order across apps (an extending app's hook runs after and can reject), ownership in the metadata API, app migrations (once, in app order, checksum mismatch refused before anything else is applied, a failing migration rolled back and the run stopped, `fb_app`, dry run) (`forgebase/tests/apps.rs`); the migration lock held for the whole migrate (`forgebase-db/tests/app_migrations.rs`); a binary with apps end to end (`forgebase/tests/app_binary.rs`). All existing tests pass unchanged except where they count the framework migrations or list the keys of a metadata item.
- **M5.2 evidence:** `forgebase new-app` ([guide](../guide/first-app.md)): `forgebase/tests/new_app.rs` (name rules, an existing directory and a path that is not a checkout are refused, relative paths, the generated file set, every placeholder replaced, the sample DocType valid); the CI job `app-template` generates an app outside the checkout and runs its tests, migrates, serves, logs a user in and reads the sample DocType from the metadata API with `"app": "demo"`. M5.1 follow-ups: `testing` types evolvable, the dry-run fix of `migrate_apps` and its test, a timeout in the lock tests, `./` in `assert_doctypes_listed`.
- **Exit criteria:** (the first is met by M5.2; the example apps of M5.3 complete it) a developer follows the guide and gets a running app with tests, without touching framework crates (CI job `app-template`). Proves success criteria 1 and 5 of the [Project Goal](vision.md#project-goal); capabilities 9 and 10.
- **Non-goals:** runtime plugins ([ADR 0006](../adr/0006-extension-model.md)), domain modules beyond the small examples, uninstalling apps, publishing to crates.io, multi-site, custom fields on another app's DocType (M10), hooks with database access, Rust-code migrations.

## M6 Jobs and scheduler

- **Scope:** `SKIP LOCKED` queue, `forgebase worker`, scheduler with advisory-lock leader election, transactional outbox. Capability 8.
- **Exit criteria:** (to be set in the M6 plan) at minimum tests for at-least-once delivery, retry/backoff, crashed-worker lease recovery and a single active scheduler leader.
- **Non-goals:** Redis, a separate jobs service, realtime.

## M7 Site management

- **Scope:** site configuration file; the multi-site database decision (lean: database per site, see [architecture, section 9](architecture.md#9-multi-site-undecided)); per-site app install; backups. Capability 1.
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
