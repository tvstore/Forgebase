# Architecture Decision Records

An ADR records one significant decision: the context, what was decided, what else was considered, and the consequences. ADRs are immutable once **Accepted**: to change a decision, write a new ADR that supersedes the old one. Process rules: [maintenance-policy](../blueprint/maintenance-policy.md#documentation-and-adr-rules). Design overview: [architecture](../blueprint/architecture.md).

## Template

```markdown
# NNNN Title

- Status: Proposed | Accepted | Accepted by owner | Superseded by NNNN | Rejected
- Date: YYYY-MM-DD

## Context
## Decision
## Alternatives considered
## Consequences
```

## Status values

| Status | Meaning |
|---|---|
| Proposed | Recommendation awaiting owner decision. |
| Accepted | Confirmed by the owner, or a process decision recorded as Accepted. |
| Accepted by owner | An Accepted decision that the owner explicitly made or provided. |
| Superseded by NNNN | Replaced by a later ADR. |
| Rejected | Considered and declined. |

## Index

| ADR | Title | Status |
|---|---|---|
| [0001](0001-record-architecture-decisions.md) | Record architecture decisions | Accepted |
| [0002](0002-modular-monolith-and-workspace-layout.md) | Modular monolith and workspace layout | Accepted |
| [0003](0003-postgresql-only.md) | PostgreSQL only | Accepted |
| [0004](0004-database-access-layer.md) | Database access layer | Accepted |
| [0005](0005-http-stack.md) | HTTP stack: axum on tokio | Accepted |
| [0006](0006-extension-model.md) | Extension model: compile-time apps | Accepted |
| [0007](0007-license-mit.md) | License: MIT | Accepted by owner |
| [0008](0008-model-selection-policy.md) | Model selection policy for the agent workflow | Accepted by owner |
| [0009](0009-lifecycle-pipeline-and-hooks.md) | Lifecycle pipeline and hooks | Accepted |
| [0010](0010-authentication-and-sessions.md) | Authentication and sessions | Accepted |
| [0011](0011-permission-model-phase-1.md) | Permission model, phase 1 | Accepted |
