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
- M2.1, `forgebase-core`: Link fields (`type = "link"`, `target = "<doctype>"`; the value is the target document's name), `Registry::check_references` (every Link target must be in the registry; a self-link is allowed), `DefinitionError::UnknownLinkTarget` and `InvalidLinkTarget`, `ValidationError::LinkNotFound` and `ValidationError::field`.
- M2.1, `forgebase-db`: a Link column is `uuid` with an unnamed foreign key to the target's `id` (`ON DELETE RESTRICT`) and an index, created by schema sync in new phases (keys, then foreign keys, then indexes) so DocTypes that link to each other or to themselves sync in one run; new steps `AddForeignKeys` and `CreateIndex`; a missing or wrong foreign key (absent, other table, delete rule other than `RESTRICT`, or on a non-Link column) is a sync conflict (`Conflict::ForeignKeyMismatch`; a `NOT VALID` foreign key counts as wrong too), a missing index is an additive step; a Link to an unknown DocType is `DbError::InvalidMetadata`.
- M2.1, `forgebase-db`: document writes resolve Link names to ids (`FOR KEY SHARE`) and reads return names; `DbError::Conflict` (optimistic concurrency) and `DbError::LinkedDocumentExists` (a document that others link to cannot be deleted; the message names the linking DocType).
- M2.1, `forgebase-cli`: Link values are JSON strings; `doc update` and `doc delete` require `--version N`; the DocType loader reports Link targets that no file defines. `examples/doctypes/customer_group.toml` and a `customer_group` Link on the example customer.

### Changed

- **Breaking (0.x, accepted by the owner 2026-10-28):** `forgebase_db::update` and `forgebase_db::delete` take an `expected_version: i64` argument (after `name`) and fail with `DbError::Conflict` when the stored version differs; `forgebase doc update` and `doc delete` require `--version N`. Before this change the last writer won.
- `forgebase_core::meta::FieldType` has a new variant `Link { target }` and `forgebase_core::document::ValidationError` (`LinkNotFound`), `forgebase_core::meta::DefinitionError` (`UnknownLinkTarget`, `InvalidLinkTarget`), `forgebase_db::Step`, `Conflict` and `DbError` have new variants (the enums are not `non_exhaustive`: exhaustive matches in other code need new arms). `Registry::add` still checks one DocType only; call `Registry::check_references` after loading all DocTypes.
- Docs: all seven M2 owner decisions of 2026-10-28 recorded as Accepted; M2 marked In progress.
