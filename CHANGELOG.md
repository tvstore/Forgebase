# Changelog

All notable changes to this project are documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/), and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html) once published.

## [Unreleased]

### Added

- Workspace skeleton (4 crates), CI, toolchain pin, cargo-deny config, contribution docs.
- Toolchain pinned to 1.99.1; MSRV (`rust-version`) 1.97.
- `forgebase-core`: DocType metadata model loaded from TOML with collected validation errors, identifier rules, dynamic `Value` model and document validation (full and partial update). New dependencies: `serde`, `toml`, `thiserror`, `rust_decimal`, `time`.
- Docs: M1 DB-layer spike result (ADR 0004 direction confirmed), owner decisions of 2026-10-28 (TOML, UUID v7, PostgreSQL floor 16, `time`, `rust_decimal`).
- `forgebase-db` (M1.2): connection pool (no TLS yet), embedded framework migrations (`fb_migration`, `fb_doctype`) with checksums and an advisory lock, and additive DocType schema sync with dry-run, conflict refusal and `lock_timeout`. Constraint and index names are chosen by PostgreSQL. Integration tests run against PostgreSQL 16 (`FORGEBASE_TEST_DATABASE_URL`; CI service container). New dependencies: `tokio-postgres`, `deadpool-postgres`; dev: `tokio`.
- Docs: owner decisions of 2026-10-28 on Int/Data/required/Decimal marked Accepted; M1.2 behavior recorded in architecture section 5 and the security model.
- `forgebase-db` (M1.3): document `insert`, `get`, `update` and `delete` on a pooled client or a caller-owned transaction. Names follow the DocType naming rule (random = the UUID v7, or a field value), `id` is a UUID v7 generated in the application, timestamps come from the database `now()`, `version` starts at 1 and counts updates, `owner` / `modified_by` record the actor. Values are bound parameters with explicit casts; unique violations, validation errors, missing documents and invalid actors have their own `DbError` variants. No permissions, concurrency check or renames yet. New dependencies: `uuid` (v7), `bytes`; `tokio-postgres` features `with-uuid-1` and `with-time-0_3`; `rust_decimal` feature `db-tokio-postgres` (in `forgebase-db` only).
- `forgebase-cli` (M1.3): the `forgebase` binary with `migrate [--dry-run]` and `doc insert|get|update|delete`, configured by `FORGEBASE_DATABASE_URL`, JSON in and out (Decimal as a string), exit code 0 / 1 / 2. New dependencies: `clap` (derive), `serde_json`, `tokio`; `time` features `parsing` and `formatting` (CLI only).
- `examples/doctypes/customer.toml` and a README quick start.
- Docs: M1 marked delivered (Done when M1.3 is merged); document CRUD rules in the architecture, CLI trust model in the security model, admission notes for `uuid`, `bytes`, `serde_json` and `clap`.
