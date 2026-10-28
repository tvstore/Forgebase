# Milestones

All milestones are **Proposed**. There are no dates. **Each milestone needs its own Stage 1 plan and explicit owner approval before implementation starts** (see [AGENTS.md](../../AGENTS.md#3-mandatory-four-stage-workflow)); the scope below is the starting proposal for that plan, not an approval. Design context: [architecture](architecture.md); capability status: [feature matrix](feature-matrix.md).

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

- **Scope:** workspace skeleton with 4 empty crates; CI (fmt, `clippy -D warnings`, test, build, cargo-deny); toolchain pin; contribution docs.
- **Exit criteria:** green CI on the empty skeleton.
- **Non-goals:** any feature code; database access; frontend.

## M1 Metadata and storage

- **Scope:** DocType definition loading and validation; additive schema sync with dry-run; insert/get/update/delete of simple documents via library API and CLI; framework migrations. Includes the DB-layer spike (tokio-postgres vs sqlx) that is the required checkpoint for [ADR 0004](../adr/0004-database-access-layer.md); if the spike contradicts the accepted direction, a new ADR supersedes it. Also decides the metadata file format (TOML vs JSON) and confirms the surrogate-UUID primary key.
- **Exit criteria:** integration tests against a real PostgreSQL (CI service container).
- **Non-goals:** links, child tables, lifecycle hooks, HTTP API, authentication, permissions, jobs, multi-site.

## M2 Relationships and lifecycle

- **Scope:** Link foreign keys; child tables; the fixed lifecycle pipeline with hooks; docstatus and amend; optimistic concurrency; naming series. Specifies the exact pipeline order.
- **Exit criteria:** lifecycle-order tests and rollback tests.
- **Non-goals:** Dynamic Link, single DocTypes (still Undecided), workflow, permissions (M3).

## M3 Auth, permissions, REST API

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
