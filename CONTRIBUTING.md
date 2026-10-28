# Contributing

Workflow, approval gates, git safety and review rules are defined only in [AGENTS.md](AGENTS.md). Read it first.

## Prerequisites

- [rustup](https://rustup.rs/). The toolchain (1.99.1, with rustfmt and clippy) is selected automatically from `rust-toolchain.toml`.
- For the MSRV check: `rustup toolchain install 1.97 --profile minimal`.
- For the dependency check: `cargo install --locked cargo-deny --version 0.20.2`.

## Local checks

Run the same commands as CI ([.github/workflows/ci.yml](.github/workflows/ci.yml)) before opening a PR:

```sh
cargo fmt --all --check
cargo clippy --workspace --all-targets --locked -- -D warnings
cargo test --workspace --locked
cargo build --workspace --locked
cargo +1.97 check --workspace --all-targets --locked   # MSRV
cargo deny --locked check                              # needs network for advisories
```

## Dependencies

Adding a dependency needs the justification described in [maintenance-policy](docs/blueprint/maintenance-policy.md#dependency-admission). It must pass cargo-deny.

## Commits and PRs

- Commit messages and technical docs are in English; keep commits small and focused.
- Do not push to `main` directly; open a PR.
- Update docs and `CHANGELOG.md` in the same PR as the behavior change.
