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

Significant dependencies get an ADR ([docs/adr](../adr/README.md)). The same rules apply to frontend (npm) dependencies, as made concrete below.

### Frontend (npm) dependencies

Status: **Accepted** (owner decisions of 2026-10-29, M4). Details and the package list: [dependency-evaluation, section 12](dependency-evaluation.md#12-frontend).

- Exact versions in `desk/package.json` (no `^` or `~`); `desk/package-lock.json` is committed and CI installs with `npm ci`.
- Runtime dependencies are `react` and `react-dom` only. No UI component library, router or data-fetching library without an ADR.
- **License allow-list**, checked by `npm run licenses` in CI: shipped packages MIT, ISC, Apache-2.0, BSD-2-Clause, BSD-3-Clause; development-only packages additionally MPL-2.0, BlueOak-1.0.0 and CC-BY-4.0. **MPL-2.0 is allowed only for packages marked `dev` in the lockfile** (build tooling that is not shipped). A missing, unknown or expression license fails the check; extending the list is an owner decision recorded here and in the dependency evaluation.
- `npm audit --audit-level=high` runs in CI (also on the daily schedule): high and critical advisories fail the job; low and moderate ones are reported and handled when convenient.
- The desk build must not need a network, a CDN or an inline script or style: the Content-Security-Policy of [the security model](security-model.md#the-http-layer-implemented-in-m33) forbids them.

## cargo-deny

Runs in CI (and advisories on a daily schedule). Covers licenses, advisories, bans (duplicates where practical) and sources. Proposed license allow-list (to be reviewed): MIT, Apache-2.0, Apache-2.0 WITH LLVM-exception, BSD-2-Clause, BSD-3-Clause, ISC, Zlib, Unicode-3.0, Unlicense, BSL-1.0, CC0-1.0. Copyleft is denied by default; a per-crate `clarify` entry for `ring` is added if needed. cargo-deny and cargo-audit are CI tools, not workspace dependencies. CI installs cargo-deny with `cargo install --locked` (no third-party action).

## Versioning and changelog

SemVer once published; 0.x until the API stabilizes. `CHANGELOG.md` in Keep a Changelog format.

## Testing strategy

| Area | Approach |
|---|---|
| `forgebase-core` | Unit tests (pure), table-driven; property tests only where cheap value (for example identifier validation). |
| `forgebase-db`, `forgebase-http` | Integration tests against a real PostgreSQL (no mocks); each test in its own transaction or fresh database; CI uses a Postgres service container. Supported PostgreSQL floor: 16 (Accepted, owner decision 2026-10-28). |
| Security | Explicit negative tests for every permission rule and for injection attempts in filters and identifiers ([security-model](security-model.md#required-negative-tests)). |
| Frontend (from M4) | Vitest on pure modules (router, API client, filter builder, list helpers, form field conversion, access rules, error mapping) with a stubbed `fetch`, ESLint rules that forbid browser storage, raw HTML and inline styles, `tsc --noEmit`, and one Playwright smoke test (`desk/e2e/smoke.spec.ts`, Chromium only, CI job `e2e`, implemented in M4.2). Component tests with `jsdom` only when a pure-module test cannot cover the risk. Decided in M4 (owner decision 2026-10-29). |
| Coverage | No percentage targets; risk-based coverage. |

## Documentation and ADR rules

- Docs are updated in the same PR as behavior changes.
- ADRs are immutable once Accepted: they are superseded, not edited. See [docs/adr/README.md](../adr/README.md).
- Do not duplicate content across documents; link instead.

## Review expectations

Every change gets an independent review. The Stage 3 checklist is defined only in [AGENTS.md](../../AGENTS.md#stage-3--agent-review). Passing CI alone is not a review.
