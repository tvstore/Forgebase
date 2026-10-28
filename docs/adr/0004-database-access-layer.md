# 0004 Database access layer

- Status: Proposed
- Date: 2026-10-28

## Context

DocType tables and columns exist only at runtime, so compile-time-schema tools (diesel `table!`, sea-orm entities, sqlx `query!` macros) fit only Forgebase's own fixed tables. Document CRUD must build SQL from metadata with validated, quoted identifiers and bound parameters. Evidence and weights: [dependency-evaluation](../blueprint/dependency-evaluation.md#4-database-access).

## Decision

Proposed: `tokio-postgres` + `deadpool-postgres` with a small in-house SQL/DDL/CRUD layer and a custom embedded migration runner (advisory lock, checksums). Measured weight is 74 packages with pooling and MSRV 1.85. sqlx (macros off) is the close alternative. **The final choice is made after the M1 spike** that prototypes one DocType CRUD path with both; this Proposed ADR is then revised with the spike result and submitted for owner acceptance.

## Alternatives considered

- sqlx without macros: built-in pool and `migrate!`, but 132 packages, MSRV 1.94, and a governance transition to the `transact-rs` organization.
- diesel / diesel-async: static schema model, off the main path for runtime DocTypes.
- sea-orm: entity-first, heaviest (152 packages), wrong abstraction.
- sea-query as a builder only (1 extra package): optional later behind an internal trait.

## Consequences

- We own and must test the SQL layer, especially identifier validation (see [security-model](../blueprint/security-model.md#injection-defense)).
- No compile-time query checking; integration tests against real PostgreSQL carry that load.
- Pool and migrations are separate concerns, each replaceable.
