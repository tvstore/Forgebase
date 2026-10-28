# Changelog

All notable changes to this project are documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/), and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html) once published.

## [Unreleased]

### Added

- Workspace skeleton (4 crates), CI, toolchain pin, cargo-deny config, contribution docs.
- Toolchain pinned to 1.99.1; MSRV (`rust-version`) 1.97.
- `forgebase-core`: DocType metadata model loaded from TOML with collected validation errors, identifier rules, dynamic `Value` model and document validation (full and partial update). New dependencies: `serde`, `toml`, `thiserror`, `rust_decimal`, `time`.
- Docs: M1 DB-layer spike result (ADR 0004 direction confirmed), owner decisions of 2026-10-28 (TOML, UUID v7, PostgreSQL floor 16, `time`, `rust_decimal`).
