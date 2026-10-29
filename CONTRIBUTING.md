# Contributing

Workflow, approval gates, git safety and review rules are defined only in [AGENTS.md](AGENTS.md). Read it first.

## Prerequisites

- [rustup](https://rustup.rs/). The toolchain (1.99.1, with rustfmt and clippy) is selected automatically from `rust-toolchain.toml`.
- For the MSRV check: `rustup toolchain install 1.97 --profile minimal`.
- For the dependency check: `cargo install --locked cargo-deny --version 0.20.2`.
- For the desk (`desk/`): Node.js `^20.19.0 || >=22.12.0` (Vite 8) with npm. `desk/.npmrc` sets `ignore-scripts=true`, so `npm ci` runs no install scripts of dependencies.

## Local checks

Run the same commands as CI ([.github/workflows/ci.yml](.github/workflows/ci.yml)) before opening a PR:

```sh
cargo fmt --all --check
cargo clippy --workspace --all-targets --locked -- -D warnings
cargo test --workspace --locked                        # needs FORGEBASE_TEST_DATABASE_URL, see below
cargo build --workspace --locked
cargo +1.97 check --workspace --all-targets --locked   # MSRV
cargo deny --locked check                              # needs network for advisories
```

The desk has its own checks (the `desk` job of CI). Run them in `desk/` when you change it:

```sh
cd desk
npm ci
npm run lint
npm run typecheck
npm test
npm run build       # writes desk/dist
npm run licenses    # license allow-list over package-lock.json
npm audit --audit-level=high
```

## PostgreSQL for tests

The `forgebase-db` and `forgebase-cli` integration tests need a PostgreSQL 16 server and a superuser (or `CREATEDB`) URL in `FORGEBASE_TEST_DATABASE_URL`. They fail with a clear message when it is not set; they never skip. Each test creates its own `fb_test_*` database and drops it at the end. The `forgebase-cli` tests run the built `forgebase` binary against such a database (they set `FORGEBASE_DATABASE_URL` for it themselves).

```sh
docker run -d --name forgebase-pg -p 5432:5432 -e POSTGRES_USER=forgebase -e POSTGRES_PASSWORD=forgebase postgres:16
# or install PostgreSQL 16 from your system package manager and create a superuser role
export FORGEBASE_TEST_DATABASE_URL=postgres://forgebase:forgebase@localhost:5432/postgres
```

CI runs the same tests against a `postgres:16` service container.

## Dependencies

Adding a dependency needs the justification described in [maintenance-policy](docs/blueprint/maintenance-policy.md#dependency-admission). It must pass cargo-deny. An npm dependency of the desk is pinned to an exact version, must pass `npm run licenses` (see [the frontend rules](docs/blueprint/maintenance-policy.md#frontend-npm-dependencies)) and, if it is a runtime dependency, needs an ADR.

## Commits and PRs

Keep commits small and focused. Update docs and `CHANGELOG.md` in the same PR as the behavior change.
