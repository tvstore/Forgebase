# Maintenance policy

Status: **Proposed** (awaiting owner acceptance). Workflow, approval gates, git safety and the model selection policy are defined only in [AGENTS.md](../../AGENTS.md); they are not repeated here. Guiding rule: prefer deleting code over adding configuration options.

## Toolchain and MSRV

Status: **Accepted** (owner decision, 2026-10-28).

- The toolchain is pinned to 1.99.1 in `rust-toolchain.toml`.
- MSRV = pinned stable minus 2 minor releases (currently 1.97). It is declared as the workspace `rust-version` and checked in CI. MSRV rises together with pin bumps and is noted in `CHANGELOG.md`. The dependency set sets a floor (measured: about 1.89 for recommended stack A, 1.94 if sqlx is chosen; see [dependency-evaluation](dependency-evaluation.md#11-supply-chain-tooling-and-msrv)); the window must stay above it.
- Edition 2024 is confirmed. `Cargo.lock` is committed (the workspace ships binaries).

## Dependency admission

Adding a dependency requires a short justification in the PR:

- need, and alternatives considered;
- license (must pass cargo-deny);
- maintenance status;
- transitive weight.

Significant dependencies get an ADR ([docs/adr](../adr/README.md)). The same rules apply to frontend (npm) dependencies, with a committed lockfile and audit/license checks.

## cargo-deny

Runs in CI (and advisories on a daily schedule). Covers licenses, advisories, bans (duplicates where practical) and sources. Proposed license allow-list (to be reviewed): MIT, Apache-2.0, Apache-2.0 WITH LLVM-exception, BSD-2-Clause, BSD-3-Clause, ISC, Zlib, Unicode-3.0, Unlicense, BSL-1.0, CC0-1.0. Copyleft is denied by default; a per-crate `clarify` entry for `ring` is added if needed. cargo-deny and cargo-audit are CI tools, not workspace dependencies. CI installs cargo-deny with `cargo install --locked` (no third-party action).

## Versioning and changelog

SemVer once published; 0.x until the API stabilizes. `CHANGELOG.md` in Keep a Changelog format.

## Testing strategy

| Area | Approach |
|---|---|
| `forgebase-core` | Unit tests (pure), table-driven; property tests only where cheap value (for example identifier validation). |
| `forgebase-db`, `forgebase-http` | Integration tests against a real PostgreSQL (no mocks); each test in its own transaction or fresh database; CI uses a Postgres service container. Supported Postgres major pinned (for example 15 or later; exact floor Undecided, decided when the related milestone is planned). |
| Security | Explicit negative tests for every permission rule and for injection attempts in filters and identifiers ([security-model](security-model.md#required-negative-tests)). |
| Frontend (from M4) | Component tests with a light runner (Vitest) plus a few end-to-end smoke tests; tooling decided in M4. |
| Coverage | No percentage targets; risk-based coverage. |

## Documentation and ADR rules

- Docs are updated in the same PR as behavior changes.
- ADRs are immutable once Accepted: they are superseded, not edited. See [docs/adr/README.md](../adr/README.md).
- Do not duplicate content across documents; link instead.

## Review expectations

Every change gets an independent review. The Stage 3 checklist is defined only in [AGENTS.md](../../AGENTS.md#stage-3--agent-review). Passing CI alone is not a review.
